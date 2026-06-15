# MCP Server

`codegraph serve` exposes a loaded graph to an AI assistant over the Model
Context Protocol. It speaks JSON-RPC 2.0 directly (no external MCP runtime
dependency). The server is read-only over the graph; the PR tools shell out to
`gh`/`git` but never write.

The graph is loaded once at startup from a `graph.json` and hot-reloads when that
file changes on disk (see [Incremental-Updates](Incremental-Updates)). Every node
label, relation, and file path is sanitized before it reaches tool output (a
security boundary on names derived from source).

Protocol version reported on `initialize`: `2024-11-05`. Server info:
`{ "name": "codegraph", "version": <crate version> }`.

## Running the server

```
codegraph serve [--graph <path>] [--http <addr>] [--api-key <key>]
```

- `--graph <path>` selects the `graph.json` to load. Default is the standard
  output location (`codegraph-out/graph.json`). If the file is missing, serve
  exits with an error pointing you to run `codegraph extract` first.
- No `--http`: serve over **stdio** (the default transport).
- `--http <addr>`: serve over **HTTP** at `host:port` (for example
  `127.0.0.1:8765`).
- `--api-key <key>`: require this key on HTTP requests. May also be set via the
  `CODEGRAPH_API_KEY` environment variable (the flag takes precedence).

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
- `GET /mcp` (with `Accept: text/event-stream`) -- opens a keep-alive SSE stream
  (the server-to-client channel). There are no server-initiated pushes yet, so it
  emits only keep-alive heartbeats. The stream is bounded (it ends when the
  session is reaped or after a hard cap near the idle timeout).
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

The `initialize` reply carries a server-level `instructions` string that orients
an assistant to the whole toolset (the recommended flow, and what "god node",
"community", and edge confidence mean), and every tool in `tools/list` documents
its parameters in the input schema, so an agent can use each one correctly.

All tools return a single text content block. The text is purpose-formatted (it
is the load-bearing output). The full set, exactly as listed by `tools/list`:

### query_graph

Retrieve a relevant subgraph for a natural-language question, rendered as text
and bounded by a token budget.

Parameters:
- `question` (string, required) -- the natural-language question.
- `mode` (string, `"bfs"` or `"dfs"`) -- traversal mode. Default `bfs`.
- `token_budget` (integer) -- approximate output token budget. Default 2000. It
  maps to a node cap (about `budget/40`, clamped to 10..400) and truncates the
  rendered text to roughly `budget*4` characters.
- `context_filter` (array of strings) -- keep only nodes whose source file path
  contains one of these substrings.

Returns a header (`Traversal: <mode> | Start: [<seeds>] | <n> nodes found`)
followed by `NODE` lines (label, file type, source file) and `EDGE` lines
(`source --relation--> target`). When the `CODEGRAPH_QUERY_LOG` environment
variable points to a path, each query is appended to it as JSONL (disable with
`CODEGRAPH_QUERY_LOG_DISABLE=1`).

### get_node

Show a node's metadata and degree.

Parameters:
- `label` (string, required) -- a label/keyword that resolves to a node.

Returns: node label, id, source file, type, community, and degree. If nothing
resolves, returns `No node matches '<label>'.`

### get_neighbors

List a node's neighbours, optionally filtered by relation.

Parameters:
- `label` (string, required).
- `relation_filter` (string) -- case-insensitive substring; keep only neighbours
  whose relation contains it.

Returns one line per neighbour with a direction marker and the relation in
brackets.

### get_community

List the members of a community.

Parameters:
- `community_id` (integer, required).

Returns the community size and each member (label and source file). Unknown or
empty community returns `No community <id>.`

### god_nodes

The most-connected nodes (highest degree).

Parameters:
- `top_n` (integer) -- how many to return. Default 10.

Returns a ranked list of label and edge count.

### graph_stats

Node, edge, and community counts plus the edge-confidence breakdown.

Parameters: none.

Returns `Graph: <n> nodes, <n> edges, <n> communities` and
`Edges: <n> EXTRACTED, <n> INFERRED, <n> AMBIGUOUS`.

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

### Tool error behavior

An unknown tool name is returned as a tool result with `isError: true` and a text
body `Unknown tool: <name>` (not a JSON-RPC protocol error). An unknown JSON-RPC
method returns error code `-32601`. An unknown resource returns `-32602`.

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

## Example: a raw JSON-RPC call over stdio

```
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"query_graph","arguments":{"question":"how does login work","mode":"bfs"}}}
```

## See also

- [Assistant-Integration](Assistant-Integration) -- wire the server into an
  assistant with `codegraph install`.
- [Querying](Querying) -- the query semantics shared with the CLI.
- [PR-Dashboard](PR-Dashboard) -- the PR tools in detail.
- [Commands](Commands) -- the full CLI reference.
