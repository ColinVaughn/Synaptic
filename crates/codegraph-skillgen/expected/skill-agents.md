---
name: codegraph
description: Queries this repo's CodeGraph knowledge graph (symbols and their calls, imports, and inheritance) instead of grepping or reading files. Use when exploring an unfamiliar codebase, finding what calls or depends on a symbol, tracing how one part reaches another, or judging the blast radius of a change.
---

# CodeGraph for your AI agent

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

## MCP (preferred for your AI agent)
Use the **codegraph** MCP server's tools (`query_graph` first), then
`get_neighbors`, `shortest_path`, `god_nodes`, `graph_stats`, `get_node`,
`get_community`, plus the PR tools `list_prs` / `get_pr_impact` / `triage_prs`.
Reference them with your client's MCP prefix (Claude Code:
`mcp__codegraph__query_graph`). The server's `initialize` reply describes the
toolset, and each tool documents its parameters. If the server is not already
connected, start it with `codegraph serve`.

Reach for the graph on "what calls X", "what breaks if I change Y", and "how does
A reach B". Don't reconstruct those by reading files.
