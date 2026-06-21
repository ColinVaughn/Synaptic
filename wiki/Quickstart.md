# Quickstart

This walks through building a graph for a repository and using it. It assumes the
`synaptic` binary is on your PATH (see [Installation](Installation)).

## 1. Build the graph

```sh
cd your-project
synaptic extract .
```

This scans the directory, extracts symbols and relationships, clusters and analyzes the
graph, and writes everything to `synaptic-out/`. It honors `.gitignore` and
`.synapticignore`, skips dependency and build directories, and skips sensitive files such
as `.env` and private keys. A code-only run makes no network calls. See
[Extraction](Extraction) for the full discovery and caching behavior.

Key outputs (full list in [Output Formats](Output-Formats)):

- `synaptic-out/graph.json` - the canonical graph
- `synaptic-out/GRAPH_REPORT.md` - structural insight report
- `synaptic-out/graph.html`, `graph-3d.html`, `graph.svg` - visualizations

## 2. Read the report

Open `synaptic-out/GRAPH_REPORT.md` for god nodes (most-connected symbols), surprising
connections, import cycles, and suggested questions. See
[Analysis and Reports](Analysis-and-Reports).

## 3. Query the graph

```sh
# Relevant subgraph for a topic:
synaptic query "authentication flow"

# Depth-first expansion (favors deep call chains):
synaptic query "request lifecycle" --dfs --max-nodes 40

# Shortest path between two symbols:
synaptic path login validate_token

# A node and its neighbours:
synaptic explain parse_config

# Reverse impact: what depends on a symbol?
synaptic affected parse_config --depth 3
```

See [Querying](Querying) for how seeds are resolved and how traversal works.

## 4. Explore visually

Open the generated files in a browser:

- `graph.html` - 2D explorer with search, community color, and relation filters
- `graph-3d.html` - 3D force graph
- `tree.html` - file tree; `callflow.html` - call-flow diagram

See [Visualizations](Visualizations).

## 5. Keep it current

```sh
synaptic update path/to/changed_file.rs   # incremental rebuild
synaptic watch                            # rebuild automatically on save
synaptic hook install                     # rebuild on commit/checkout via git hooks
```

See [Incremental Updates](Incremental-Updates).

## 6. Use it from an AI assistant

```sh
synaptic serve                 # MCP server over stdio
synaptic install               # wire Synaptic into a host assistant
```

`install` wires Synaptic into a host assistant so it queries the graph before grepping or
reading files: a Claude skill file + hooks, an always-on instructions block for other
assistants, and a native MCP server + hook for Codex. See [MCP Server](MCP-Server) and
[Assistant Integration](Assistant-Integration).

## Next steps

- Multiple repos or a monorepo: [Workspaces and Federation](Workspaces-and-Federation)
- Enrich docs/papers with an LLM: [Semantic Analysis](Semantic-Analysis)
- Pull in external sources: [Ingestion](Ingestion)
- Every flag of every command: [Commands](Commands)
