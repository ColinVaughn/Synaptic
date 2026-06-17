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
- `codegraph predict [<files>...]` (or `--base <rev>`): forecast a change BEFORE
  you make it -- the graph nodes the changed files define, the reverse-impact
  blast radius that depends on them, which edited symbols are public API, the
  tests that exercise the code, new import cycles, and a verify checklist
  (forecast.json + forecast.md). Run it before editing a symbol other code
  depends on.
- `codegraph predict --edit "<kind>:<symbol>"`: analytic mode -- forecast a
  DESCRIBED edit (kind = delete, signature, or visibility) before writing any
  code. Reports the predicted graph delta (node and edges removed, public API
  removed) and which dependents will break vs need review.
- `codegraph speculate [<files>...]`: the ground-truth check -- apply your pending
  change in a throwaway git worktree and actually RUN the forecast's at-risk tests
  plus a build/type-check, reporting real pass/fail (report.json + report.md). It
  is disposable and never touches your working tree. Because it executes commands
  it is NOT part of the read-only MCP surface by default; it appears as the MCP
  `speculate` tool only when the server is started with `--allow-exec`, otherwise
  run it here on the CLI.

## MCP (preferred for Kilo Code)
Use the **codegraph** MCP server's tools. Start with `query_graph`, then:
- `get_source` -- read a symbol's actual code (no need to open the file).
- `affected` -- the blast radius of changing a symbol; `working_changes_impact`
  does the same for your current git diff (no PR needed).
- `predict_impact` -- forecast a change before you make it: pass the files you
  are about to edit (or omit them for your current diff) to get the blast radius,
  public APIs at risk, the tests that exercise it, and a verify checklist. Reach
  for it before editing.
- `affected_tests` -- predictive test selection: the tests that exercise the code
  you are about to change. Run those before and after editing.
- `predict_edit` -- what breaks if you delete / change the signature of / make
  private a symbol (classified into "will break" vs "to review").
- `speculate` (present only when the server runs with `--allow-exec`) -- run the
  change for real in a throwaway worktree and report actual test/build pass/fail.
  Off by default because it executes commands; otherwise use the CLI `speculate`.
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
Before editing a symbol other code depends on, forecast the change with
`predict_impact` (or `codegraph predict`) and run the checks it lists.

## Verify a change before you commit (grounded review)
A change can look correct and still break callers or tests. Don't rely on
re-reading your own diff -- ground the judgment in two signals the graph gives you
for free:
1. Forecast it: `codegraph predict <files>` for the blast radius, the public APIs
   at risk, the at-risk tests, the risk score, and a verify checklist. For an edit
   you have only described (not yet written), use `codegraph predict --edit
   "<kind>:<symbol>"`.
2. Confirm it for real: `codegraph speculate <files>` applies the change in a
   disposable worktree and actually runs those at-risk tests plus a build/type-
   check, so you see real pass/fail instead of a guess.
3. Judge safety from three inputs only -- the diff, the forecast, and the speculate
   result -- not from re-reading the implementation. A fresh-context reviewer (a
   subagent that sees just those three) catches breakage that self-review misses,
   because its verdict is grounded in graph and sandbox evidence rather than in the
   same reasoning that produced the change.
