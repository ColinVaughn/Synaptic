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
one the server supports (`2025-06-18`, `2025-03-26`, or `2024-11-05`) the server
echoes it back, otherwise it returns its latest, `2025-06-18`. A client that
sends no `protocolVersion` gets `2025-06-18`. Server info is
`{ "name": "codegraph", "version": <crate version> }`.

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
codegraph serve [--graph <path>] [--http <addr>] [--api-key <key>] [--source-root <dir>]
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

`tools/list` reports 17 tools. Every tool documents its parameters in its input
schema, and every tool carries annotations so a host knows how safe it is to run:

```json
"annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": <bool> }
```

All 17 tools are `readOnlyHint: true`. `openWorldHint` is `true` only for the
tools that reach outside the graph by shelling out (`list_prs`, `get_pr_impact`,
`triage_prs`, `working_changes_impact`); it is `false` for the rest.

Each tool returns a text content block (the load-bearing, purpose-formatted
output). Four tools additionally declare an `outputSchema` and return a typed
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

Returns: node label, id, source file, type, community, and degree. If nothing
resolves, returns `No node matches '<label>'.`

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
  `embeds`, `depends_on`, `reads_from`).

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

### Structured output

Four tools declare an `outputSchema` and return a `structuredContent` object
beside the text content, so a client can parse the result instead of scraping the
formatted text:

| Tool | `structuredContent` shape |
|---|---|
| `graph_stats` | `{ nodes, edges, communities, extracted, inferred, ambiguous }` |
| `god_nodes` | `{ god_nodes: [{ label, degree, id }] }` |
| `affected` | `{ seed, affected: [{ label, depth, via_relation }] }` |
| `query_graph` | `{ nodes: [{ label, file_type, source_file }], edges: [{ source, relation, target }] }` |

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
`codegraph://stats`) when the graph hot-reloads on disk, signalling that resource
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
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}
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
