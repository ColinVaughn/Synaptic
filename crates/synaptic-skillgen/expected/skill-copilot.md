---
name: synaptic
description: Queries this repo's Synaptic code knowledge graph -- symbols and how they call, import, inherit, and (cross-language) reach each other. Use when exploring an unfamiliar codebase; finding what calls or depends on a symbol (callers/callees/dependents); judging the blast radius of a change; deciding whether a "0 dependents" answer is trustworthy when code dispatches dynamically (reflection / event buses); forecasting what a planned edit breaks, which tests to run, and verifying it by running them; reading a symbol's source; or searching the source for a string literal / config value / log message / TODO with each hit attributed to its enclosing symbol. Prefer it over grepping or reading files broadly. Also does structural pattern search, plan-only refactors, time-travel diffs, and SQL audit.
---

# Synaptic for GitHub Copilot

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

## Capabilities (CLI command -- MCP tool)
Prefer the MCP tools when GitHub Copilot has the **synaptic** server connected; the CLI
is the fallback. A "--" in a column means that side has no direct equivalent.

| Goal | CLI | MCP tool |
|---|---|---|
| Relevant subgraph for a question | `synaptic query "<q>"` | `query_graph` (start here; terse, `full=true` for the subgraph) |
| Read a symbol's code (or a `file`+`lines` range) | -- | `get_source` |
| A node + its neighbours / detail | `synaptic explain <node>` | `get_neighbors`, `get_node`, `describe_node` |
| Shortest path between two nodes | `synaptic path <a> <b>` | `shortest_path` |
| Who calls a symbol / what it calls | -- | `find_callers` / `find_callees` (`show_sites=true` for call-site lines) |
| Find all references / uses of a symbol (a type's imports, inheritance, type uses) | `synaptic references <node>` | `find_references` (superset of callers; use for a type/interface) |
| Blast radius of editing a symbol | `synaptic affected <node>` | `affected`; `working_changes_impact` for your git diff |
| Reflection / dynamic-dispatch sites | `synaptic hazards` | `dynamic_hazards` |
| Forecast a change before editing | `synaptic predict [<files>]` | `predict_impact`; `affected_tests` for just the tests |
| Forecast a described (unwritten) edit | `synaptic predict --edit "<kind>:<sym>"` | `predict_edit` |
| Run the change for real in a throwaway worktree | `synaptic speculate [<files>]` | `speculate` (MCP only with `--allow-exec`) |
| Structural / pattern search (SYNQL, not text) | `synaptic search "<synql>"` | `structural_search` |
| List every symbol defined in a file (outline) | `synaptic search --file <path>` | `structural_search` (`file` param) |
| Content (regex/literal) search, hit -> enclosing symbol | -- | `search_text` (not a shell grep -- string literals/config/log/TODO; pivot to `affected`) |
| Graph overview / hubs / clusters | -- | `graph_stats`, `god_nodes`, `get_community` |
| Architecture diff between two git revs | `synaptic diff <rev1> [rev2]` | `time_travel_diff` |
| Plan-only rename (never edits source) | `synaptic refactor rename <name> --to <new>` | `plan_rename` |
| Audit / critique SQL | -- | `audit_sql` / `advise_sql` |
| Graph-aware PR review (needs `gh`) | -- | `list_prs` / `get_pr_impact` / `triage_prs` |

Reference MCP tools with your client's prefix (Claude Code:
`mcp__synaptic__query_graph`); the server's `initialize` reply orients you and each
tool documents its own parameters. If the server is not connected, start it with
`synaptic serve`.

Reach for the graph on "what calls X", "what breaks if I change Y", "how does A
reach B", and to read a symbol's code -- don't reconstruct those by reading files.
Pin a name shared by several files with a `name@file` qualifier (e.g.
`announce@core/foo.ts`); an ambiguous name lists each candidate with its file and
degree. Impact crosses language boundaries (PyO3/FFI, HTTP/gRPC, subprocess, event
buses, Electron IPC are all graph edges), and a 0-dependent symbol reached only by
reflection or dynamic dispatch is flagged, not assumed safe -- see the server's
`initialize` instructions and the `affected` / `dynamic_hazards` tool docs. Before
editing a symbol other code depends on, run `predict_impact` (or `synaptic
predict`) and the checks it lists.

## Verify before you commit
Before committing, ground the judgment in graph evidence, not a re-read of your
diff:
1. `synaptic predict <files>` (or `--edit "<kind>:<symbol>"` for an unwritten edit)
   -- blast radius, at-risk public APIs and tests, and a verify checklist.
2. `synaptic speculate <files>` -- runs the at-risk tests + build in a throwaway
   worktree for real pass/fail.
3. Judge safety from the diff + forecast + speculate result only; a fresh-context
   subagent that sees just those three catches breakage self-review misses.
