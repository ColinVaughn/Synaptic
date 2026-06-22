# Commands

Complete reference for the `synaptic` CLI. Every command is a subcommand of `synaptic` (for example `synaptic extract`, `synaptic query "..."`). Run `synaptic --help` for the generated summary, or `synaptic <command> --help` for a single command.

Most read commands operate on `synaptic-out/graph.json` by default; build it first with [`extract`](#extract). See [Quickstart](Quickstart) for an end-to-end walkthrough.

## Summary

| Command | Purpose |
| --- | --- |
| [`extract`](#extract) | Build the graph for a directory and write `synaptic-out/`. |
| [`export`](#export) | Re-emit an output format from an existing `graph.json` (no re-extraction), or push live to a database. |
| [`query`](#query) | Find a relevant subgraph for a free-text query. |
| [`search`](#search) | Structural search (SYNQL) and named architectural patterns. Not text search. |
| [`path`](#path) | Shortest path between two nodes. |
| [`explain`](#explain) | Show a node and its neighbours. |
| [`update`](#update) | Incrementally rebuild after files change (or fully with `--full`). |
| [`watch`](#watch) | Watch the working tree and rebuild on change (debounced). |
| [`affected`](#affected) | Nodes that transitively depend on a node (reverse-impact). |
| [`diff`](#diff) | Time-travel: diff the graph between two git revisions (dependencies, removed APIs, drift, cycles, hotspots). |
| [`predict`](#predict) | Forecast a change before applying it: blast radius, public APIs at risk, new cycles, and a verify checklist. |
| [`sql`](#sql) | Audit SQL for performance + security over the SQL-aware graph, or advise on a candidate query before writing it. |
| [`hook`](#hook) | Manage git hooks and the `graph.json` merge driver. |
| [`serve`](#serve) | Run the MCP server (stdio or HTTP). |
| [`ingest`](#ingest) | Ingest an external source into the graph, or fetch a URL for the next extract. |
| [`install`](#install) | Install the Synaptic skill for a host assistant. |
| [`uninstall`](#uninstall) | Remove the Synaptic skill for a platform (or all). |
| [`prs`](#prs) | Graph-aware PR dashboard and per-PR detail. |
| [`skill`](#skill) | Maintain the generated skill artifacts (dev/CI). |
| [`workspace`](#workspace) | Multi-repo / monorepo federation. |
| [`global`](#global) | Manage the cross-repo global graph store (`~/.synaptic`). |
| [`merge-graphs`](#merge-graphs) | Compose several `graph.json` files into one namespaced graph. |
| [`cache`](#cache) | Maintain the on-disk extraction cache. |
| [`self-update`](#self-update) | Update the binary from the latest GitHub release (opt-in). |

There is also an internal `merge-driver` command. It is hidden from `--help` and invoked by git, not users; see [`hook`](#hook).

## extract

Scan a directory, build the knowledge graph, and write the artifact set to `synaptic-out/`.

Syntax:

```sh
synaptic extract [PATH] [--directed] [--obsidian] [--wiki] [--semantic] [--no-columns]
```

Arguments and flags:

| Name | Default | Description |
| --- | --- | --- |
| `PATH` | `.` | Root directory to scan. |
| `--directed` | off | Produce a directed graph. |
| `--obsidian` | off | Also write an Obsidian vault (one note per node) under `synaptic-out/obsidian/`. |
| `--wiki` | off | Also write a Markdown wiki under `synaptic-out/wiki/`. |
| `--semantic` | off | Run the LLM semantic pass over documents/papers and enable the LLM dedup tiebreaker. Requires an API key in the environment (for example `OPENAI_API_KEY`). Makes paid API calls. |
| `--no-columns` | off | Skip SQL column and index nodes. Smaller `graph.json` on column-heavy schemas, at the cost of column-level SQL audit rules. |

The default run is fully offline and needs no API key. It always writes `graph.json`, `graph.html`, `GRAPH_REPORT.md`, `graph.graphml`, `graph.cypher`, `graph.dot`, `callflow.html`, `tree.html`, `graph.svg`, and `graph-3d.html` into `synaptic-out/`. With `--obsidian` and `--wiki` it adds the `obsidian/` and `wiki/` directories. Markdown heading structure is always extracted; the LLM concept pass runs only with `--semantic`.

Example:

```sh
synaptic extract . --directed
```

See [Extraction](Extraction), [Output-Formats](Output-Formats), [Semantic-Analysis](Semantic-Analysis), and [Visualizations](Visualizations).

## export

Regenerate one output format from an existing `graph.json` without re-extracting, or push the graph live to a database.

Syntax:

```sh
synaptic export <FORMAT> [--graph <PATH>] [--out <PATH>] [--repo <TAG>] [--push <URI>] [--user <USER>] [--password <PW>]
```

Arguments and flags:

| Name | Default | Description |
| --- | --- | --- |
| `FORMAT` | required | One of: `json`, `html`, `svg`, `graphml`, `cypher`, `dot`, `callflow` (alias `callflow-html`), `tree`, `3d` (alias `force3d`), `obsidian`, `wiki`, `report`, `neo4j`, `falkordb`. |
| `--graph` | `synaptic-out/graph.json` | Source `graph.json`. |
| `--out` | alongside the source `graph.json` | Output file or directory. |
| `--repo` | none | Scope to one federated member (its `repo` tag) before exporting. |
| `--push` | none | For `neo4j`/`falkordb`: push live to this URI (for example `bolt://localhost:7687` or `falkordb://localhost:6379`) instead of writing the cypher script. Requires building with `--features push`. |
| `--user` | `neo4j` | Auth user for `--push` (Neo4j). |
| `--password` | none | Auth password for `--push` (or set `NEO4J_PASSWORD` / `FALKORDB_PASSWORD`). |

Notes: `report` recomputes communities and analysis from the loaded graph. `export json --repo X` with no `--out` is rejected, because the default output name would overwrite the source `graph.json` with a scoped subgraph; pass `--out` to write the scoped graph elsewhere. Live `--push` requires a build with `--features push`; otherwise `neo4j`/`falkordb` write the cypher script.

Example:

```sh
synaptic export svg --out diagram.svg
synaptic export neo4j --push bolt://localhost:7687 --password secret
```

See [Output-Formats](Output-Formats) and [Workspaces-and-Federation](Workspaces-and-Federation).

## query

Find a relevant subgraph for a free-text query.

Syntax:

```sh
synaptic query <TEXT> [--graph <PATH>] [--max-nodes <N>] [--repo <TAG>] [--dfs]
```

Arguments and flags:

| Name | Default | Description |
| --- | --- | --- |
| `TEXT` | required | Free-text query. |
| `--graph` | `synaptic-out/graph.json` | Source graph. |
| `--max-nodes` | `30` | Maximum nodes in the returned subgraph. |
| `--repo` | none | Scope to one federated member (its `repo` tag). |
| `--dfs` | off | Expand the subgraph depth-first instead of breadth-first (favors deep call chains over broad neighbourhoods). |

Prints the matched seed nodes and the resulting subgraph (nodes and labeled edges). If nothing matches, it reports no matches.

Example:

```sh
synaptic query "auth token refresh" --max-nodes 50
```

See [Querying](Querying).

## path

Shortest path between two nodes, each given by id or label.

Syntax:

```sh
synaptic path <FROM> <TO> [--graph <PATH>] [--repo <TAG>]
```

| Name | Default | Description |
| --- | --- | --- |
| `FROM` | required | Source node id or label. |
| `TO` | required | Target node id or label. |
| `--graph` | `synaptic-out/graph.json` | Source graph. |
| `--repo` | none | Scope to one federated member (its `repo` tag). |

Prints the path as labels joined by arrows, or a message if one or both endpoints cannot be resolved or no path exists.

Example:

```sh
synaptic path "LoginController" "Database"
```

See [Querying](Querying).

## explain

Show a node and its immediate neighbours.

Syntax:

```sh
synaptic explain <NODE> [--graph <PATH>] [--repo <TAG>]
```

| Name | Default | Description |
| --- | --- | --- |
| `NODE` | required | Node id or label. |
| `--graph` | `synaptic-out/graph.json` | Source graph. |
| `--repo` | none | Scope to one federated member (its `repo` tag). |

Prints the node's label, source file, community (if assigned), and each neighbour with the edge direction and relation.

When a name is shared by several files, pin it with a `name@file-substring`
qualifier (e.g. `announce@core/foo.ts`). This works for every command that takes a
node name (`explain`, `path`, `affected`, and `predict --edit`). If the name is
still ambiguous, the candidate list shows each candidate's id, file, and degree
inline.

Example:

```sh
synaptic explain "PaymentService"
synaptic explain "announce@core/foo.ts"   # qualify a name shared by several files
```

See [Querying](Querying).

## search

Structural search over the graph with **SYNQL** (a small Cypher-inspired query
language), or a built-in architectural pattern. This is not text search: it
matches on structure (kind, visibility, lines-of-code, fan-in/out, degree,
community) and relationships.

Syntax:

```sh
synaptic search [QUERY] [--pattern <NAME>] [--list-patterns]
                 [--explain] [--save <NAME>] [--saved <NAME>] [--list-saved]
                 [--graph <PATH>] [--repo <TAG>] [--json] [--limit <N>]
```

| Name | Default | Description |
| --- | --- | --- |
| `QUERY` | none | A SYNQL query (omit when using `--pattern`/`--saved`/`--list-patterns`). |
| `--pattern` | none | Run a built-in pattern instead of a query. |
| `--list-patterns` | off | List the built-in patterns and exit. |
| `--explain` | off | Print the query plan (scan, joins, filter, project/aggregate) without running it. |
| `--save` | none | Save the given query under a name (`synaptic-out/synql/<name>.synql`). |
| `--saved` | none | Run a previously saved query by name. |
| `--list-saved` | off | List saved query names and exit. |
| `--graph` | `synaptic-out/graph.json` | Source graph. |
| `--repo` | none | Scope to one federated member (its `repo` tag). |
| `--json` | off | Emit results as JSON. |
| `--limit` | `50` | Max rows to display. |

### SYNQL

```
MATCH pattern [WHERE expr] RETURN items [LIMIT n]
```

- `pattern` is `(var:kind)` node patterns joined by relationships
  `-[:rel]->`, `<-[:rel]-`, or `-[:rel]-` (either direction). The `:kind` and
  relationship name are optional.
- **Variable-length paths:** a relationship may repeat: `-[:calls*1..3]->`
  (1 to 3 hops), `*` (1 to 8), `*2` (exactly 2), `*2..` / `*..3`. Bounded by a
  cap so a cycle always terminates; matches by reachability within the range.
- Node properties usable in `WHERE` / comparisons: `kind`, `name`, `file`,
  `lang`, `visibility`, `loc`, `fan_in`, `fan_out`, `degree`, `community`.
- Operators: `=`, `!=`, `<`, `<=`, `>`, `>=`, and `=~` (regex on a string
  property); combine with `AND` / `OR` / `NOT` and parentheses.
- **RETURN** takes bound variables (`RETURN a, b`), or an **aggregation**:
  `RETURN c.community, count(c)` groups by the property and counts per group;
  `count(*)` totals. (A `var.field` projection returns the distinct values.)

Evaluation is lenient: a comparison against a missing property (e.g. `loc` on a
node with no span, or any property on a graph built before metadata enrichment)
simply does not match, rather than erroring.

Examples:

```sh
synaptic search "MATCH (c:class) WHERE c.loc > 500 AND c.fan_out > 20 RETURN c"
synaptic search 'MATCH (c:struct) WHERE c.name =~ "Extractor$" RETURN c'
synaptic search "MATCH (a:class)-[:implements]->(b:interface) RETURN a, b"
synaptic search "MATCH (a)-[:calls*1..3]->(b) RETURN a, b"   # transitive callers
synaptic search "MATCH (c:class) RETURN c.community, count(c)" # group + count
synaptic search "MATCH (c:class) WHERE c.loc > 500 RETURN c" --explain
synaptic search "MATCH (c:class) RETURN c" --save big_classes
synaptic search --saved big_classes
```

### Named patterns

`synaptic search --list-patterns` lists them; `--pattern <name>` runs one:

| Pattern | Matches |
| --- | --- |
| `god-class` | Classes over 500 LOC with more than 20 outgoing dependencies. |
| `singleton` | Classes that hold or return an instance of their own type. |
| `factory` | Functions/methods returning an abstract type with 2+ implementations. |
| `observer` | Subject classes holding a field of an interface implemented by 2+ types. |
| `service-locator` | Classes accessed from 3+ distinct communities. |

The patterns are structural heuristics over the enriched graph; their precision
depends on how much `kind`/`visibility` a language extractor supplies (see
[Extraction]). `service-locator` requires community assignments (build with
`synaptic extract`).

```sh
synaptic search --pattern god-class
synaptic search --pattern singleton --json
```

See [Querying](Querying), [Extraction](Extraction), and [Analysis-and-Reports](Analysis-and-Reports).

## update

Incrementally rebuild the graph after files change, or do a full rebuild.

Syntax:

```sh
synaptic update [PATHS...] [--full] [--directed] [--force]
```

| Name | Default | Description |
| --- | --- | --- |
| `PATHS...` | none | Changed file paths (repo-relative). Empty plus no `--full` triggers a full rebuild. |
| `--full` | off | Rebuild every code file from scratch (preserves semantic nodes). |
| `--directed` | off | Build directed when there is no existing graph to inherit from. |
| `--force` | off | Bypass the shrink guard. |

Behavior: when no paths are given on the command line (and not `--full`), changed paths are read from the `SYNAPTIC_CHANGED` environment variable (newline-delimited), which is how the post-commit hook passes them. The command inherits the existing graph and its `directed` flag when present, serializes concurrent rebuilds with a lock (queuing paths if another rebuild holds it), and writes the same artifact set as [`extract`](#extract).

Example:

```sh
synaptic update src/auth.rs src/db.rs
synaptic update --full
```

See [Incremental-Updates](Incremental-Updates).

## watch

Watch the working tree and rebuild incrementally on change, debounced.

Syntax:

```sh
synaptic watch [--directed] [--force]
```

| Name | Default | Description |
| --- | --- | --- |
| `--directed` | off | Build directed when there is no existing graph to inherit from. |
| `--force` | off | Bypass the shrink guard on each rebuild. |

Watches the current directory recursively, debounces a burst of saves into one rebuild, ignores the output/VCS/build subtrees (so writing `graph.json` cannot self-trigger), and rebuilds on changed code and Markdown (`.md`, `.mdx`, `.qmd`) files. Stop with Ctrl-C.

Example:

```sh
synaptic watch --directed
```

See [Incremental-Updates](Incremental-Updates).

## affected

List nodes that transitively depend on a node (reverse-impact analysis).

Syntax:

```sh
synaptic affected <NODE> [--graph <PATH>] [--depth <N>] [--relation <REL>]...
```

| Name | Default | Description |
| --- | --- | --- |
| `NODE` | required | Node id, label, bare name, source file, or unique label substring. |
| `--graph` | `synaptic-out/graph.json` | Source graph. |
| `--depth` | `2` | Max hops to walk backward. |
| `--relation` | structural impact relations | Restrict to these edge relations; repeatable. |

When no `--relation` is given, the default structural impact relations are: `calls`, `references`, `imports`, `imports_from`, `re_exports`, `inherits`, `extends`, `implements`, `uses`, `mixes_in`, `embeds`, `depends_on`, `reads_from`, plus the cross-language relations `invokes`, `binds_native`, `calls_service`, and `handled_by` (so impact spans subprocess/FFI/HTTP/gRPC boundaries; see [Cross-Language-Edges](Cross-Language-Edges)). Containment relations (such as `contains`/`method`) are intentionally excluded. Output lists each affected node with the relation it was reached through and its source location.

Example:

```sh
synaptic affected src/config.rs --depth 3
synaptic affected "User" --relation calls --relation references
```

See [Querying](Querying) and [Analysis-and-Reports](Analysis-and-Reports).

## diff

Time-travel: compare the code graph at two git revisions and report what changed architecturally.

Syntax:

```sh
synaptic diff [REV1] [REV2] [--since <DATE>] [--root <DIR>] [--directed] [--scope <PREFIX>] [--top <N>] [--module-depth <N>] [--json] [--report <PATH>] [--html <PATH>] [--no-cache]
```

| Name | Default | Description |
| --- | --- | --- |
| `REV1` | required* | Base revision (for example `HEAD~10`, a branch, or a SHA). *Optional when `--since` is given. |
| `REV2` | working tree | Target revision; omit to compare the base against the current working tree. |
| `--since` | none | Resolve the base from a date (latest commit on HEAD at or before it, by commit date). Mutually exclusive with `REV1`. |
| `--root` | `.` | Repo root. |
| `--directed` | off | Build a directed graph for each revision. |
| `--scope` | none | Limit reports to source files under this repo-relative path prefix. |
| `--top` | `20` | Max rows per ranked section. |
| `--module-depth` | `2` | Path-component depth defining a "module" (for example `2` => `crates/foo`). |
| `--json` | off | Emit the full report as JSON. |
| `--report` | none | Also write a Markdown report to this path. |
| `--html` | none | Also write a self-contained, theme-aware HTML report to this path. |
| `--no-cache` | off | Always rebuild; skip the per-commit snapshot store. |

Each revision is materialized into a throwaway `git worktree` and built with the same pipeline as [`extract`](#extract) (nothing is written into your working tree). Built graphs are cached per commit SHA under `synaptic-out/history/`, so the first diff of a cold repo builds two full graphs and later diffs of the same commits are near-instant. The report has five sections:

- **Added / removed dependencies** — module-to-module dependency edges that appeared or disappeared.
- **Removed APIs** — code symbols that were referenced from another file and are now gone (an export-surface heuristic).
- **Architectural drift** — change in module coupling (cross-module edge fraction) overall and per module, plus community count.
- **New cycles** — dependency cycles present in `REV2` but not in `REV1`.
- **Hotspots of change** — files ranked by line churn (`git diff --numstat`) plus graph node churn.

Example:

```sh
synaptic diff HEAD~10 HEAD
synaptic diff v1.2.0 main --report drift.md
synaptic diff HEAD --scope crates/auth --json
synaptic diff --since 2026-01-01 --html drift.html
```

See [Analysis-and-Reports](Analysis-and-Reports) and [Incremental-Updates](Incremental-Updates).

## predict

Forecast the consequences of a change before applying it. Maps the changed files to the graph nodes they define, walks the reverse-impact blast radius that depends on them, flags which edited symbols are public API, and (against a base revision) folds in a time-travel diff for new import cycles, removed APIs, and dependency deltas. Writes `forecast.json` + an agent-readable `forecast.md`. Synaptic never edits source; the forecast is data an agent reads first.

Syntax:

```sh
synaptic predict [PATHS]... [--base <REV>] [--graph <PATH>] [--root <DIR>] [--depth <N>] [--max-hits <N>] [--no-diff] [--gate] [--edit <KIND:SYMBOL>] [--out <DIR>] [--repo <NAME>] [--json]
```

| Name | Default | Description |
| --- | --- | --- |
| `PATHS` | `git diff --name-only <base>` | Repo-relative changed files to forecast. When omitted, derived from the working-tree diff vs `--base`. |
| `--base` | `HEAD` | Base revision the change is measured against (used for the changed-file diff and the time-travel diff). |
| `--graph` | `synaptic-out/graph.json` | Source graph. |
| `--root` | `.` | Repo root for the time-travel diff. |
| `--depth` | `3` | Reverse-impact hop bound. |
| `--max-hits` | `200` | Cap on blast-radius dependents reported. |
| `--no-diff` | off | Skip the git/worktree time-travel diff (faster; no cycle / removed-API detection). |
| `--gate` | off | Exit non-zero if the change introduces a new import cycle or removes a public API (a pre-commit / CI quality gate). Forces the time-travel diff. |
| `--edit` | none | Analytic mode: forecast a *described* edit `<kind>:<symbol>` (kind = `delete`, `signature`, or `visibility`) before any code is written. If the name is shared by several files, qualify it as `<kind>:<name>@<file-substring>` (e.g. `delete:announce@core/foo.ts`). Pure-graph, no git. Writes `editforecast.json` + `editforecast.md` and ignores `--base`/`--gate`. |
| `--out` | `synaptic-out/predict` | Output directory for `forecast.json` + `forecast.md`. |
| `--repo` | none | Scope to one federated member (its `repo` tag). |
| `--json` | off | Print the forecast as JSON to stdout (no files written). |

The forecast has these parts:

- **Change risk** — a heuristic 0..100 score (low / medium / high) from diffusion (blast-radius size), size (git churn), public-API changes, and how often the touched files change in history, with the contributing factors named. Advisory and uncalibrated.
- **Changed nodes** — graph nodes defined in the changed files (what the change edits).
- **Blast radius** — nodes that transitively depend on the changed nodes, deduped to the shallowest hop and labeled with the relation they were reached through.
- **Tests at risk** — the test subset of the blast radius (tests detected by path convention); the tests to run for this change. Predictive test selection on the static graph.
- **Public API at risk** — changed nodes that are public; editing them can break callers outside their file or module.
- **New import cycles / Removed APIs / Dependency delta** — from the time-travel diff of the base against the working tree (omitted with `--no-diff`).
- **Co-change suggestions** — files that historically change together with the changed files (mined from recent git history, ranked by confidence). Catches coupling static analysis misses. Requires a git repo.
- **Verify checklist** — concrete commands to run before and after the change.

The time-travel diff builds the base revision in a throwaway `git worktree` (like [`diff`](#diff)); pass `--no-diff` for a fast, pure-graph forecast that needs no git.

Example:

```sh
synaptic predict src/auth.rs --no-diff
synaptic predict --base main --json
synaptic predict src/config.rs src/db.rs --depth 4
synaptic predict --edit "delete:Service" --json
```

In `--edit` mode the forecast is the predicted graph delta of the described edit: whether the symbol's node disappears, how many edges that severs, whether it removes a public API from external view, and which dependents will break vs need review. It is the analytic, pre-code counterpart to [`speculate`](#speculate) (which confirms a change empirically).

See [`affected`](#affected), [`diff`](#diff), [`speculate`](#speculate), and [MCP-Server](MCP-Server) (the `predict_impact` tool).

## speculate

Speculatively execute a proposed change for real. Applies the change in a throwaway `git worktree`, runs the forecast's at-risk tests plus a build/type-check, and reports the actual pass/fail outcome — the ground-truth half of prediction (the graph narrows *what to check*, the sandbox *confirms* it). The worktree is disposable and your real working tree is never touched. This is an opt-in CLI command and is deliberately **not** an MCP tool (it runs commands, which would break the server's read-only invariant).

Syntax:

```sh
synaptic speculate [PATHS]... [--base <REV>] [--patch <FILE>] [--test-cmd <TMPL>] [--check-cmd <CMD>] [--no-detect] [--depth <N>] [--timeout <SECS>] [--max-tests <N>] [--fail-fast] [--graph <PATH>] [--root <DIR>] [--out <DIR>] [--repo <NAME>] [--json]
```

| Name | Default | Description |
| --- | --- | --- |
| `PATHS` | derived | Repo-relative changed files. Empty: derived from `--patch`, else from `git diff --name-only <base>`. Explicit paths also scope the applied working-tree diff. |
| `--base` | `HEAD` with `--patch`, else the detected default branch | Revision to apply onto and diff against. |
| `--graph` | `synaptic-out/graph.json` | Source graph used to select the at-risk tests. |
| `--patch` | none | Apply this unified-diff file instead of the current working-tree changes (can include new files). |
| `--test-cmd` | auto-detected | Test command template; `{files}` expands to the at-risk test files (run per file). With no placeholder it runs once as a whole suite. |
| `--check-cmd` | auto-detected | Build / type-check command, run once before the tests. |
| `--no-detect` | off | Do not auto-detect commands from project markers (`Cargo.toml`, `go.mod`, `pyproject.toml`, `package.json`). |
| `--depth` | `3` | Reverse-impact hop bound for selecting at-risk tests. |
| `--timeout` | `300` | Per-command wall-clock budget in seconds. |
| `--max-tests` | `20` | Cap on the number of at-risk test files run. |
| `--fail-fast` | off | Stop after the first failing test. |
| `--out` | `synaptic-out/speculate` | Output directory for `report.json` + `report.md`. |
| `--repo` | none | Scope to one federated member (its `repo` tag). |
| `--json` | off | Print the report as JSON to stdout (no files written). |

The command exits non-zero when the change breaks the build or an at-risk test (so it can gate CI or an agent loop); a clean run or a no-op change exits 0. When no test/build command is detected and none is given, it reports the run as inconclusive rather than guessing. Working-tree mode captures the change with `git diff`, which omits untracked files — use `--patch` to speculate a change that adds new files.

The throwaway worktree is a clean checkout, so it has no installed dependencies of its own. Because the worktree lives inside the repo, Node tooling (`npm`, `npx tsc`) resolves `node_modules` upward and finds the parent repo's installed packages, so it works without an install step. Ecosystems that do not resolve dependencies upward (a Python virtualenv, etc.) still need their environment on `PATH` — point `--test-cmd`/`--check-cmd` at it, or run the project's own activation in the command.

Example:

```sh
synaptic speculate src/auth.rs
synaptic speculate --patch change.diff --test-cmd "pytest {files}"
synaptic speculate --base main --max-tests 5 --json
```

See [`predict`](#predict) (forecast the same change without running it) and [`diff`](#diff).

## eval

Calibrate Synaptic's own inference quality. `eval replay` measures
change-forecast quality by replaying git history; `eval cross-language` measures
how grounded the inferred cross-language edges are on a single graph.

`eval replay` re-predicts each commit in a range from the **parent**-state graph (built in a throwaway worktree) and scores the prediction against git ground truth: the tests co-edited in the commit (that already existed at the parent) and the public APIs the time-travel diff reports as removed. It reports pooled recall/precision and blast-radius selectivity, and can gate CI on a recall floor. This turns prediction quality into a regression-testable metric.

### eval replay

Syntax:

```sh
synaptic eval replay [FROM] [--root <DIR>] [--depth <N>] [--max-commits <N>] [--directed] [--min-test-recall <PCT>] [--out <DIR>] [--json]
```

| Name | Default | Description |
| --- | --- | --- |
| `FROM` | `HEAD~10` | Replay the commits after this revision (e.g. `HEAD~20`, a branch, a SHA); evaluates `FROM..HEAD`. |
| `--root` | `.` | Repo root. |
| `--depth` | `3` | Reverse-impact hop bound for each forecast. |
| `--max-commits` | `50` | Cap on the number of commits replayed. |
| `--directed` | off | Build directed graphs for each revision. |
| `--min-test-recall` | none | CI gate: exit non-zero if co-edited test recall is below this percentage. |
| `--out` | `synaptic-out/eval` | Output directory for `report.json` + `report.md`. |
| `--json` | off | Print the report as JSON to stdout (no files written). |

Ground truth is a deterministic proxy (not CI logs or sandbox runs): a co-edited test stands in for a relevant test (co-edited is not the same as failed), and tests **added** in a commit are excluded from the recall denominator because they cannot be predicted from the parent graph. Removed-API recall has signal only on languages whose extractor records visibility, so it is reported as a lower bound and is not used by the gate. Each replayed commit is built in a throwaway `git worktree` (cached per commit), so the first run on a cold repo is slow.

Example:

```sh
synaptic eval replay HEAD~20 --json
synaptic eval replay main --min-test-recall 60   # a CI gate
```

### eval cross-language

Calibrate the [cross-language edge layer](Cross-Language-Edges) (subprocess /
FFI / HTTP / gRPC / PyO3) over a single built graph. These edges are `INFERRED`,
so the calibration reports not just counts but how grounded they are. No git
history is involved.

Syntax:

```sh
synaptic eval cross-language [--graph <PATH>] [--json]
```

| Name | Default | Description |
| --- | --- | --- |
| `--graph` | `synaptic-out/graph.json` | The built graph to calibrate. |
| `--json` | off | Print the full `CrossLanguageReport` as JSON to stdout. |

It prints the per-relation edge counts plus two precision proxies: **service
connectivity** (the fraction of service-boundary nodes that are two-sided, with
both a `calls_service` consumer and a `handled_by` producer) and **invocation
resolution** (the fraction of `invokes` edges whose target resolved to an
in-repo file). Example output:

```
Cross-language calibration: cross-language: 14 edge(s); service boundaries 4/6 two-sided (66%); invocations 0/0 resolved (0%); 0 FFI binding(s)
```

Calibration is advisory: it measures detector precision across releases, it does
not retune anything.

See [`predict`](#predict), [`speculate`](#speculate), and [`diff`](#diff).

## sql

Audit the SQL in the graph for performance and security problems, or critique a
candidate query before it is written. See [SQL Auditing](SQL-Auditing) for the
full rule catalog and the SQL-aware graph model (columns, indexes, RLS policies,
grants, and the code -> table links the rules read).

### sql audit

Run every rule over the SQL-aware graph and write `synaptic-out/sql/findings.json`
+ `audit.md` (or `--json` to stdout). Findings are sorted by severity, each with a
location, the offending object/query, a remediation, and a confidence.

Syntax:

```
synaptic sql audit [--graph <path>] [--root <dir>] [--severity <level>] [--repo <tag>] [--out <dir>] [--json] [--explain --db-url <url>]
```

- `--severity <critical|high|medium|low|info>` keeps only findings at least that severe.
- `--root <dir>` (default `.`) lets the N+1 rule read the call-site source for loops.
- `--explain --db-url <url>` runs a live `EXPLAIN` to confirm sequential scans
  (requires building with `--features live-explain`). It runs `EXPLAIN` only, never
  `EXPLAIN ANALYZE`, so it does not execute your queries.

### sql advise

Critique a single candidate query before writing it: the same perf/security
checks plus a graph cross-reference (does the table exist, is it behind RLS, are
the filtered columns indexed).

Syntax:

```
synaptic sql advise --query "<sql>" [--dialect <postgres|mysql|mssql|sqlite>] [--graph <path>] [--repo <tag>] [--json]
```

The `audit_sql` / `advise_sql` [MCP tools](MCP-Server) expose both to an assistant.

## refactor

Safe refactor: plan a single-symbol rename and verify the graph after an AI agent applies it. Synaptic never edits source itself; it produces an execution plan for the agent (Claude / Codex / Cursor) and then checks invariants.

### refactor rename

Syntax:

```sh
synaptic refactor rename <NAME> --to <NEWNAME> [--id <NODEID>] [--file <SUBSTR>] [--root <DIR>] [--graph <PATH>] [--out <DIR>] [--min-confidence <F>] [--json]
```

| Name | Default | Description |
| --- | --- | --- |
| `NAME` | required | The symbol to rename (its name, or a node id). |
| `--to` | required | The new name. |
| `--id` | none | Disambiguate by node id when the name matches several definitions. |
| `--file` | none | Disambiguate by file-path substring. |
| `--root` | `.` | Repo root; referencing files are read from here for column-accurate sites. |
| `--graph` | `synaptic-out/graph.json` | Graph to plan against. |
| `--out` | `synaptic-out/refactor` | Output directory for the plan. |
| `--min-confidence` | `0.8` | Minimum per-site confidence score `[0,1]` to land in `edits` vs `review`. |
| `--no-text-scan` | off | Skip the whole-word textual scan for references the graph does not record as edges (type uses, enum-variant paths). |
| `--max-text-sites` | `200` | Cap on textual occurrences enumerated by the text scan. |
| `--json` | off | Emit the plan as JSON to stdout. |

The symbol is resolved to a definition node. If the name matches several definitions the command lists the candidates (with `--id` hints) and exits — ambiguity is surfaced, never silently guessed. The plan recovers concrete edit sites: the definition plus every resolved reference. Call sites get a column-accurate span from the AST cache (member calls fall back to line-only); other references (inherits/implements/uses) use the edge line and are flagged for the agent to locate the token. A whole-word textual scan additionally enumerates references the conservative graph does not record as edges (type annotations, enum-variant paths); these land in `review` (disable with `--no-text-scan`). Sites carry a `repo` tag on a federated graph, so a cross-repo rename is surfaced (verify is single-repo in v1).

Each site is scored (`EXTRACTED` / `INFERRED` / `AMBIGUOUS`); low-confidence or ambiguous sites are routed to a `review` list instead of `edits`. A name collision (the new name already exists) is flagged. Two artifacts are written, plus a `before-graph.json` snapshot used by `verify`:

- `plan.json` — machine-readable: target, new name, overall confidence + score, blast radius, ordered `edits`, and a `review` list.
- `plan.md` — agent-readable narrative: definition first, references grouped by file, the review list, and the exact verify command.

Examples:

```sh
synaptic refactor rename UserService --to AccountService
synaptic refactor rename User --to Account --file models/   # disambiguate
synaptic refactor rename Confidence --to Trust --id src_confidence_confidence --json
```

### refactor move / extract

Relocate a symbol's definition to another module. `move` targets an existing file; `extract` a new one. The symbol name is unchanged — what changes is where it lives and the imports that reach it.

```sh
synaptic refactor move <NAME> --to <FILE> [--id <ID>] [--file <SUBSTR>] [--root <DIR>] [--graph <PATH>] [--out <DIR>] [--json]
synaptic refactor extract <NAME> --to <NEWFILE> [ ...same flags... ]
```

The plan identifies the definition block to cut (its span), the destination, one import-update site per referencing file, the resolved usages for context, and a destination name collision if any. Verify with `--relocate`.

```sh
synaptic refactor move parse_config --to crates/core/src/config.rs
synaptic refactor extract Helper --to src/helpers.rs
```

### refactor verify

Syntax:

```sh
synaptic refactor verify --plan <PLAN.JSON> [--root <DIR>] [--relocate] [--json]
```

Run after the agent applies the plan's edits. It rebuilds the current source and checks invariants against the pre-edit snapshot, exiting non-zero on failure. For a **rename**:

- **definition-renamed** — the old definition is gone and the new one exists at the target's file (matched by full path + kind).
- **references-preserved** — every file that referenced the old symbol still references the renamed one (compares the set of referencing files, naming any that went missing).
- **no-lost-nodes** — no located code nodes were dropped (guards against an accidental deletion during the rename).
- **no-new-cycles** — no dependency cycle exists that was absent before.

For a **move/extract** (`--relocate`): **definition-relocated** (the definition now lives in the destination file, not the old one), **references-preserved**, and **no-new-cycles**.

Scope (v1): a single named symbol; rename/move/extract. Cross-repo is surfaced in the plan but verify rebuilds a single repo. No signature changes. The plan format is designed to extend further.

See [Analysis-and-Reports](Analysis-and-Reports).

## hook

Manage git hooks (post-commit/post-checkout) and the `graph.json` merge driver.

Syntax:

```sh
synaptic hook <ACTION>
```

Actions:

| Action | Description |
| --- | --- |
| `install` | Install the hooks and register the merge driver (idempotent). |
| `uninstall` | Remove the hooks (and the Synaptic blocks from any shared hook files). |
| `status` | Show which hooks are currently installed. |

Each action prints the per-hook installed/not-installed state. The hooks keep `graph.json` current after commits and checkouts; the merge driver union-composes both sides so `graph.json` never conflicts during merges.

Example:

```sh
synaptic hook install
synaptic hook status
```

See [Incremental-Updates](Incremental-Updates).

## serve

Run the MCP server exposing read-only graph tools (and PR tools) to an AI assistant.

Syntax:

```sh
synaptic serve [--graph <PATH>] [--http <ADDR>] [--api-key <KEY>] [--source-root <DIR>] [--allow-exec]
```

| Name | Default | Description |
| --- | --- | --- |
| `--graph` | `synaptic-out/graph.json` | Graph to serve. |
| `--http` | none (stdio) | Serve over HTTP at this address (for example `127.0.0.1:8765`) instead of stdio. The MCP endpoint is `/mcp`. |
| `--api-key` | none | Require this API key for HTTP requests (or set `SYNAPTIC_API_KEY`). |
| `--source-root` | dir above `synaptic-out/` | Trusted root for resolving a node's source file in the `get_source` tool (path-traversal jailed). |
| `--allow-exec` | off | Expose the command-running `speculate` tool (the 27th tool). This makes the server no longer read-only, so enable it only for trusted clients. See [MCP Server](MCP-Server). |

Defaults to stdio transport. The MCP server reports protocol `2025-11-25` and exposes 26 read-only tools (27 with `--allow-exec`, which adds the command-running `speculate` tool), prompts, completions, resource templates/subscriptions, and structured tool output. When serving HTTP on a wildcard address with no API key, it prints a warning.

Example:

```sh
synaptic serve
synaptic serve --http 127.0.0.1:8765 --api-key secret
```

See [MCP-Server](MCP-Server) and [Assistant-Integration](Assistant-Integration).

## ingest

Ingest an external source into the graph, or fetch a URL into `synaptic-out/ingested/` for the next extract.

Syntax:

```sh
synaptic ingest <SOURCE> [SOURCE-ARGS]
```

Sources:

| Source | Argument | Description |
| --- | --- | --- |
| `cargo` | `<PATH>` | A Cargo workspace root; adds crate nodes and internal-dependency edges. |
| `mcp` | `<FILE>` | An MCP config file (`.mcp.json`, `claude_desktop_config.json`, etc.). |
| `scip` | `<FILE>` | A SCIP-index JSON file (simplified shape); adds symbol nodes and edges. |
| `pg` | `[DSN]` (default empty) | A live PostgreSQL database; adds table/view/function nodes and foreign-key edges. Empty DSN uses `PG*` env vars. Requires `--features pg`. |
| `url` | `<URL>` | A URL, fetched (SSRF-guarded) into `synaptic-out/ingested/`. |
| `office` | `<FILE>` | An office spreadsheet (`.xlsx`/`.xls`/`.ods`), converted to markdown in `synaptic-out/ingested/`. Requires `--features office`. |
| `gws` | `<FILE>` | A Google-Workspace pointer (`.gdoc`/`.gsheet`/`.gslides`), exported to markdown via the `gws` CLI. Requires `--features gws`. |
| `media` | `<FILE>` | A local audio/video file, transcribed to markdown. Requires `--features media`. |

The `cargo`, `mcp`, `scip`, and `pg` sources merge their nodes/edges into `synaptic-out/graph.json` and rewrite the artifacts. The `url`, `office`, `gws`, and `media` sources write into `synaptic-out/ingested/` and require a follow-up `synaptic extract` (or `update`) to index. Feature-gated sources error with a rebuild hint when the feature is not compiled in.

Example:

```sh
synaptic ingest cargo .
synaptic ingest url https://example.com/spec.html
```

See [Ingestion](Ingestion).

## install

Install the Synaptic skill for a host assistant in the current directory.

Syntax:

```sh
synaptic install [PLATFORM] [--global]
synaptic install --refresh
```

| Name | Default | Description |
| --- | --- | --- |
| `PLATFORM` | `claude` | One of: `claude`, `agents`, `codex`, `opencode`, `gemini`, `cursor`, `copilot`, `kilo`. |
| `--global` | off | Codex only: register the MCP server in the global `~/.codex/config.toml` (per-repo server) for the Codex desktop app, instead of the project `.codex/`. |
| `--refresh` | off | Re-render every skill recorded in `~/.synaptic/skills.toml` to the current version (the `PLATFORM` arg is ignored). Hand-edited skills are left untouched. This is what `self-update` runs automatically. See [Assistant Integration](Assistant-Integration#versioning-and-auto-refresh). |

`codex` gets extra wiring: a native MCP server and a `SessionStart` hook (project
`.codex/` by default, or a global per-repo server with `--global` for the desktop
app). Prints the files written. Each install is recorded so a later
`self-update` (or `install --refresh`) can re-render it to the new version.

Examples:

```sh
synaptic install claude
synaptic install codex            # Codex CLI (project .codex/)
synaptic install codex --global   # Codex desktop app (global ~/.codex)
synaptic install --refresh        # update all installed skills to this version
```

See [Assistant-Integration](Assistant-Integration).

## uninstall

Remove the Synaptic skill for a platform.

Syntax:

```sh
synaptic uninstall [PLATFORM] [--all] [--global]
```

| Name | Default | Description |
| --- | --- | --- |
| `PLATFORM` | `claude` | The platform to remove (same set as [`install`](#install)). |
| `--all` | off | Uninstall from every supported platform (ignores `PLATFORM`). |
| `--global` | off | Codex only: remove this repo's server from the global `~/.codex/config.toml`. |

Example:

```sh
synaptic uninstall cursor
synaptic uninstall codex --global
synaptic uninstall --all
```

See [Assistant-Integration](Assistant-Integration).

## prs

Graph-aware PR dashboard, single-PR detail, triage, and conflict views. Requires the `gh` CLI.

Syntax:

```sh
synaptic prs [NUMBER] [--repo <OWNER/NAME>] [--base <BRANCH>] [--graph <PATH>] [--triage] [--conflicts]
```

| Name | Default | Description |
| --- | --- | --- |
| `NUMBER` | none | PR number for a detailed view; omit for the dashboard. |
| `--repo` | current directory's repo | Target repo `owner/name`. |
| `--base` | the repo's default branch | Base branch to filter to. |
| `--graph` | `synaptic-out/graph.json` | Graph used for blast-radius (communities + node count). |
| `--triage` | off | Ranked actionable PRs with blast radius (deterministic; no LLM). |
| `--conflicts` | off | PRs that touch the same graph community (merge-order risk). |

With no number and no flags it prints the open-PR dashboard. A number shows that PR's detail including graph blast radius. `--conflicts` and `--triage` are dashboard-level views and take precedence over a PR number. Graph blast radius is attached only when a `graph.json` is present. For LLM-summarized triage, use the MCP server's `triage_prs` tool.

Example:

```sh
synaptic prs
synaptic prs 142
synaptic prs --triage
```

See [PR-Dashboard](PR-Dashboard).

## skill

Maintain the generated skill artifacts (development/CI). Checks for drift against the committed snapshots, or re-blesses them after an intentional change.

Syntax:

```sh
synaptic skill <ACTION>
```

| Action | Description |
| --- | --- |
| `check` | Re-render the skill artifacts and fail if they differ from the committed `expected/` snapshots (CI anti-drift guard). |
| `bless` | Rewrite the committed `expected/` snapshots from the current render. |

Example:

```sh
synaptic skill check
```

See [Development](Development).

## workspace

Multi-repo / monorepo federation: discover members, build a federated graph, and scope queries to a repo.

Syntax:

```sh
synaptic workspace <ACTION>
```

Actions:

| Action | Syntax | Description |
| --- | --- | --- |
| `init` | `init [--scan-repos [DIR]] [--depth <N>] [--max <N>]` | Write `synaptic-workspace.toml`, auto-discovering members. `--scan-repos` also scans a parent dir for sibling git repos and appends `[[repos]]` entries (bare flag scans the parent of the current repo). `--depth` defaults to `3`, `--max` defaults to `50`. |
| `add` | `add <TARGET>` | Add a member: an existing local path becomes a `members` entry, a git URL becomes a `[[repos]]` entry. |
| `discover` | `discover [PATH] [--depth <N>] [--max <N>]` | Scan a parent dir for sibling git repos and federate them without writing a manifest. `PATH` defaults to the parent of the current repo; `--depth` defaults to `3`, `--max` to `50`. |
| `build` | `build [--changed] [--directed]` | Build all members and federate into `synaptic-out/graph.json`. `--changed` only rebuilds members that changed; `--directed` produces a directed federated graph. |
| `federate` | `federate <DIR>` | Compose from a directory of published `<member>/graph.json` artifacts. |
| `sync` | `sync` | Pull remote git members, then rebuild deltas. |
| `status` | `status` | Show each member's change status (no build). |
| `list` | `list` | List the workspace members (local members and remote repos). |

`build`, `federate`, `discover`, and `sync` write the federated graph artifacts plus a `surfaces/` directory of per-member export surfaces, and print a build summary (node/edge/community counts, cross-repo links, per-member sizes).

Example:

```sh
synaptic workspace init --scan-repos
synaptic workspace build --directed
synaptic workspace status
```

See [Workspaces-and-Federation](Workspaces-and-Federation).

## global

Manage the cross-repo global graph store at `~/.synaptic`.

Syntax:

```sh
synaptic global <ACTION>
```

| Action | Syntax | Description |
| --- | --- | --- |
| `add` | `add <GRAPH> [--as <TAG>]` | Add (or update) a repo's `graph.json` under a tag. The tag defaults to the graph's grandparent directory name. Skipped when the source is unchanged. |
| `remove` | `remove <TAG>` | Remove a repo's nodes from the global store. |
| `list` | `list` | List the repos in the store with node/edge counts and source path. |
| `path` | `path` | Print the global graph path. |

Example:

```sh
synaptic global add ./synaptic-out/graph.json --as backend
synaptic global list
```

See [Workspaces-and-Federation](Workspaces-and-Federation).

## merge-graphs

Compose several `graph.json` files into one namespaced graph.

Syntax:

```sh
synaptic merge-graphs <GRAPHS...> [--out <PATH>]
```

| Name | Default | Description |
| --- | --- | --- |
| `GRAPHS...` | required (at least one) | The `graph.json` files to merge. Each file's tag is its grandparent directory name. |
| `--out` | `synaptic-out/merged-graph.json` | Output path. |

Prints the number of graphs merged, the output path, node/edge totals, and the tags. Errors if no graphs are given.

Example:

```sh
synaptic merge-graphs repo-a/synaptic-out/graph.json repo-b/synaptic-out/graph.json --out merged.json
```

See [Workspaces-and-Federation](Workspaces-and-Federation).

## cache

Maintain the on-disk extraction cache at `synaptic-out/cache`.

Syntax:

```sh
synaptic cache <ACTION>
```

Action `clear`:

```sh
synaptic cache clear [PATH] [--recursive]
```

| Name | Default | Description |
| --- | --- | --- |
| `PATH` | `.` | Repo/workspace root whose `synaptic-out/cache` to remove. |
| `--recursive` | off | Also remove every `synaptic-out/cache` found beneath `PATH` (federated member caches), via a bounded, noise-pruned walk that skips `node_modules` and `.git`. |

The AST cache normally self-invalidates when extractors change; use `clear` for a guaranteed cold start or suspected corruption. Only the regenerable `synaptic-out/cache` subtree is ever removed.

Example:

```sh
synaptic cache clear
synaptic cache clear . --recursive
```

See [Extraction](Extraction).

## self-update

Update the `synaptic` binary in place from the latest [GitHub Release](../../releases). This is **opt-in**: Synaptic never checks for updates or replaces the binary unless you run this command or enable the background notice below. See [Updating](Updating) for the full walkthrough.

Syntax:

```sh
synaptic self-update [--enable | --disable] [--check] [--yes]
```

| Name | Default | Description |
| --- | --- | --- |
| `--enable` | off | Turn on the once-a-day "update available" notice and exit. Writes `~/.synaptic/update.toml`; no network. |
| `--disable` | off | Turn the background notice off and exit. |
| `--check` | off | Report whether a newer release exists, then exit without downloading. |
| `--yes` / `-y` | off | Skip the confirmation prompt before downloading and replacing. |

`--enable` and `--disable` cannot be combined with each other or with `--check`/`--yes` — they only toggle the background notice and exit.

With no flags, `self-update` queries the latest release and compares it to the running version. If it is not newer it prints `Synaptic is up to date (<version>)` and exits. If it is newer it shows the version delta and release notes, prompts `Download and replace the current binary? [y/N]`, then (on confirmation, or with `--yes`) downloads the prebuilt archive for your platform, verifies its SHA-256 checksum when one is published, and atomically replaces the running binary plus its `syn` alias. It then re-renders every installed agent skill recorded in `~/.synaptic/skills.toml` to the new version (hand-edited skills are left untouched; run `synaptic install --refresh` to do this on its own). The new version takes effect on the next invocation.

If no prebuilt binary exists for your platform, `self-update` prints the releases URL and exits without changing anything. A source/`cargo install` build can self-update, but the swap installs the default-feature prebuilt binary (rebuild from source to keep extra Cargo features).

Examples:

```sh
synaptic self-update            # interactive: check, confirm, replace
synaptic self-update --check    # just report availability (scriptable)
synaptic self-update --yes      # unattended update
synaptic self-update --enable   # opt in to the daily reminder
```

The background notice is throttled to once per 24 hours, prints a single line to stderr, swallows network errors, and can be force-disabled with `SYNAPTIC_UPDATE_CHECK=0`. See [Updating](Updating) and [Configuration](Configuration).

## merge-driver (internal)

`synaptic merge-driver <BASE> <CURRENT> <OTHER>` is a git merge driver for `graph.json`. It is hidden from `--help` and invoked by git as `%O %A %B`, not by users. It union-composes both sides into `CURRENT` so `graph.json` never conflicts. It is registered automatically by `synaptic hook install`.
