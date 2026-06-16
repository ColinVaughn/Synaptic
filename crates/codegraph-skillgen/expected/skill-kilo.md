---
name: codegraph
description: Queries this repo's CodeGraph knowledge graph (symbols and their calls, imports, and inheritance) instead of grepping or reading files. Use when exploring an unfamiliar codebase, finding what calls or depends on a symbol, tracing how one part reaches another, reading a symbol's source, or judging the blast radius of a change.
---

# CodeGraph for Kilo Code

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
- `codegraph search "<cgql>"` / `--pattern <name>`: structural search (CGQL) by
  kind/visibility/loc/fan-in-out, variable-length paths, and `count(...)`
  aggregation + named patterns (singleton, factory, observer, service-locator,
  god-class). Not text search. `.name` is the bare symbol (no `()`); use `=~` for
  a regex/substring match. `--explain` shows the plan; `--save`/`--saved` store
  queries.
- `codegraph diff <rev1> [rev2]` (or `--since <date>`): how the graph changed
  between two git revisions (new/removed dependencies, removed APIs, drift, new
  cycles, hotspots); `--html` writes a report.
- `codegraph refactor rename <name> --to <new>` (also `move`/`extract`): a
  confidence-scored plan (plan.json + plan.md) for you to apply; CodeGraph never
  edits source. Then `codegraph refactor verify --plan <plan.json>` checks the
  graph after you edit.

## MCP (preferred for Kilo Code)
Use the **codegraph** MCP server's tools. Start with `query_graph`, then:
- `get_source` -- read a symbol's actual code (no need to open the file).
- `affected` -- the blast radius of changing a symbol; `working_changes_impact`
  does the same for your current git diff (no PR needed).
- `find_callers` / `find_callees` -- who calls a symbol / what it calls.
- `get_neighbors`, `shortest_path`, `god_nodes`, `graph_stats`, `get_node`,
  `get_community` -- navigate and inspect the graph.
- `structural_search` -- CGQL or a named pattern (kind/loc/fan-in-out, not text).
- `time_travel_diff` -- how the graph changed between two git revisions.
- `plan_rename` -- a plan-only, confidence-scored rename plan (never edits;
  apply it, then `codegraph refactor verify` on the CLI).
- `list_prs` / `get_pr_impact` / `triage_prs` -- graph-aware PR review (need `gh`).

Reference them with your client's MCP prefix (Claude Code:
`mcp__codegraph__query_graph`). The server's `initialize` reply describes the
toolset, and each tool documents its parameters. If the server is not already
connected, start it with `codegraph serve`.

Reach for the graph on "what calls X", "what breaks if I change Y", "how does A
reach B", and to read a symbol's code. Don't reconstruct those by reading files.
