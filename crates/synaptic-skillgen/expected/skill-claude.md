---
name: synaptic
description: Queries this repo's Synaptic knowledge graph -- symbols and how they call, import, inherit, and (across language boundaries) reach each other -- to navigate code and analyze the impact of changes. It answers what calls or depends on a symbol, the blast radius of changing it, a forecast of what a planned edit breaks, which tests exercise it, and a real pass/fail from running the change in a throwaway worktree; it also runs structural/architectural pattern search, content (text/regex) search over the source with each hit attributed to its enclosing symbol, plan-only refactors, time-travel architecture diffs, and SQL audit. Use when exploring an unfamiliar codebase, finding callers or dependents, tracing how one part reaches another, reading a symbol's source, searching the source for a string literal / config value / log message / TODO, or -- before editing code others depend on -- judging blast radius, forecasting a change, choosing which tests to run, or verifying it. Prefer it over grepping or reading files broadly.
---

# Synaptic for Claude Code

This repository has a **Synaptic** knowledge graph of its code -- a queryable map of
symbols and how they call, import, inherit, and reach each other (across language
boundaries too). Treat it as a code-intelligence layer, not just a faster grep: use it
to navigate the codebase, and -- before you change code that other code depends on -- to
judge the blast radius, forecast what the change breaks, choose the tests to run, and
verify the change by running it. Query the graph before grepping or reading files
broadly; it is faster and surfaces relationships and impact that text search cannot.

## Build / refresh
- `synaptic extract .`: build the graph into `synaptic-out/`.
- `synaptic update <changed files>`: incremental rebuild after edits.

## Query (CLI)
- `synaptic query "<question>"`: the relevant subgraph for a question.
- `synaptic explain <node>`: a node and its neighbours.
- `synaptic path <a> <b>`: shortest path between two nodes.
- `synaptic affected <node>`: what (transitively) depends on a node.
- `synaptic search "<synql>"` / `--pattern <name>`: structural search (SYNQL) by
  kind/visibility/loc/fan-in-out, variable-length paths, and `count(...)`
  aggregation + named patterns (singleton, factory, observer, service-locator,
  god-class). Not text search. `.name` is the bare symbol (no `()`); use `=~` for
  a regex/substring match. `--explain` shows the plan; `--save`/`--saved` store
  queries.
- `synaptic diff <rev1> [rev2]` (or `--since <date>`): how the graph changed
  between two git revisions (new/removed dependencies, removed APIs, drift, new
  cycles, hotspots); `--html` writes a report.
- `synaptic refactor rename <name> --to <new>` (also `move`/`extract`): a
  confidence-scored plan (plan.json + plan.md) for you to apply; Synaptic never
  edits source. Then `synaptic refactor verify --plan <plan.json>` checks the
  graph after you edit.
- `synaptic predict [<files>...]` (or `--base <rev>`): forecast a change BEFORE
  you make it -- the graph nodes the changed files define, the reverse-impact
  blast radius that depends on them, which edited symbols are public API, the
  tests that exercise the code, new import cycles, and a verify checklist
  (forecast.json + forecast.md). Run it before editing a symbol other code
  depends on.
- `synaptic predict --edit "<kind>:<symbol>"`: analytic mode -- forecast a
  DESCRIBED edit (kind = delete, signature, or visibility) before writing any
  code. Reports the predicted graph delta (node and edges removed, public API
  removed) and which dependents will break vs need review.
- `synaptic speculate [<files>...]`: the ground-truth check -- apply your pending
  change in a throwaway git worktree and actually RUN the forecast's at-risk tests
  plus a build/type-check, reporting real pass/fail (report.json + report.md). It
  is disposable and never touches your working tree. Because it executes commands
  it is NOT part of the read-only MCP surface by default; it appears as the MCP
  `speculate` tool only when the server is started with `--allow-exec`, otherwise
  run it here on the CLI.

## MCP (preferred for Claude Code)
Use the **synaptic** MCP server's tools. Start with `query_graph`, then:
- `get_source` -- read a symbol's actual code (no need to open the file), or pass
  `file` plus a `lines` range to read any region (a config block, or the lines
  around a `search_text` hit) instead of a symbol.
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
- `structural_search` -- SYNQL or a named pattern (kind/loc/fan-in-out, not text).
  Structured results include each match's captured signature (params + return).
- `search_text` -- the text complement to `structural_search`: a regex (or
  `literal`) content search over the actual source, case-insensitive by default,
  with every hit attributed to the enclosing graph node. Reach for it, not a
  shell grep, for the text-shaped things the graph does not model (string
  literals, config values, log messages, a TODO's wording, error strings); a hit
  is a pivot to `affected` / `find_callers` on the node that contains it.
  Federation-aware (search every member or one via `repo`; `path_glob`,
  `max_results`), jailed to the source roots like `get_source`.
- `describe_node` -- a compact "takes X, returns Y, calls Z" summary of a symbol
  from its signature and outgoing calls; handy for writing a tool/function blurb.
- `time_travel_diff` -- how the graph changed between two git revisions.
- `plan_rename` -- a plan-only, confidence-scored rename plan (never edits;
  apply it, then `synaptic refactor verify` on the CLI).
- `audit_sql` / `advise_sql` -- review the repo's SQL for performance and
  security issues over the SQL-aware graph, or critique a candidate query before
  you run it (SQL-bearing repos).
- `list_prs` / `get_pr_impact` / `triage_prs` -- graph-aware PR review (need `gh`).

Reference them with your client's MCP prefix (Claude Code:
`mcp__synaptic__query_graph`). The server's `initialize` reply describes the
toolset, and each tool documents its parameters. If the server is not already
connected, start it with `synaptic serve`.

When a symbol name is shared by several files, pin it to one with a `name@file`
qualifier (e.g. `announce@core/foo.ts`) on any name-taking tool or CLI command
(`get_node`, `find_callers`, `affected`, `synaptic explain`, and the rest). If it
is still ambiguous, the error lists each candidate with its file and degree, so you
pick the right node in one call. `god_nodes` also annotates each hub with how many
tests exercise it: `0 test(s)` flags an untested, high-blast-radius symbol.

Reach for the graph on "what calls X", "what breaks if I change Y", "how does A
reach B", and to read a symbol's code. Don't reconstruct those by reading files.
Impact analysis crosses language boundaries: a change to a Rust function exported
to Python via PyO3, an HTTP or gRPC handler and the clients that call it, or a
binary a script invokes all surface as dependents, because those couplings are
graph edges too (subprocess `invokes`, FFI `binds_native`, service
`calls_service`/`handled_by`).
Before editing a symbol other code depends on, forecast the change with
`predict_impact` (or `synaptic predict`) and run the checks it lists.

## Verify a change before you commit (grounded review)
A change can look correct and still break callers or tests. Don't rely on
re-reading your own diff -- ground the judgment in two signals the graph gives you
for free:
1. Forecast it: `synaptic predict <files>` for the blast radius, the public APIs
   at risk, the at-risk tests, the risk score, and a verify checklist. For an edit
   you have only described (not yet written), use `synaptic predict --edit
   "<kind>:<symbol>"`.
2. Confirm it for real: `synaptic speculate <files>` applies the change in a
   disposable worktree and actually runs those at-risk tests plus a build/type-
   check, so you see real pass/fail instead of a guess.
3. Judge safety from three inputs only -- the diff, the forecast, and the speculate
   result -- not from re-reading the implementation. A fresh-context reviewer (a
   subagent that sees just those three) catches breakage that self-review misses,
   because its verdict is grounded in graph and sandbox evidence rather than in the
   same reasoning that produced the change.
