# Quickstart

This walks through building a graph for a repository and using it. It assumes the
`codegraph` binary is on your PATH (see [Installation](Installation)).

## 1. Build the graph

```sh
cd your-project
codegraph extract .
```

This scans the directory, extracts symbols and relationships, clusters and analyzes the
graph, and writes everything to `codegraph-out/`. It honors `.gitignore` and
`.codegraphignore`, skips dependency and build directories, and skips sensitive files such
as `.env` and private keys. A code-only run makes no network calls. See
[Extraction](Extraction) for the full discovery and caching behavior.

Key outputs (full list in [Output Formats](Output-Formats)):

- `codegraph-out/graph.json` - the canonical graph
- `codegraph-out/GRAPH_REPORT.md` - structural insight report
- `codegraph-out/graph.html`, `graph-3d.html`, `graph.svg` - visualizations

## 2. Read the report

Open `codegraph-out/GRAPH_REPORT.md` for god nodes (most-connected symbols), surprising
connections, import cycles, and suggested questions. See
[Analysis and Reports](Analysis-and-Reports).

## 3. Query the graph

```sh
# Relevant subgraph for a topic:
codegraph query "authentication flow"

# Depth-first expansion (favors deep call chains):
codegraph query "request lifecycle" --dfs --max-nodes 40

# Shortest path between two symbols:
codegraph path login validate_token

# A node and its neighbours:
codegraph explain parse_config

# Reverse impact: what depends on a symbol?
codegraph affected parse_config --depth 3
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
codegraph update path/to/changed_file.rs   # incremental rebuild
codegraph watch                            # rebuild automatically on save
codegraph hook install                     # rebuild on commit/checkout via git hooks
```

See [Incremental Updates](Incremental-Updates).

## 6. Use it from an AI assistant

```sh
codegraph serve                 # MCP server over stdio
codegraph install               # wire CodeGraph into a host assistant
```

`install` wires CodeGraph into a host assistant so it queries the graph before grepping or
reading files: a Claude skill file + hooks, an always-on instructions block for other
assistants, and a native MCP server + hook for Codex. See [MCP Server](MCP-Server) and
[Assistant Integration](Assistant-Integration).

## Next steps

- Multiple repos or a monorepo: [Workspaces and Federation](Workspaces-and-Federation)
- Enrich docs/papers with an LLM: [Semantic Analysis](Semantic-Analysis)
- Pull in external sources: [Ingestion](Ingestion)
- Every flag of every command: [Commands](Commands)
