# MCP Server

`codegraph serve` exposes a loaded graph to an AI assistant over the Model
Context Protocol. It speaks JSON-RPC 2.0 directly (no external MCP runtime
dependency). The server is read-only over the graph; the PR and working-changes
tools shell out to `gh`/`git` to read state but never write.

The graph is loaded once at startup from a `graph.json` and hot-reloads when that
file changes on disk (see [Incremental-Updates](Incremental-Updates)). Every node
label, relation, and file path is sanitized before it reaches tool output (a
security boundary on names derived from source). `get_source` is the one tool
that returns raw file contents; it reads only files inside a configured source
root (a path-traversal jail), and the bytes it returns are the verbatim source
the agent asked for.

## Protocol version

The `initialize` reply negotiates the protocol version: if the client requests
one the server supports (`2025-11-25`, `2025-06-18`, `2025-03-26`, or
`2024-11-05`) the server echoes it back, otherwise it returns its latest,
`2025-11-25`. A client that sends no `protocolVersion` gets `2025-11-25`. Server
info is `{ "name": "codegraph", "version": <crate version>, "description": <one-line summary> }`.

The `initialize` reply also advertises capabilities:

```json
{
  "tools": {},
  "resources": { "subscribe": true },
  "prompts": {},
  "completions": {},
  "logging": {}
}
```

and carries a server-level `instructions` string that orients an assistant to
the whole toolset (the recommended flow, and what "god node", "community", and
edge confidence mean).

## Running the server

```
codegraph serve [--graph <path>] [--http <addr>] [--api-key <key>] [--source-root <dir>] [--allow-exec]
```

- `--graph <path>` selects the `graph.json` to load. Default is the standard
  output location (`codegraph-out/graph.json`). If the file is missing, serve
  exits with an error pointing you to run `codegraph extract` first.
- No `--http`: serve over **stdio** (the default transport).
- `--http <addr>`: serve over **HTTP** at `host:port` (for example
  `127.0.0.1:8765`).
- `--api-key <key>`: require this key on HTTP requests. May also be set via the
  `CODEGRAPH_API_KEY` environment variable (the flag takes precedence).
- `--source-root <dir>`: the trusted root for resolving a node's source file in
  `get_source`. Default is the directory above `codegraph-out/` (the repo root);
  it falls back to the current directory when that cannot be derived.
- `--allow-exec`: expose the command-running `speculate` tool (off by default).
  This makes the server **no longer read-only** — `speculate` executes the
  project's test/build commands in a throwaway worktree — so enable it only for
  trusted clients. Without it the tool is neither advertised nor runnable.

### stdio transport

```
codegraph serve
```

Newline-delimited JSON-RPC 2.0 on stdin/stdout: one request per line, one
response line per request. Notifications (requests with no `id`) get no reply.
Blank lines and unparseable lines are ignored. A status line is printed to
stderr (`[codegraph] MCP server ready on stdio`) so it never pollutes the
JSON-RPC stream on stdout.

This is the mode an assistant launches as a subprocess. See
[Assistant-Integration](Assistant-Integration) for wiring it into a host.

### Registering with Codex

`codegraph install codex` wires this stdio server into Codex automatically: a
`[mcp_servers.codegraph]` entry in the project `.codex/config.toml` (Codex CLI),
or a per-repo `[mcp_servers.codegraph-<repo>]` in the global `~/.codex/config.toml`
with `codegraph install codex --global` (Codex desktop app, which only reads the
global config). `codegraph` must be on your `PATH`. See
[Assistant-Integration](Assistant-Integration#codex).

### HTTP transport

```
codegraph serve --http 127.0.0.1:8765 --api-key s3cret
```

Streamable-HTTP on the `/mcp` route:

- `POST /mcp` -- one JSON-RPC request, returns its JSON response. A notification
  returns HTTP 202 with no body. An invalid JSON body returns 400.
- `GET /mcp` (with `Accept: text/event-stream`) -- opens the server-to-client SSE
  stream. It emits keep-alive heartbeats, and, for a subscribed session, pushes a
  `notifications/resources/updated` event when the graph hot-reloads (see
  [Resource subscriptions](#resource-subscriptions)). The stream is bounded (it
  ends when the session is reaped or after a hard cap near the idle timeout).
- `DELETE /mcp` -- terminates a session (204 if it existed, 404 if unknown, 400
  if no session id header).

A startup line is printed to stderr: `[codegraph] MCP server on
http://<addr>/mcp`.

#### Sessions

Stateful by default. An `initialize` POST mints an opaque 128-bit session id,
returned in the `Mcp-Session-Id` response header. Later requests should carry it
in the `Mcp-Session-Id` request header:

- Unknown or expired id on a non-initialize request: 404 (the client should
  re-initialize).
- A missing id on a non-initialize request is tolerated, so simple
  request/response clients keep working.

A background reaper drops sessions idle longer than 1 hour (3600s).

#### Authentication

If `--api-key` (or `CODEGRAPH_API_KEY`) is set and non-blank, every request to
both `/mcp` and `/api/*` must supply it. A blank/absent key disables auth. The
key may be supplied as either:

- `X-API-Key: <key>`, or
- `Authorization: Bearer <key>` (the `Bearer` scheme is case-insensitive).

A missing or wrong key returns 401. The comparison is constant-time.

If you serve on a wildcard address (`0.0.0.0` / `::`) with no API key, serve
prints a warning to stderr.

#### Host allowlist (DNS-rebinding protection)

When bound to a specific or loopback address, only requests whose `Host` header
is `localhost`, `127.0.0.1`, `[::1]`, or the bound IP (each with or without the
port) are accepted; others get 403 (`forbidden host`). Binding to a wildcard
address (`0.0.0.0` / `::`) disables this check, treated as an intentional public
exposure.

## MCP tools

`tools/list` reports 26 tools by default (27 with `--allow-exec`, which adds
`speculate`). Every tool documents its parameters in its input schema, and every
tool carries annotations so a host knows how safe it is to run:

```json
"annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": <bool> }
```

All 26 default tools are `readOnlyHint: true`. `openWorldHint` is `true` only for
the tools that reach outside the graph by shelling out (`list_prs`,
`get_pr_impact`, `triage_prs`, `working_changes_impact`, `predict_impact`,
`affected_tests`, and `time_travel_diff`); it is `false` for the rest, including
`predict_edit` (a pure in-memory graph query). `plan_rename` is plan-only and
never edits source.

The lone exception is the opt-in **`speculate`** tool, present only when the
server is started with `--allow-exec`. It runs the project's test/build commands
in a throwaway worktree (the empirical counterpart to `predict_impact`), so it is
annotated honestly as `readOnlyHint: false, openWorldHint: true`. The default
server never advertises or runs it, preserving the strictly read-only surface.

Each tool returns a text content block (the load-bearing, purpose-formatted
output). Six tools additionally declare an `outputSchema` and return a typed
`structuredContent` object alongside the text (a 2025-06-18 feature) -- see
[Structured output](#structured-output).

### query_graph

Primary entry point. Retrieve a relevant subgraph for a natural-language
question.

Parameters:
- `question` (string, required) -- the natural-language question.
- `mode` (string, `"bfs"` or `"dfs"`) -- traversal mode. Default `bfs`.
- `token_budget` (integer) -- approximate output token budget. Default 2000. It
  maps to a node cap (about `budget/40`, clamped to 10..400). The rendered text
  is bounded to about `token_budget` tokens: output within `token_budget*4` bytes
  is returned as-is (a fast path), and only larger output is truncated, using the
  `cl100k_base` tokenizer for an exact cut.
- `context_filter` (array of strings) -- keep only nodes whose source file path
  contains one of these substrings.

Returns a header (`Traversal: <mode> | Start: [<seeds>] | <n> nodes found`)
followed by `NODE` lines (label, file type, source file) and `EDGE` lines
(`source --relation--> target`), plus a `structuredContent` mirror (see below).
When the `CODEGRAPH_QUERY_LOG` environment variable points to a path, each query
is appended to it as JSONL (disable with `CODEGRAPH_QUERY_LOG_DISABLE=1`).

### get_node

Show a node's metadata and degree.

Parameters:
- `label` (string, required) -- a label/keyword that resolves to a node.

Returns: node label, id, source file, type, community, and degree, plus kind,
visibility, and LOC when the node carries them (added by the enrichment pass). If
nothing resolves, returns `No node matches '<label>'.`

### get_source

Return the actual source lines for a symbol, so an assistant can read a function
or class body without opening the file itself.

Parameters:
- `label` (string, required) -- resolves to a node.
- `context_lines` (integer) -- how many lines to return from the symbol's start.
  Default 40, clamped to 1..400.

Resolves the node, reads its file under the `--source-root` jail, and returns a
header (`<label> [<type>] <source_file>:L<from>-L<to>`) followed by the numbered
lines. The graph records a start line only, so the window is `context_lines`
lines from there. Returns `Source not available for <label> (<file>).` when there
is no readable source root, the file is missing, or it escapes the jail.

### get_neighbors

List a node's neighbours, optionally filtered by relation.

Parameters:
- `label` (string, required).
- `relation_filter` (string) -- case-insensitive substring; keep only neighbours
  whose relation contains it.

Returns one line per neighbour with a direction marker and the relation in
brackets.

### get_community

List the members of a community, one page at a time.

Parameters:
- `community_id` (integer, required).
- `offset` (integer) -- members to skip before the page. Default 0.
- `limit` (integer) -- maximum members in this page. Default 100.

Returns `Community <id> (showing <k> of <total>):` and each member on the page
(label and source file). Unknown or empty community returns `No community <id>.`

### god_nodes

The most-connected nodes (highest degree).

Parameters:
- `top_n` (integer) -- how many to return. Default 10.
- `offset` (integer) -- hubs to skip before the page (absolute rank is preserved
  in the numbering). Default 0.

Returns a ranked list of label and edge count, plus a `structuredContent` mirror.

### graph_stats

Node, edge, and community counts plus the edge-confidence breakdown.

Parameters: none.

Returns `Graph: <n> nodes, <n> edges, <n> communities` and
`Edges: <n> EXTRACTED, <n> INFERRED, <n> AMBIGUOUS`, plus a `structuredContent`
mirror.

### list_repos

Federated workspace members (repo tags) with node/edge counts. Edges are counted
under their source node's repo. Empty (single-repo) graphs return `No federated
repos (single-repo graph).` See [Workspaces-and-Federation](Workspaces-and-Federation).

Parameters: none.

### repo_stats

Node and edge counts for one federated member.

Parameters:
- `repo` (string, required) -- the repo tag.

Returns `Repo <repo>: <n> nodes, <n> edges`, or `No nodes for repo <repo>.`

### shortest_path

Shortest path between two keyword-resolved nodes.

Parameters:
- `source` (string, required).
- `target` (string, required).
- `max_hops` (integer) -- hop limit. Default 8.

Returns the path as `label -> label -> ...` with the hop count. Reports when the
endpoints do not resolve, resolve to the same node, exceed `max_hops`, or have no
path.

### affected

Reverse-impact: the nodes that transitively depend on a symbol (the blast radius
of changing it), by walking impact edges backward.

Parameters:
- `label` (string, required) -- the symbol to start from.
- `depth` (integer) -- maximum hops to walk backward. Default 3, clamped to
  1..16.
- `relations` (array of strings) -- edge relations to follow. Defaults to the
  structural-impact set (`calls`, `references`, `imports`, `imports_from`,
  `re_exports`, `inherits`, `extends`, `implements`, `uses`, `mixes_in`,
  `embeds`, `depends_on`, `reads_from`) **plus the cross-language relations**
  `invokes`, `binds_native`, `calls_service`, and `handled_by`, so the blast
  radius spans subprocess, FFI, and HTTP/gRPC boundaries (see
  [Cross-Language-Edges](Cross-Language-Edges)).

Returns `<n> nodes depend on <seed> (<= <depth> hops):` and, per hit, the hop
count, the relation it was reached through, and the label, plus a
`structuredContent` mirror. This is the MCP form of the CLI `affected` command.

### find_callers

The nodes that call, use, or reference this symbol (incoming call-like edges
only). Answers "who calls X".

Parameters:
- `label` (string, required).

### find_callees

The nodes this symbol calls, uses, or references (outgoing call-like edges only).
Answers "what does X call".

Parameters:
- `label` (string, required).

### list_prs

Open PRs with CI/review state targeting the base branch. Requires the `gh` CLI.
See [PR-Dashboard](PR-Dashboard).

Parameters:
- `base` (string) -- base branch; defaults to the repo's detected default branch.
- `repo` (string) -- target repo `owner/name`; defaults to the current
  directory's repo.

### get_pr_impact

One PR's detail plus its graph blast radius (communities and nodes touched).
Requires `gh`.

Parameters:
- `pr_number` (integer, required).
- `repo` (string).

### triage_prs

Actionable PRs ranked by status with blast radius, returned as structured data
with an instruction for the calling model to prioritize (the MCP host is itself
the LLM, so no LLM call is made here, unlike the CLI `prs --triage`). Requires
`gh`. Fetches each PR's changed files concurrently.

Parameters:
- `base` (string).
- `repo` (string).

### working_changes_impact

Graph blast radius of your branch's changes against a base branch (`git diff
<base>`, which covers committed plus uncommitted changes, the same set a PR would
have): which graph nodes and communities they touch, before opening a PR. Uses
`git` only, so it works offline and before any PR exists; no `gh` required.

Parameters:
- `base` (string) -- base branch to diff against; defaults to the detected
  default branch.

Returns `Working changes vs <base>: <n> files, <n> graph nodes, <n> communities
touched` and the changed files, or `No changes vs <base> (or git unavailable).`

### predict_impact

Forecast the consequences of a change before editing: which graph nodes the
changed files define, the reverse-impact blast radius that depends on them, which
edited symbols are public API (callers outside the file or module may break), and
a verify checklist. Pure-graph and read-only; for new-cycle / removed-API
detection use `time_travel_diff` or the `codegraph predict` CLI (those build
worktrees). `openWorldHint: true` (shells out to `git diff` when `files` is
omitted).

Parameters:
- `files` (array of strings) -- repo-relative changed files to forecast. Omit to
  use the working-tree diff against `base`.
- `base` (string) -- base branch to diff against when `files` is omitted; defaults
  to the detected default branch.
- `depth` (integer) -- reverse-impact hop bound. Default 3, max 16.

Returns the forecast summary, a heuristic change-risk score (low/medium/high
with its drivers), the changed nodes, the public APIs at risk, the tests at risk,
the blast radius (one dependent per line: hop count, relation, label, file), and
the verify checklist.

### affected_tests

Predictive test selection: the tests that exercise the code you are about to
change. Walks the reverse-impact set from the changed files and keeps the test
nodes (detected by path convention: a `test`/`tests`/`__tests__`/`testing`
directory, or a `test_*` / `*_test` / `*_spec` / `*.test.*` / `*.spec.*` /
`*Test` / `conftest` filename; a `spec/` directory alone is not treated as a test
tree). The focused
"which tests should I run for this change" view. `openWorldHint: true` (shells out
to `git diff` when `files` is omitted).

Parameters:
- `files` (array of strings) -- repo-relative changed files. Omit to use the
  working-tree diff against `base`.
- `base` (string) -- base branch to diff against when `files` is omitted; defaults
  to the detected default branch.
- `depth` (integer) -- reverse-impact hop bound. Default 3, max 16.

Returns one test per line (hop count, relation, label, file), or a note when no
test in the graph reaches the change. Inline unit tests not under a test path
(e.g. Rust `#[cfg(test)]`) are not detected.

### predict_edit

What breaks if you make a specific kind of edit to a symbol, classified into
"will break" and "to review". Complements `plan_rename` (which is rename-only).
Pure in-memory graph query; `openWorldHint: false`.

Parameters:
- `symbol` (string, required) -- the symbol to edit (name, bare name, or node id).
- `kind` (string, required) -- `delete` (every dependent breaks), `signature`
  (callers and type users break; bare imports go to review), or `visibility`
  (references from other files break when narrowing to private).
- `depth` (integer) -- reverse-impact hop bound. Default 3, max 16.

Returns the summary plus the "will break" and "review" dependents (each with hop
count, relation, label, file, and the reason), or a note if the symbol or kind is
not recognized.

### structural_search

Structural search over the graph with CGQL (a small Cypher-inspired query
language), or a named architectural pattern. Not text search: it matches on
kind/visibility/loc/fan-in/out/etc. `.name` is the bare symbol (no parentheses);
use `=~` for a regex/substring match.

Parameters:
- `query` (string) -- a CGQL query, e.g. `MATCH (c:class) WHERE c.loc > 500 RETURN c`.
  Omit when using `pattern`.
- `pattern` (string) -- a built-in pattern name instead of a query: `singleton`,
  `factory`, `observer`, `service-locator`, `god-class`.
- `limit` (integer) -- max rows to return. Default 50.

Returns the matched rows (one node per line: label, kind/visibility, source
location), or the parse error if the query is malformed. It also returns a
`structuredContent` mirror: one object per matched node with its `id`, `label`,
`kind`, `visibility`, `file`, `line`, `loc`, and captured **signature** (params
with optional types, return type, and the raw header), so an agent can route on
a function's shape without reading source. Aggregate queries (`count(...)`,
projections) return scalar `groups` instead.

### describe_node

A compact "takes X, returns Y, calls Z" description of a symbol, composed from
its captured [signature](Extraction#node-metadata-kind-visibility-span-signature)
and outgoing call edges. Graph-only (no source read), built for generating
tool/function descriptions or quickly grasping a function's shape. The "calls"
clause includes cross-language `invokes` and `calls_service` targets.

Parameters:
- `label` (string, required) -- the symbol to describe (bare name, full label,
  node id, or source file).

Returns the one-line summary, then a `Signature:` line and a `Calls (<n>): ...`
line when present, plus a `structuredContent` mirror
(`{ found, id, label, kind, summary, callees, signature }`). Returns
`No node matches '<label>'.` when nothing resolves.

### time_travel_diff

How the code graph changed between two git revisions: added/removed module
dependencies, removed APIs, architectural drift, new dependency cycles, and
change hotspots. Builds each revision in a throwaway git worktree (slow on a cold
repo; cached per commit SHA afterwards). `openWorldHint: true`.

Parameters:
- `rev1` (string, required) -- the base revision (e.g. `HEAD~10`, a branch, a SHA).
- `rev2` (string) -- the target revision. Defaults to the current working tree.
- `top` (integer) -- max rows per ranked section. Default 20.

Returns a summary (`<n> new nodes, <n> new edges, ...`), the added/removed
dependencies, removed APIs, drift, new cycles, and hotspots.

### plan_rename

Plan-only: a confidence-scored rename plan (edit sites, blast radius, collision
check) for an agent to apply. Never edits source. After applying the edits, run
`codegraph refactor verify` on the CLI to check the post-edit graph.

Parameters:
- `name` (string, required) -- the symbol to rename (its name, or a node id).
- `to` (string, required) -- the new name.
- `id` (string) -- disambiguate by node id when the name matches several definitions.
- `file` (string) -- disambiguate by a file-path substring.

Returns `Rename <old> -> <new> [<confidence>], <n> edit(s) across <n> file(s), <n>
to review, <n> affected`, or an error string if the symbol is not found.

### audit_sql

Audit the codebase's SQL for performance and security problems over the
SQL-aware graph: row-level-security gaps, over-broad grants, likely SQL
injection, missing indexes on filter/foreign-key columns, `SELECT *`,
non-sargable predicates, N+1 patterns, and missing primary keys.

Parameters:
- `severity` (string) -- only return findings at least this severe
  (`critical`|`high`|`medium`|`low`|`info`).

Returns a one-line summary plus, in `structuredContent`, the full `AuditReport`.
See [SQL Auditing](SQL-Auditing).

### advise_sql

Critique a single candidate query before it is written. Runs the same
performance + security checks on the query text and cross-references the graph:
whether the referenced tables exist, are behind row-level security, and have
indexes on the columns you filter on.

Parameters:
- `query` (string, required) -- the SQL to critique.
- `dialect` (string) -- optional dialect hint (`postgres`|`mysql`|`mssql`|`sqlite`).

Returns the findings as text + `structuredContent`.

### Structured output

Eight tools declare an `outputSchema` and return a `structuredContent` object
beside the text content, so a client can parse the result instead of scraping the
formatted text:

| Tool | `structuredContent` shape |
|---|---|
| `graph_stats` | `{ nodes, edges, communities, extracted, inferred, ambiguous }` |
| `god_nodes` | `{ god_nodes: [{ label, degree, id }] }` |
| `affected` | `{ seed, affected: [{ label, depth, via_relation }] }` |
| `query_graph` | `{ nodes: [{ label, file_type, source_file }], edges: [{ source, relation, target }] }` |
| `structural_search` | `{ columns, results: [[{ id, label, kind, visibility, file, line, loc, signature }]] }` (or `groups` for aggregates) |
| `describe_node` | `{ found, id, label, kind, summary, callees, signature }` |
| `audit_sql` / `advise_sql` | `{ version, summary, findings: [{ rule_id, severity, category, title, detail, location, remediation, confidence }] }` |

The other tools return text only.

### Tool error behavior

An unknown tool name is returned as a tool result with `isError: true` and a text
body `Unknown tool: <name>` (not a JSON-RPC protocol error). An unknown JSON-RPC
method returns error code `-32601`. An unknown resource or prompt returns
`-32602`.

## MCP resources

`resources/list` reports six read-only resources, each fetched with
`resources/read` by `uri`:

- `codegraph://report` (text/markdown) -- the `GRAPH_REPORT.md` next to the
  loaded graph, if present.
- `codegraph://stats` (text/plain) -- the same content as `graph_stats`.
- `codegraph://god-nodes` (text/plain) -- the top 10 god nodes.
- `codegraph://surprises` (text/plain) -- surprising cross-community connections.
- `codegraph://audit` (text/plain) -- the edge-confidence breakdown.
- `codegraph://questions` (text/plain) -- suggested questions to ask the graph.

See [Analysis-and-Reports](Analysis-and-Reports) and
[Semantic-Analysis](Semantic-Analysis) for what these surface.

### Resource templates

`resources/templates/list` reports two templates, so any node or community is
addressable as a resource by URI:

- `codegraph://node/{label}` (text/plain) -- metadata for one node by label, id,
  or bare name (the same content as `get_node`).
- `codegraph://community/{id}` (text/plain) -- members of one community by id.

`resources/read` resolves these templated URIs directly (for example
`codegraph://node/AuthService`).

### Resource subscriptions

The server advertises `resources.subscribe`. `resources/subscribe` and
`resources/unsubscribe` are accepted and acknowledged. Over the HTTP transport, a
session that has opened the `GET /mcp` SSE stream receives a
`notifications/resources/updated` event (with `params.uri` =
`codegraph://stats`) when the graph hot-reloads on disk, signaling that resource
contents have changed and should be re-read. (The stdio transport is
request/response only and does not push.)

## MCP prompts

`prompts/list` reports four guided workflows; `prompts/get` returns a single
user-role message that tells the host model how to drive the tools. Arguments are
sanitized before interpolation.

| Prompt | Arguments | What it asks for |
|---|---|---|
| `onboard` | none | Orient in the codebase via `graph_stats`, `god_nodes`, and `codegraph://questions`. |
| `explain_subsystem` | `topic` (required) | Explain a subsystem using `query_graph`, `get_source`, and `find_callers`/`find_callees`. |
| `assess_pr` | `pr_number` (required) | Assess a PR's risk via `get_pr_impact` and `affected`. |
| `trace_flow` | `from`, `to` (required) | Trace a path with `shortest_path` and `get_source`. |

An unknown prompt name returns `-32602`.

## Completions

`completion/complete` provides argument autocomplete. It keys off the argument's
`name` and prefix-matches values, returning `{ completion: { values, total,
hasMore } }` capped at 100 values:

- `label`, `source`, `target` -- node labels. The match also sees past leading
  punctuation, so a prefix like `tool_get` matches a method node labeled
  `.tool_get_node()`.
- `repo` -- federated repo tags.
- `community_id` -- community ids.

## Logging

The server advertises the `logging` capability and accepts `logging/setLevel`
(acknowledged with an empty result). It does not currently emit
`notifications/message` log records.

## REST API

A read-only JSON surface mirrors the engine calls behind the tools (for non-MCP
clients). Every route honors the same API-key and Host-allowlist guard as
`/mcp`. Each returns `{ "text": <output> }` with the tool's formatted text passed
through verbatim.

| Method and route | Query params | Returns |
|---|---|---|
| `GET /api/stats` | none | graph stats text |
| `GET /api/god-nodes` | `top_n` (default 10) | god-node list |
| `GET /api/node/:label` | label in the path | node metadata |
| `GET /api/query` | `q` (required), `token_budget` (default 1200) | subgraph text; BFS traversal, no context filter |
| `GET /api/repos` | `repo` (optional) | one member's stats if `repo` is given, else the member list |

`GET /api/query` without `?q=` returns 400.

Example:

```
curl -H "X-API-Key: s3cret" "http://127.0.0.1:8765/api/query?q=authentication&token_budget=800"
```

## Example: a raw JSON-RPC session over stdio

```
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"query_graph","arguments":{"question":"how does login work","mode":"bfs"}}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_source","arguments":{"label":"login_user"}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"affected","arguments":{"label":"login_user"}}}
```

## See also

- [Assistant-Integration](Assistant-Integration) -- wire the server into an
  assistant with `codegraph install`.
- [Querying](Querying) -- the query semantics shared with the CLI.
- [PR-Dashboard](PR-Dashboard) -- the PR tools in detail.
- [Commands](Commands) -- the full CLI reference.
