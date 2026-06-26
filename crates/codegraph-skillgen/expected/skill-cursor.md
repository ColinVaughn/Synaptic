---
name: codegraph
description: Queries this repo's CodeGraph knowledge graph (symbols and their calls, imports, and inheritance) instead of grepping or reading files. Use when exploring an unfamiliar codebase, finding what calls or depends on a symbol, tracing how one part reaches another, reading a symbol's source, or judging the blast radius of a change.
---

# CodeGraph for Cursor

This repository has a **CodeGraph** knowledge graph of its code. Before grepping
or reading files broadly, query the graph. It is faster and surfaces
relationships (calls, imports, inheritance, impact).

## Build / refresh
- `codegraph extract .`: build the graph into `codegraph-out/`.
- `codegraph update <changed files>`: incremental rebuild after edits.

## Query (CLI)
- `codegraph query "<question>"`: the relevant subgraph for a question.
- `codegraph explain <node>`: a node and its neighbours.
- `codegraph path <a> <b>`: shortest path between two nodes.
- `codegraph affected <node>`: what (transitively) depends on a node.

## MCP (preferred for Cursor)
Use the **codegraph** MCP server's tools. Start with `query_graph`, then:
- `get_source` -- read a symbol's actual code (no need to open the file).
- `affected` -- the blast radius of changing a symbol; `working_changes_impact`
  does the same for your current git diff (no PR needed).
- `find_callers` / `find_callees` -- who calls a symbol / what it calls.
- `get_neighbors`, `shortest_path`, `god_nodes`, `graph_stats`, `get_node`,
  `get_community` -- navigate and inspect the graph.
- `list_prs` / `get_pr_impact` / `triage_prs` -- graph-aware PR review (need `gh`).

Reference them with your client's MCP prefix (Claude Code:
`mcp__codegraph__query_graph`). The server's `initialize` reply describes the
toolset, and each tool documents its parameters. If the server is not already
connected, start it with `codegraph serve`.

Reach for the graph on "what calls X", "what breaks if I change Y", "how does A
reach B", and to read a symbol's code. Don't reconstruct those by reading files.
