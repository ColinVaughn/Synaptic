---
name: codegraph
description: Query this repo's CodeGraph knowledge graph before broad file exploration.
---

# CodeGraph — Gemini

This repository has a **CodeGraph** knowledge graph of its code. Before grepping
or reading files broadly, query the graph — it is faster and surfaces
relationships (calls, imports, inheritance, impact).

## Build / refresh
- `codegraph extract .` — build the graph into `codegraph-out/`.
- `codegraph update <changed files>` — incremental rebuild after edits.

## Query
- `codegraph query "<question>"` — the relevant subgraph for a question.
- `codegraph explain <node>` — a node and its neighbours.
- `codegraph path <a> <b>` — shortest path between two nodes.
- `codegraph affected <node>` — what (transitively) depends on a node.

## MCP (preferred for Gemini)
Run `codegraph serve` and use the MCP tools: `query_graph`, `get_node`,
`get_neighbors`, `get_community`, `god_nodes`, `graph_stats`, `shortest_path`,
plus the PR tools `list_prs` / `get_pr_impact` / `triage_prs`.

Reach for the graph on "what calls X", "what breaks if I change Y", and "how does
A reach B" — don't reconstruct those by reading files.
