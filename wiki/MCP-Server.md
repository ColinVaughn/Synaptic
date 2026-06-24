# MCP Server

`synaptic serve` exposes a loaded graph to an AI assistant over the Model
Context Protocol. It speaks JSON-RPC 2.0 directly (no external MCP runtime
dependency). The server is read-only over the graph; the PR and working-changes
tools shell out to `gh`/`git` to read state but never write.

The graph is loaded once at startup from a `graph.json` and hot-reloads when that
file changes on disk. It also keeps itself current as you edit source: there is no
live filesystem watcher — instead each query first does a cheap, debounced
staleness check against the build manifest and re-extracts any changed files
before answering. So a file you edit but never query is reflected on the *next*
query, not the instant you save. See
[Incremental-Updates → serve auto-freshen](Incremental-Updates#synaptic-serve-auto-freshen-on-query-catch-up).
Every node
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
info is `{ "name": "synaptic", "version": <crate version>, "description": <one-line summary> }`.

Over HTTP, requests sent after initialization carry an `MCP-Protocol-Version`
header. A present-but-unsupported value is rejected with `400 Bad Request`; an
absent header is tolerated (assumed `2025-03-26`, for backwards compatibility),
and the `initialize` request itself is exempt because its version is set by the
negotiation above.

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
synaptic serve [--graph <path>] [--http <addr>] [--api-key <key>] [--source-root <dir>] [--allow-exec] [--concise]
```

- `--graph <path>` selects the `graph.json` to load. Default is the standard
  output location (`synaptic-out/graph.json`). If the file is missing, serve
  exits with an error pointing you to run `synaptic extract` first.
- No `--http`: serve over **stdio** (the default transport).
- `--http <addr>`: serve over **HTTP** at `host:port` (for example
  `127.0.0.1:8765`).
- `--api-key <key>`: require this key on HTTP requests. May also be set via the
  `SYNAPTIC_API_KEY` environment variable (the flag takes precedence).
- `--source-root <dir>`: the trusted root for resolving a node's source file in
  `get_source`. Default is the directory above `synaptic-out/` (the repo root);
  it falls back to the current directory when that cannot be derived.
- `--allow-exec`: expose the command-running `speculate` tool (off by default).
  This makes the server **no longer read-only** — `speculate` executes the
  project's test/build commands in a throwaway worktree — so enable it only for
  trusted clients. Without it the tool is neither advertised nor runnable.
- `--concise`: token-lean output. Lowers the default list/budget sizes so tool
  results return less to the model (`query_graph` `token_budget` 1200, list limits
  to 20, `dynamic_hazards` to 20, `get_community` to 40, `top_n` to 6,
  `context_lines` to 25). An explicit per-call argument always wins. Equivalent to
  setting the `SYNAPTIC_CONCISE` environment variable (see [Configuration]).

### stdio transport

```
synaptic serve
```

Newline-delimited JSON-RPC 2.0 on stdin/stdout: one request per line, one
response line per request. Notifications (requests with no `id`) get no reply.
Blank lines and unparseable lines are ignored. A status line is printed to
stderr (`[synaptic] MCP server ready on stdio`) so it never pollutes the
JSON-RPC stream on stdout.

This is the mode an assistant launches as a subprocess. See
[Assistant-Integration](Assistant-Integration) for wiring it into a host.

### Registering with Codex

`synaptic install codex` wires this stdio server into Codex automatically: a
`[mcp_servers.synaptic]` entry in the project `.codex/config.toml` (Codex CLI),
or a per-repo `[mcp_servers.synaptic-<repo>]` in the global `~/.codex/config.toml`
with `synaptic install codex --global` (Codex desktop app, which only reads the
global config). `synaptic` must be on your `PATH`. See
[Assistant-Integration](Assistant-Integration#codex).

### HTTP transport

```
synaptic serve --http 127.0.0.1:8765 --api-key s3cret
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

A startup line is printed to stderr: `[synaptic] MCP server on
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

If `--api-key` (or `SYNAPTIC_API_KEY`) is set and non-blank, every request to
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

`tools/list` reports 29 tools by default (30 with `--allow-exec`, which adds
`speculate`). Every tool documents its parameters in its input schema, and every
tool carries annotations so a host knows how safe it is to run:

```json
"annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": <bool> }
```

All 29 default tools are `readOnlyHint: true`. `openWorldHint` is `true` only for
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
output). Fourteen tools additionally declare an `outputSchema` and return a typed
`structuredContent` object alongside the text (a 2025-06-18 feature) -- see
[Structured output](#structured-output).

### query_graph

Primary entry point. Retrieve a relevant subgraph for a natural-language
question.

Results are ranked by relevance: expansion is best-first (a priority frontier),
high-fan-out hub nodes are down-weighted so they do not flood the result, and
every node carries a relevance `score`. Nodes and edges come back sorted by it, so
you can focus on the top results and ignore the low-scored tail. See
[Querying#query] for the scoring details.

Parameters:
- `question` (string, required) -- the natural-language question.
- `mode` (string, `"bfs"` or `"dfs"`) -- traversal mode. Default `bfs`. Both
  expand best-first by relevance; the mode only breaks score ties (bfs favors
  shallower nodes, dfs deeper ones).
- `full` (boolean) -- return the whole subgraph (all budget-bounded nodes plus
  their edges) instead of the terse top-N node list. Default false. Set it true
  when you need the relationships, not just which symbols match.
- `token_budget` (integer) -- approximate output token budget for the full
  subgraph. Default 1200. It maps to a node cap (about `budget/40`, clamped to
  10..400); the terse default shows only the top ~15 of those. The rendered text
  is bounded to about `token_budget` tokens: output within `token_budget*4` bytes
  is returned as-is (a fast path), and only larger output is truncated, using the
  `cl100k_base` tokenizer for an exact cut. In `full` mode the edges are capped to
  about twice the node count.
- `context_filter` (array of strings) -- keep only nodes whose source file path
  contains one of these substrings.
- `since` (string) -- optional recency boost. Nodes whose file changed on the
  current branch since this baseline rank higher. The baseline is a git ref
  (`main`, `HEAD~10`), a date (`"2 weeks ago"`), or `auto` to detect the default
  branch. Scope is `merge-base(since, HEAD)..working-tree`, so it includes
  uncommitted edits; the boost is churn-weighted. Silently ignored when the source
  is not a git repo or the ref does not resolve.
- `recency_mode` (string, `"boost"` or `"seed"`) -- only with `since`. `boost`
  (default) re-ranks query matches by recency. `seed` also injects changed-file
  nodes as seeds, so the changed surface appears even when the question matches
  little -- use it to answer "what did this branch change".

By default the response is terse: a header (`Traversal: <mode> | Start: [<seeds>]
| <n> nodes found`, plus a `Recency:` line when `since` is used) followed by the
top ranked `NODE` lines (`[score]`, an optional `(changed)` marker, label, file
type, source file) and **no edges**, ending with a `(terse: top N of M matched
nodes ...)` note when more matched. Pass `full=true` for the whole subgraph: all
budget-bounded nodes plus `EDGE` lines (`source --relation--> target`). Either way
a `structuredContent` mirror (see below) accompanies the text.
When the `SYNAPTIC_QUERY_LOG` environment variable points to a path, each query
is appended to it as JSONL (disable with `SYNAPTIC_QUERY_LOG_DISABLE=1`).

### Resolving names

Every tool that takes a `label`/`symbol`/`source`/`target` resolves it through the
same cascade: exact node id, then case-insensitive label, bare name, source file,
and finally a unique label substring. When a name is shared by several files you
can pin it to one with a `name@file-substring` qualifier (e.g.
`announce@core/foo.ts`) -- this works uniformly across `get_node`,
`get_neighbors`, `get_source`, `find_callers`, `find_callees`, `find_references`,
`shortest_path`, `affected`, and `predict_edit`. (`plan_rename` instead takes a dedicated `file`
parameter for the same purpose.) If the name is still ambiguous, the tool returns
the candidate list with each candidate's id, file, and degree inline, so you can
pick one without a follow-up `get_node` call.

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
- `label` (string) -- resolves to a node. Omit when reading by `file`.
- `file` (string) -- read this file directly instead of resolving a symbol:
  repo-relative, or `tag/path` in a federated graph (the `tag` from `list_repos`).
  Use it to read a region that is not a single symbol -- a config block, or the
  lines around a [`search_text`](#search_text) hit.
- `lines` (string) -- with `file`, the range to read: `"108-140"`, or a single
  `"108"` (reads `context_lines` from there). Ignored without `file`.
- `context_lines` (integer) -- how many lines to return from the symbol's/line's
  start. Default 40, clamped to 1..400.

Resolves the node (or the raw `file`), reads it under the `--source-root` jail,
and returns a header (`<label> [<type>] <source_file>:L<from>-L<to>`, or
`<file>:L<from>-L<to>` for a raw range) followed by the numbered lines. For a
symbol the graph records a start line, so the window is `context_lines` from
there (or stops at the symbol's end when a span is recorded).

In a **federated/global graph**, a node's `source_file` is repo-prefixed
(`<tag>/...`) and the member repos live in sibling directories outside any single
`--source-root`. When the server is started on the global graph (a
`global-manifest.json` sits next to it), `get_source` registers each member's own
source root from the manifest and resolves a federated node under that root, so
sources read correctly across repos. A co-located workspace build (members are
subdirectories of one root) already resolves under the single `--source-root`.

When the source cannot be read, the message names the cause and the root it
tried, instead of a bare "not available": no source root configured; the file was
not found under `<root>` (with a hint that in a federated workspace it may live in
a sibling repo outside this root); or the path resolved outside the configured
`--source-root` and was refused.

### get_neighbors

List a node's neighbours, optionally filtered by relation.

Parameters:
- `label` (string, required).
- `relation_filter` (string) -- case-insensitive substring; keep only neighbours
  whose relation contains it.
- `show_sites` (boolean) -- under each neighbour, print the actual source line of
  that edge's call/reference site (`at file:line: <code>`, read from the jail).
  Default false; enriches the text view only.
- `limit` (integer, default 50) -- max neighbours listed before a `+N more`
  summary. Ignored when `verbose` is true.
- `verbose` (boolean, default false) -- list every neighbour instead of the
  capped top-N (use after a `relation_filter` on a hub).

Returns one line per neighbour with a direction marker and the relation in
brackets, capped at `limit` with a `+N more` note on a hub. When a
`relation_filter` matches none of the node's edges, the result is `(none with
relation '<filter>'; this node has: <rel>(<count>), ...)` -- naming the relations
the node does have, so an empty result is not mistaken for a missing node. A
`structuredContent` mirror carries `{ seed, neighbors, by_relation, total,
truncated }`, where `by_relation` tallies every edge on the node before any filter
and `total` is the full matched count (which may exceed the capped `neighbors`).

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
- `top_n` (integer) -- how many to return. Default 10, capped at 200 per page
  (page further with `offset`); each hub costs a reverse-impact walk to count its
  tests, so the page is bounded.
- `offset` (integer) -- hubs to skip before the page (absolute rank is preserved
  in the numbering). Default 0.

Returns a ranked list of label, connection count, and how many tests transitively
exercise each hub (`N test(s)`), plus a `structuredContent` mirror. A hub with
`0 test(s)` is an untested high-blast-radius symbol -- exactly what to flag.

`degree` is **total connections** -- every edge kind, including the `method`/`contains`
edges that link a class to its members. It therefore measures structural
centrality/size, **not** how many things depend on a symbol: a class can top this
list yet have very few incoming dependents (its members hold the coupling). For
"what depends on X / what breaks if I change it", use [`affected`](#affected), not
degree.

### graph_stats

Node, edge, and community counts plus the edge-confidence breakdown.

Parameters: none.

Returns `Graph: <n> nodes, <n> edges, <n> communities` and
`Edges: <n> EXTRACTED, <n> INFERRED, <n> AMBIGUOUS`, plus a `structuredContent`
mirror. On a federated (multi-repo) graph it adds a `Cross-repo: <n> edge(s) span
repositories (<n> cross-language: HTTP/RPC/FFI/WebSocket boundaries)` line, and the
structured output carries `cross_repo` / `cross_language` counts (both 0, and the
line omitted, for a single-repo graph). When the graph has reflection /
dynamic-dispatch sites it adds a `Dynamic-dispatch sites: <n> (<n> opaque, <n>
evidence-linked)` line, mirrored in the structured output as `dynamic_sites` /
`dynamic_sites_opaque` / `dynamic_refs_linked` (see
[`dynamic_hazards`](#dynamic_hazards)).

### list_repos

Federated workspace members (repo tags) with node/edge counts. Edges are counted
under their source node's repo. Empty (single-repo) graphs return `No federated
repos (single-repo graph).` plus a `structuredContent` mirror
(`{ repos: [{ repo, nodes, edges, source_hash? }] }`, an empty array for a
single-repo graph). See [Workspaces-and-Federation](Workspaces-and-Federation).

When a `workspace-state.json` sits next to the graph, each repo also carries a
`source_hash` -- a content fingerprint of that member's sources from the last
extraction -- shown as `src <hash>` in the text and `source_hash` in the structured
output. It makes **per-repo staleness** visible in a federation: a member whose code
changed since the graph was built keeps its old fingerprint until that member is
re-extracted.

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

**Class/type nodes fold in their members.** A class's callers attach to its
methods, not the bare type symbol -- a reverse walk from the class node alone would
return ~0 and read as "safe to change". So when the target is a class / struct /
interface / enum / trait, `affected` seeds the walk from the type **and its
members** and prefixes the output with a note (`<X> is a class with <N> members;
impact is aggregated across the class and its members`). The `structuredContent`
mirror carries `aggregated_over_members: <N>`. The same fold applies to
[`find_callers`](#find_callers) / [`find_callees`](#find_callees) (which report the
external callers/callees of the members) and to [`describe_node`](#describe_node)
(which lists the members). See also the [Limitations](#limitations) on dynamic
dispatch.

When the name does not resolve to a single node, `affected` reports it rather than
silently picking one: the text lists the candidates, and the structured mirror sets
`resolved: false` (with `ambiguous: true` and a `candidates` array, or `found:
false`) instead of a misleading `total: 0`.

### find_callers

The nodes that call, use, or reference this symbol (incoming call-like edges
only). Answers "who calls X". The header carries the true total and a
per-relation breakdown; the list is capped on a hub with a `+N more` summary.

Parameters:
- `label` (string, required).
- `limit` (integer, default 50) — max callers listed before a `+N more` summary. Ignored when `verbose` is true.
- `verbose` (boolean, default false) — emit the full, uncapped caller list.
- `show_sites` (boolean, default false) — under each caller, print the actual
  source line where the call happens (`at file:line: <code>`, read from the jail).
  Turns "who calls X" into "who calls X, and the exact line" with no second
  `get_source`.

For a class/type, the external callers of its **members** are folded in and the
output is labelled (a class's callers attach to its methods). Only static callers
are seen -- see [Limitations](#limitations).

### find_callees

The nodes this symbol calls, uses, or references (outgoing call-like edges only).
Answers "what does X call". Same capped, count-and-breakdown output as
`find_callers`. For a class/type, the callees of its **members** are folded in and
labelled (a class doesn't call; its methods do).

Parameters:
- `label` (string, required).
- `limit` (integer, default 50) — max callees listed before a `+N more` summary. Ignored when `verbose` is true.
- `verbose` (boolean, default false) — emit the full, uncapped callee list.
- `show_sites` (boolean, default false) — under each callee, print the actual
  source line where this symbol calls it (`at file:line: <code>`), so "what does X
  call" also shows HOW it calls it.

### find_references

Find-all-references: **every** place a symbol is used, not just where it is called.
`find_callers` reports incoming call/use/reference edges, so for a type or interface
it misses the `imports`, `implements`/`inherits`, and type-use edges that are the
whole point of "where is this type used". `find_references` returns every incoming
edge except structural ownership (`contains`/`defines`/`has_*`), so the result unions
calls, imports, inheritance/implements, type uses, cross-language coupling
(`calls_service`/`handled_by`/`invokes`/`binds_native`), and evidence-linked
`dynamic_ref`. The header carries the total and a per-relation breakdown.

It is the superset companion to `find_callers` (calls only) aimed at
types/interfaces/enums/constants. References are to the symbol **itself** — unlike
`find_callers`, a class's members are **not** folded in (it answers "where is `Foo`
referenced", not "where are `Foo`'s methods called"). On a federated graph a
cross-repo use surfaces the same as a local one.

Parameters:
- `label` (string, required).
- `limit` (integer, default 50) — max references listed before a `+N more` summary. Ignored when `verbose` is true.
- `verbose` (boolean, default false) — emit the full, uncapped reference list.
- `show_sites` (boolean, default false) — under each reference, print the actual
  source line where the use happens (`at file:line: <code>`).

### list_prs

Open PRs with CI/review state targeting the base branch. Requires the `gh` CLI.
When `gh` is missing or unauthenticated, the tool reports
`gh CLI not found or not authenticated (run: gh auth login). PR data is skipped;
graph audit continues offline.` -- the failure is scoped to the PR tools and the
rest of the graph stays usable. See [PR-Dashboard](PR-Dashboard).

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

Default output lists the changed files plus node/community counts. Pass
`verbose` to also list the top touched nodes (ranked by connectivity) and the
touched communities with labels.

Parameters:
- `base` (string) -- base branch to diff against; defaults to the detected
  default branch.
- `verbose` (boolean, default false) -- also list the top touched nodes and labeled communities, not just files.
- `limit` (integer, default 20) -- max touched nodes listed when `verbose`.
- `code_only` (boolean, default false) -- count and list only code nodes,
  excluding non-code files (`package.json`, lockfiles, `.md` docs) to sharpen the
  blast radius.

Returns `Working changes vs <base>: <n> files, <n> graph nodes, <n> communities
touched` and the changed files. With `code_only`, the count reads `<n> code graph
nodes`. The two empty outcomes are reported distinctly: a real clean tree gives
`No changes vs <base>.`, while a missing/failed git or a directory that is not a
git repository (e.g. the top-level directory of a federated workspace) gives a
`git unavailable or not a git repository ... Graph audit continues offline.`
message, so the two are never conflated. `git` runs in the server's working
directory, so run the server from inside the repo whose diff you want.

### predict_impact

Forecast the consequences of a change before editing: which graph nodes the
changed files define, the reverse-impact blast radius that depends on them, which
edited symbols are public API (callers outside the file or module may break), and
a verify checklist. Pure-graph and read-only; for new-cycle / removed-API
detection use `time_travel_diff` or the `synaptic predict` CLI (those build
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
the verify checklist, plus a `structuredContent` mirror carrying the full
`ChangeForecast` (not truncated by `limit`, which caps only the text).

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
test in the graph reaches the change, plus a `structuredContent` mirror
(`{ tests, total }`). Inline unit tests not under a test path (e.g. Rust
`#[cfg(test)]`) are not detected.

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
- `limit` (integer, default 20) -- max entries shown per section (will break /
  review) before a `+N more` summary. Ignored when `verbose`.
- `verbose` (boolean, default false) -- emit the full, uncapped lists instead of
  the summarized top-N.

Returns the summary plus the "will break" and "review" dependents. Each section
header carries the total and a by-depth rollup (e.g. `1h: 274, 2h: 155`); each
listed dependent shows its hop count, relation, label, file, and the reason. On a
hub the list is capped with a `+N more` note unless `verbose`. Returns a note if
the symbol or kind is not recognized.

### structural_search

Structural search over the graph with SYNQL (a small Cypher-inspired query
language), a named architectural pattern, or a file outline. Not text search: it
matches on kind/visibility/loc/fan-in/out/etc. `.name` is the bare symbol (no
parentheses); use `=~` for a regex/substring match.

Parameters:
- `query` (string) -- a SYNQL query, e.g. `MATCH (c:class) WHERE c.loc > 500 RETURN c`.
  Omit when using `pattern` or `file`.
- `pattern` (string) -- a built-in pattern name instead of a query: `singleton`,
  `factory`, `observer`, `service-locator`, `god-class`.
- `file` (string) -- list every symbol defined in this file (a path substring),
  ordered by line: a **file outline**, no query needed. Used only when `query` and
  `pattern` are omitted (precedence is `pattern` > `query` > `file`). The path
  matches literally; on a federated graph a bare path matches the file across every
  member, while a `tag/`-qualified path scopes to one.
- `limit` (integer) -- max rows to return. Default 25.

Returns the matched rows (one node per line: label, kind/visibility, source
location), or the parse error if the query is malformed. It also returns a
`structuredContent` mirror: one object per matched node with its `id`, `label`,
`kind`, `visibility`, `file`, `line`, `loc`, and captured **signature** (params
with optional types, return type, and the raw header), so an agent can route on
a function's shape without reading source. Aggregate queries (`count(...)`,
projections) return scalar `groups` instead.

### search_text

The complement to `structural_search`. Where `structural_search` matches the
**graph** (kinds, loc, fan-in/out, symbol names) and cannot see file content,
`search_text` is a real **content** search over the source files -- for
everything text-shaped the graph does not model: string literals, config values,
log messages, a TODO's wording, error strings, magic numbers. It reads through
the same per-repo source roots and containment jail as `get_source` (so it needs
a `--source-root`, or a federated graph whose members register their own roots),
honoring each repo's `.gitignore`/`.synapticignore` and **skipping Synaptic's own
generated output** (any `synaptic-out/` directory, plus any custom `--out` dir
identified by its `graph.json` + `.manifest.json` marker pair), so the graph
artifacts, exports, and `graph.json.bak*` backups never drown real source hits. A
genuine source file merely named `graph.json` (no sibling manifest) is still
searched.

Parameters:
- `pattern` (string, required) -- a regex by default; a fixed string when `literal=true`.
- `literal` (boolean) -- treat `pattern` as a literal, not a regex. Default false.
- `case_sensitive` (boolean) -- force case sensitivity. Omit for **smart case**:
  case-insensitive unless `pattern` contains an uppercase letter, so `todo` stays
  broad while `TODO`/`FIXME` are precise (this sharply cuts false positives like a
  lowercase "todos" matching `TODO`). `true` is always sensitive, `false` always
  insensitive.
- `repo` (string) -- restrict to one federated member (a tag from `list_repos`).
  Works even when the graph is served over a single parent source root (the member
  is located under `<source-root>/<tag>`). Omit to search every member / the
  single repo.
- `path_glob` (string) -- only files matching this glob, e.g. `**/*.ts` or `src/**`,
  applied relative to each repo root.
- `max_results` (integer) -- hits to return before truncation is flagged. Default 100, max 1000.

The defining feature is **graph attribution**: every hit carries the node whose
body encloses it (innermost span wins), so a matched line is a pivot -- from a
fragile regex literal straight to `affected`/`find_callers` on the function that
contains it. Each text row is `file:line:col  <line>   [enclosing-symbol kind]`;
the `structuredContent` mirror is
`{ pattern, total, truncated, files_scanned, hits: [{ repo, file, line, col, match, line_text, node? }] }`,
where `node` is null only when the hit falls outside any captured span. On a
federated graph this is strictly more useful than a raw shell `grep`: grep does
not know where the member repos live, which ignore files apply, or which symbol a
line belongs to.

### dynamic_hazards

Lists the **reflection / dynamic-dispatch sites** recorded in the graph. Static
analysis cannot follow a by-name member lookup, a dispatch table, `eval`, a
dynamic `import()`, or .NET / Python / JVM reflection, so a symbol reached only
that way has no static dependents -- and a bare "0 dependents" then reads as "safe
to change" when it is not. This tool is how you judge that risk.

Where it can, Synaptic resolves dynamic dispatch instead of just cataloging it:
event buses (`EventEmitter`, DOM `CustomEvent`, C# events) link a publisher and
subscriber through an `event #<name>` node, and a reflection site whose name is a
**string literal** is evidence-linked to its unique target with a low-confidence
`dynamic_ref` edge (so it shows up as a caveated dependent in `affected` /
`find_callers`). What remains -- computed names, fully-dynamic dispatch -- cannot
be linked, so it is listed here as the residual risk, and `affected` / `get_node`
/ `describe_node` attach a `dynamic_caveat` to a 0-dependent symbol in such a
scope.

Parameters:
- `repo` (string) -- restrict to one federated member (a tag from `list_repos`).
- `path_glob` (string) -- only sites in files matching this glob, e.g. `**/*.ts`.
- `kind` (string) -- one of `reflection`, `dynamic_import`, `eval` (the kinds the
  detectors emit; event buses are modeled as edges, not sites).
- `target` (string) -- only sites that could reach this symbol: sites whose literal
  key names it, plus opaque sites in a file that defines it.
- `max_results` (integer) -- sites to return before truncation. Default 30, max 1000.
  It is a scan-and-narrow tool: filter by `repo`/`path_glob`/`kind`/`target`
  rather than raising this.

Each text row is `[repo] file:line  <kind>  <"key"|(opaque)>  in <enclosing
symbol>`; the `structuredContent` mirror is
`{ total, truncated, sites: [{ repo, file, line, kind, key, host }] }`. `graph_stats`
reports the totals (`dynamic_sites`, `dynamic_sites_opaque`, `dynamic_refs_linked`),
and the CLI exposes the same listing as `synaptic hazards`.

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

Plan-only: a confidence-scored rename plan for an agent to apply. Never edits
source. After applying the edits, run `synaptic refactor verify` on the CLI to
check the post-edit graph.

Parameters:
- `name` (string, required) -- the symbol to rename (its name, or a node id).
- `to` (string, required) -- the new name.
- `id` (string) -- disambiguate by node id when the name matches several definitions.
- `file` (string) -- disambiguate by a file-path substring.
- `limit` (integer, default 50) -- max sites listed per section (Edits, Review) before a `+N more` summary. Ignored when `verbose` is true.
- `verbose` (boolean, default false) -- list every edit/review site instead of the summarized top-N.

Returns the `Rename <old> -> <new> [<confidence>], <n> edit(s) across <n>
file(s), <n> to review, <n> affected` summary, followed by the actual edit sites
(`file:line:col`, `old -> new`, reason, confidence) under `Edits (<n>):` and the
lower-confidence ones under `Review (<n>):` — so an agent can apply the rename
without a second round-trip to the CLI's `plan.md`. Returns an error string if the
symbol is not found.

### audit_sql

Audit the codebase's SQL for performance and security problems over the
SQL-aware graph: row-level-security gaps, over-broad grants, likely SQL
injection, missing indexes on filter/foreign-key columns, `SELECT *`,
non-sargable predicates, N+1 patterns, and missing primary keys.

Parameters:
- `severity` (string) -- only return findings at least this severe
  (`critical`|`high`|`medium`|`low`|`info`).
- `limit` (integer, default 20) -- max findings before a `+N more` summary.
  Ignored when `verbose` is true.
- `verbose` (boolean, default false) -- list all findings AND each finding's full
  detail + fix, instead of the terse one-line-per-finding summary.

Returns a one-line report summary, then one line per finding by default
(`[severity] rule_id @ location (conf) title`); `verbose` adds each finding's
detail and fix. A `structuredContent` mirror carries the full `AuditReport`. See
[SQL Auditing](SQL-Auditing).

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

Fourteen tools declare an `outputSchema` and return a `structuredContent` object
beside the text content, so a client can parse the result instead of scraping the
formatted text:

| Tool | `structuredContent` shape |
|---|---|
| `graph_stats` | `{ nodes, edges, communities, extracted, inferred, ambiguous, cross_repo, cross_language }` |
| `get_node` | `{ found, id, label, source_file, file_type, degree, community?, kind?, visibility?, loc? }` (on an ambiguous name: `found:false` with `ambiguous`+`candidates`, matching `affected`/`describe_node`) |
| `god_nodes` | `{ god_nodes: [{ label, degree, id, test_count }] }` (`degree` = total connections incl. members; centrality/size, not incoming-dependence) |
| `affected` | `{ seed, resolved, affected: [{ label, depth, via_relation }], total, truncated, by_depth, aggregated_over_members? }` (on an unresolved name: `resolved:false` with `ambiguous`+`candidates` or `found:false`) |
| `query_graph` | `{ nodes: [{ label, file_type, source_file, score, changed }], edges: [{ source, relation, target }] }` (nodes sorted by `score`; `changed` is true when `since` was given and the node's file changed; `edges` is empty unless `full=true`) |
| `structural_search` | `{ columns, results: [[{ id, label, kind, visibility, file, line, loc, signature }]] }` (or `groups` for aggregates) |
| `search_text` | `{ pattern, total, truncated, files_scanned, hits: [{ repo, file, line, col, match, line_text, node? }] }` (`node` is the enclosing symbol `{ id, label, kind, community }`, or null when the hit is outside any captured span) |
| `describe_node` | `{ found, id, label, kind, summary, callees, signature, members?, member_count? }` (`members` listed for a class/type; on an ambiguous name: `found:false` with `ambiguous`+`candidates`) |
| `get_neighbors` | `{ seed, neighbors: [{ label, relation, direction }], by_relation: { <relation>: <count> }, total, truncated }` (`by_relation` tallies every edge before any filter; `total` is the full matched count, capped to `limit`) |
| `list_repos` | `{ repos: [{ repo, nodes, edges, source_hash? }] }` (empty array for a single-repo graph; `source_hash` present when a `workspace-state.json` sibling exists) |
| `predict_impact` | the full `ChangeForecast`: `{ summary, changed_files, changed_nodes, public_api_breaks, blast_radius, blast_radius_total, at_risk_tests, verify_checklist, risk }` (not truncated by `limit`, which caps only the text) |
| `affected_tests` | `{ tests: [{ id, label, file, depth, via_relation }], total }` |
| `audit_sql` / `advise_sql` | `{ version, summary, findings: [{ rule_id, severity, category, title, detail, location, remediation, confidence }] }` |

The other tools return text only. A tool whose structured mirror cannot resolve
its node (e.g. `get_neighbors` on an ambiguous label) omits `structuredContent`
rather than returning a null object; the text content still carries the
disambiguation message.

### Tool error behavior

An unknown tool name is returned as a tool result with `isError: true` and a text
body `Unknown tool: <name>` (not a JSON-RPC protocol error). An unknown JSON-RPC
method returns error code `-32601`. An unknown resource or prompt returns
`-32602`.

### Limitations

- **Static analysis only (with modelled exceptions).** Edges are read from source
  structure, so a call wired up at runtime is not always captured. Two common
  cross-process mechanisms **are** modelled, so their handlers do show callers:
  **Electron IPC** (`ipcMain.handle`/`on` ↔ `ipcRenderer.invoke`/`send`/`on`,
  `webContents.send`) and **WebSocket / socket.io** message channels — a sender and
  its handler meet on a channel node (`ipc #<ch>` / `ws #<cmd>`), so `affected`,
  `find_callers`, and `shortest_path` cross the boundary. Still not traced: a
  handler reached only via a custom event bus, a DI container, a runtime-built
  dispatch table, or reflection can show **0 callers** even though it runs. Read a
  surprising 0-caller result on a dispatched handler as "no *static* caller", not
  "dead code".
- **Class vs. method impact.** Because callers attach to a class's methods, not the
  bare type symbol, `affected` / `find_callers` / `find_callees` / `describe_node`
  fold a class's members in automatically and label the result (see
  [`affected`](#affected)). `god_nodes` `degree` still counts those member edges, so
  it ranks structural size/centrality, **not** incoming dependence -- use `affected`
  for blast radius.
- **Inline unit tests.** A test defined in the same file as the code under test may
  not be linked as a separate test node, so `affected_tests` can undercount in
  test-sparse or inline-test codebases.
- **Text content is not in the graph (but `search_text` reaches it).** The graph
  models structure, not the bytes of a line: string literals, config values, log
  messages, a TODO's wording, and magic numbers are not nodes. Use
  [`search_text`](#search_text) for those -- it searches the source directly and
  attributes each hit back to its enclosing symbol, so you still land on a graph
  node to pivot from. For *reading* logic, `get_source` returns a symbol's body or
  an arbitrary `file`+`lines` range, and `show_sites` on `find_callers` /
  `find_callees` / `get_neighbors` prints the exact call line for each edge -- so
  "A calls B" becomes "A calls B at this line" without leaving the graph.
- **Federated staleness.** The graph is a snapshot. In a multi-repo workspace,
  members drift between extractions; `list_repos` surfaces a per-repo `source_hash`
  (when a `workspace-state.json` sibling exists) so drift is at least visible, but
  the graph still reflects the last extraction until you re-run it.

## MCP resources

`resources/list` reports six read-only resources, each fetched with
`resources/read` by `uri`:

- `synaptic://report` (text/markdown) -- the `GRAPH_REPORT.md` next to the
  loaded graph, if present.
- `synaptic://stats` (text/plain) -- the same content as `graph_stats`.
- `synaptic://god-nodes` (text/plain) -- the top 10 god nodes.
- `synaptic://surprises` (text/plain) -- surprising cross-community connections.
- `synaptic://audit` (text/plain) -- the edge-confidence breakdown.
- `synaptic://questions` (text/plain) -- suggested questions to ask the graph.

See [Analysis-and-Reports](Analysis-and-Reports) and
[Semantic-Analysis](Semantic-Analysis) for what these surface.

### Resource templates

`resources/templates/list` reports two templates, so any node or community is
addressable as a resource by URI:

- `synaptic://node/{label}` (text/plain) -- metadata for one node by label, id,
  or bare name (the same content as `get_node`).
- `synaptic://community/{id}` (text/plain) -- members of one community by id.

`resources/read` resolves these templated URIs directly (for example
`synaptic://node/AuthService`).

### Resource subscriptions

The server advertises `resources.subscribe`. `resources/subscribe` and
`resources/unsubscribe` are accepted and acknowledged. Over the HTTP transport, a
session that has opened the `GET /mcp` SSE stream receives a
`notifications/resources/updated` event (with `params.uri` =
`synaptic://stats`) when the graph hot-reloads on disk, signaling that resource
contents have changed and should be re-read. (The stdio transport is
request/response only and does not push.)

## MCP prompts

`prompts/list` reports four guided workflows; `prompts/get` returns a single
user-role message that tells the host model how to drive the tools. Arguments are
sanitized before interpolation.

| Prompt | Arguments | What it asks for |
|---|---|---|
| `onboard` | none | Orient in the codebase via `graph_stats`, `god_nodes`, and `synaptic://questions`. |
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
  assistant with `synaptic install`.
- [Querying](Querying) -- the query semantics shared with the CLI.
- [PR-Dashboard](PR-Dashboard) -- the PR tools in detail.
- [Commands](Commands) -- the full CLI reference.
