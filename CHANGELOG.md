# Changelog

All notable changes to Synaptic are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> Entries at or before 0.2.12 were released under the project's former name,
> **CodeGraph**, and reference the old `codegraph` command and crate names. They
> are preserved verbatim as historical record.

## [Unreleased]

### Added
- `self-update` command: opt-in self-replacement from the latest GitHub release,
  with SHA-256 checksum verification and a confirmation prompt before the binary
  is swapped (`--yes` to skip, `--check` to report availability only). An opt-in
  background check (`self-update --enable`) prints a one-line "update available"
  notice at most once per day; it is off by default, writes to
  `~/.synaptic/update.toml`, and can be force-disabled with
  `SYNAPTIC_UPDATE_CHECK=0`. Release archives now publish a `.sha256` sidecar.

## [0.3.0] - 2026-06-21

### Changed
- **Project renamed from CodeGraph to Synaptic.** This is a full rebrand and a
  breaking change for existing setups:
  - **Binary:** the CLI is now `synaptic`, with `syn` as a built-in short alias
    (both ship from the same crate). The old `codegraph` binary no longer exists.
  - **Crates:** every `codegraph-*` workspace crate is renamed `synaptic-*`.
  - **Query language:** CGQL is now **SynQL** (Synaptic Query Language); saved
    queries use the `.synql` extension under `synaptic-out/synql/` (was
    `codegraph-out/cgql/`).
  - **Environment variables:** all `CODEGRAPH_*` variables are now `SYNAPTIC_*`
    (e.g. `SYNAPTIC_API_KEY`, `SYNAPTIC_BACKEND`, `SYNAPTIC_QUERY_LOG`). There is
    no fallback to the old names.
  - **Files & dirs:** the default output directory is `synaptic-out` (was
    `codegraph-out`); the ignore file is `.synapticignore` (was `.codegraphignore`).
  - **MCP server:** `serverInfo.name` is now `synaptic`; generated assistant
    skills/configs invoke the `synaptic` binary.
- Migration: rebuild your graph (`synaptic extract .`), rename any committed
  `codegraph-out/` and `.codegraphignore` to their `synaptic` equivalents, and
  update env vars and assistant integrations to the new names.

## [0.2.12] - 2026-06-20

### Fixed
- **CLI `affected` is now bounded, matching the MCP tool.** `codegraph affected` printed
  every dependent (hundreds on a hub). It now leads with a per-depth breakdown
  (`Total: N [depth 1: …, depth 2: …]`), lists the top-N, and appends a
  `... (+N more; pass --verbose for the full list)` note. New `--limit` (default 50) and
  `--verbose` flags control it, mirroring the MCP `affected` parameters added in 0.2.11.
  (The MCP `affected` `limit`/`verbose` from 0.2.11 were already wired; if a client still
  sees a 50-cap with no override, refresh the binary and reconnect so it re-fetches the
  tool list.)

## [0.2.11] - 2026-06-20

Two more agent-tooling fixes from continued re-testing.

### Fixed
- **`affected` output is now bounded.** The 0.2.9 summary+top-N treatment reached
  `predict_impact` and `audit_sql` but not `affected`, the tool most likely aimed at a
  hub node (which could dump hundreds of dependents in one response). It now leads with a
  per-depth breakdown (`[depth 1: 140, depth 2: 160]`), lists the top-N, and appends a
  `... (+N more; pass verbose=true)` note. New optional `limit` (default 50) and `verbose`
  parameters control it; the structured output is capped the same way and adds `total`,
  `truncated`, and `by_depth`.
- **`describe_node` / `structural_search` no longer HTML-escape signatures.** Generics
  came back entity-encoded in the structured channel (`Record&lt;string, unknown&gt;`)
  because signatures were sanitized with the HTML-escaping metadata path meant for
  `graph.json`/HTML viewers. The structured signature is now sanitized with the plain
  label rule (control-strip + length cap, no entity escaping), so `Record<string,
  unknown>` and `Promise<void>` come through verbatim — important since `describe_node`
  feeds tool/function-description generation.

## [0.2.10] - 2026-06-20

Follow-up to 0.2.9, from a re-test on the same TypeScript repo. Four of the five 0.2.9
fixes were confirmed; this release closes the remaining gaps. Requires re-extraction for
the changed-node fix (the config marker is written at extract time).

### Fixed
- **`predict_impact` still listed JSON/YAML config keys as changed nodes:** the 0.2.9
  exclusion only caught markdown headings because the `config_key` marker lived on the
  edge, not the node. Config-key nodes (package.json keys, tsconfig keys, etc.) and
  YAML/k8s/CI resource nodes now carry a node-level `_node_type`, so `is_code_symbol`
  excludes them from the changed-node set in both the MCP `predict_impact` response and the
  CLI `forecast.json`.
- **Verify-checklist example pointed at a config key:** the `codegraph affected "..."`
  example in `predict_impact`'s checklist now prefers a real code symbol (one with a kind)
  instead of whatever node sorts first.
- **CLI `explain` / `path` / `affected` did not share the resolver:** these commands now
  use the same resolver as the MCP tools, so an ambiguous name reports
  `'<name>' is ambiguous - N candidates: [...]` instead of "Node not found", uniformly
  across CLI and MCP.

## [0.2.9] - 2026-06-20

Quality pass on the agent-facing tools, from issues found driving CodeGraph over a real
TypeScript codebase. Backward-compatible: MCP tool count is unchanged (only new optional
parameters), and `graph.json` gains only additive edge keys.

### Fixed
- **SQL audit false positives:** `audit_sql` no longer flags ordinary string literals and
  comments that merely begin with a SQL verb (e.g. `return 'Update password'`). SQL is now
  extracted only when a string carries the companion clause a real statement of its shape
  requires (`UPDATE`->`SET`, `DELETE`/`SELECT`->`FROM`, `INSERT`->`INTO`, `MERGE`->`INTO`/
  `USING`), and the query-text rules additionally drop any snippet that does not parse as
  real SQL (placeholders normalized first, so parameterized queries and `::` casts survive).
- **`predict_edit` / `plan_rename` missed module-level usages:** a symbol used only through a
  module-level import (e.g. a test that does `import { fn } from './mod'` and calls it at top
  level) is now resolved. The reverse-impact walk could not reach it because the import edge
  points at a module stub, not the symbol. Imports now record the names they bring in, and
  the forecast resolves module importers back to the symbol -- named importers are reported as
  "will break" (or a rename edit site), opaque ones as "to review". An exported symbol with
  module importers can no longer report a bare `0 will break, 0 to review`.
- **Node resolution consistency:** all name-taking tools share one resolver. An ambiguous
  name now reports `'<name>' is ambiguous - N candidates: [...]` with candidate ids instead
  of a misleading "No node matches", trailing `()` is stripped consistently, and the wording
  is uniform across `get_node`, `describe_node`, `get_source`, `get_neighbors`,
  `find_callers`/`find_callees`, `affected`, `shortest_path`, and `predict_edit`.
- **`predict_impact` changed-node pollution:** the changed-node list now contains only code
  symbols. Markdown headings and JSON/YAML config keys living in a changed file are excluded,
  so the count and output are no longer inflated by non-code nodes.

### Changed
- **Bounded tool output:** `predict_impact` and `audit_sql` default to a summary plus a
  top-N view, with new optional `limit` and `verbose` parameters for the full dump. This
  keeps large reports from overflowing the response channel (they previously had to be
  spilled to files); `advise_sql` (a single query) is never truncated.

## [0.2.8] - 2026-06-20

### Added
- **`query_graph` relevance scores:** the tool now ranks results by relevance instead of
  returning them in traversal/lexicographic order. Expansion is best-first (a priority
  frontier keyed by relevance), high-fan-out hub nodes are down-weighted so a registry or
  builder no longer floods the result with its incidental neighbours, and seeds are scored
  with length-normalised IDF. Each structured node carries a `score` (higher = more
  relevant; nodes are sorted by it) and edges are ordered by endpoint relevance, so a
  caller can focus on the top results and ignore the low-scored tail. The `codegraph query`
  CLI prints the ranked nodes with their scores.
- **`query_graph` git/recency awareness:** an optional `since` argument (a git ref like
  `main`, a date like `"2 weeks ago"`, or `auto` to detect the default branch) boosts nodes
  whose file changed on the current branch, so in-progress code surfaces first. Scope is
  `merge-base(since, HEAD)..working-tree`, so it includes uncommitted edits; the boost is
  churn-weighted. `recency_mode: "seed"` additionally injects the changed-file nodes as
  seeds, surfacing the branch's changed surface even when the question matches little
  ("what did this branch change"). Changed nodes are flagged with `changed: true` (a
  `(changed)` marker in text) and the result header reports the baseline. The `codegraph
  query` CLI exposes the same via `--since` / `--seed-changed`. Resolution runs git and
  degrades gracefully to a plain query when git is unavailable.
- **History helpers:** `codegraph_history::git::merge_base` (common ancestor of two revs)
  and a pure `parse_numstat` (so callers running git through their own runner can reuse the
  parsing).

## [0.2.7] - 2026-06-20

### Added
- **MCP `MCP-Protocol-Version` header validation (Streamable HTTP):** a request sent after
  initialization with an unsupported `MCP-Protocol-Version` is now rejected with HTTP
  `400 Bad Request`, per the 2025-11-25 transport. An absent header is tolerated for
  backwards compatibility (assumed `2025-03-26`), and the `initialize` request is exempt.
- **`advise_sql` typed output schema:** the MCP tool now declares the same structured
  `findings` shape as `audit_sql` (`rule_id`, `severity`, `category`, `title`, `detail`,
  `location`, `remediation`, `confidence`), so clients can parse its result.

### Changed
- **Skill framing:** the generated CodeGraph skill (frontmatter description, intro, and the
  always-on block) now positions the graph as a code-intelligence and change-impact layer
  -- navigate code AND forecast/verify a change before editing -- rather than only a faster
  search.
- **MCP server `instructions`:** the `initialize` orientation text now covers the full tool
  surface (impact/forecasting, structural search, `describe_node`, `time_travel_diff`,
  `plan_rename`, SQL audit) instead of the original twelve-tool subset, and no longer points
  to the CLI for the architecture diff that `time_travel_diff` already exposes.
- **`SECURITY.md`:** replaced the placeholder template with an accurate policy (supported
  version line, private-advisory reporting, and the read-only / `--allow-exec` and
  `Host`/`Origin`-allowlist boundaries).

### Fixed
- **MCP tool descriptions and schemas reconciled with behavior:** `audit_sql` no longer
  advertises N+1 detection (which needs a source root the read-only MCP path does not pass);
  the `affected` `relations` default now lists the cross-language relations it actually walks
  (`invokes`, `binds_native`, `calls_service`, `handled_by`); `god_nodes` and `shortest_path`
  now state their numeric defaults (10 and 8); and `get_source` documents that it stops at a
  symbol's span end. Wiki structured-output and tool counts, and the 0.2.5 static-rule count,
  reconciled with the code.

## [0.2.6] - 2026-06-19

### Changed
- **MCP server, protocol 2025-11-25:** the server now negotiates protocol revision
  `2025-11-25` as its latest (legacy `2025-06-18` / `2025-03-26` / `2024-11-05` still
  accepted), advertises the optional `serverInfo.description`, and rejects browser
  requests carrying a disallowed `Origin` header with HTTP `403` (DNS-rebinding
  protection over Streamable HTTP, alongside the existing `Host` allowlist).

## [0.2.5] - 2026-06-19

### Added
- **SQL performance & security auditor (`codegraph sql audit`):** new `codegraph-sqlaudit` crate
  that runs a rule engine over a SQL-aware graph and reports findings by severity, each with a
  location, the offending object/query, a remediation, a confidence score, and the graph evidence
  that triggered it. 19 static rules across security (row-level-security gaps, RLS not `FORCE`d,
  `USING`-without-`WITH CHECK`, views without `security_invoker`, SQL Server security-policy
  coverage, over-broad grants, secret-looking columns, string-concatenation injection),
  performance (unindexed foreign-key columns, unindexed RLS-filter columns, `SELECT *`, non-sargable predicates,
  `UPDATE`/`DELETE` with no `WHERE`, `ORDER BY RAND()`, N+1 in a loop, many-join queries), and
  design (missing primary key, implied-but-missing foreign key, positional `INSERT`).
- **`codegraph sql advise --query "<sql>"`:** critiques a candidate query before it is written,
  cross-referenced against the graph's tables, indexes, and RLS.
- **Live `EXPLAIN` (optional `live-explain` feature):** `sql audit --explain --db-url <url>` runs
  `EXPLAIN` (never `EXPLAIN ANALYZE`) to confirm real sequential scans, raising `PERF-PLAN-001`.
- **SQL-aware extraction:** `.sql` now produces `table` / `view` / `column` / `index` / `trigger` /
  `procedure` / `policy` / `role` nodes and `has_column` / `has_index` / `indexes` / `references` /
  `protected_by` / `grants` / `reads_from` edges, and a cross-language pass links application code to
  the tables it touches (`queries` / `writes_to` / `calls_proc`). A dedicated regex path recovers
  columns, primary/foreign keys (inline and via `ALTER TABLE ADD CONSTRAINT`), and indexes from the
  bracketed T-SQL / SQL Server DDL that the multi-dialect parser cannot read.
- **`audit_sql` and `advise_sql` MCP tools** (read-only), bringing the default MCP tool set to 26.
- **CGQL SQL properties:** tables expose `rls_enabled` and `dialect`, so the SQL layer is queryable
  with `codegraph search` (e.g. every table with row-level security disabled).
- **`extract --no-columns`:** skip SQL column and index nodes for a smaller `graph.json` on
  column-heavy schemas (the table / RLS / policy / grant / view facts are kept).
- **3D viewer "Spread" slider:** scales the force simulation's repulsion and link distance and
  reheats the layout live, so a dense central cluster can be expanded outward for a clearer view.

### Changed
- **The 2D, 3D, and SVG visualizations are now SQL- and cross-language-aware:** nodes are shaped by
  their real kind (table, column, view, index, procedure, trigger, policy) and edges are colored by
  relation, so the SQL layer and code-to-SQL bridges stand apart from generic calls. The interactive
  viewers add color-by-kind, per-kind filters (a schema/layer view), a show-columns toggle, a
  bridges-only toggle, and SQL facts (dialect, type, PK/FK, RLS) in tooltips and the details panel.
  On large column-heavy graphs the SVG keeps structural nodes and drops columns first rather than
  taking an arbitrary cut.
- **MessagePack AST cache is now the default** (`cache-binary`): the per-file extraction cache is
  stored as MessagePack instead of JSON — faster to decode and smaller on disk, which helps most on
  column-heavy SQL schemas. Build with `--no-default-features` to fall back to JSON.
- Documentation: a new "SQL Auditing" wiki page, updated visualization / output / extraction /
  commands / MCP / querying / languages pages, and a README that leads with the full capability set.

### Fixed
- SQL extraction was blind on real T-SQL / SQL Server schemas — bracketed identifiers collapsed a
  whole schema to a single node and produced zero columns; the dedicated T-SQL path fixes object
  naming and recovers columns, keys, and indexes from `ALTER TABLE` and `CREATE INDEX`.
- Auditor false positives found on a real Postgres application: schema-qualified views were wrongly
  flagged for a missing `security_invoker`, and table-level foreign keys produced spurious
  implied-foreign-key findings. Both are corrected (the FK rule now keys off a column-level
  `fk_target` and a key-typed `*_id` column that is not the primary key).

## [0.2.4] - 2026-06-17

### Added
- **Function signatures in node metadata:** functions and methods now carry a captured
  `signature` (parameter names with optional types, a return type, and a raw header), surfaced in
  `graph.json`, the structured `structural_search` output, and the `get_node` tool. Captured for
  the config-driven languages plus Go and Rust; types appear when the source annotates them.
- **`describe_node` MCP tool:** a graph-only "takes X, returns Y, calls Z" description composed
  from a symbol's signature and outgoing call edges (the "calls" clause includes the cross-language
  `invokes`/`calls_service` targets). Read-only and in the default tool set.
- **Cross-language edges (`invokes` / `binds_native` / `calls_service` / `handled_by`, all
  INFERRED):** a post-extraction pass that links coupling no single-language parse can see, so
  impact analysis spans language boundaries.
  - Subprocess invocations for Python, JS/TS, Go, Rust, Ruby, and PHP, resolved to in-repo
    binaries/scripts where a unique match exists.
  - FFI bindings: PyO3, ctypes/cffi, JNI, cgo, and node-gyp/N-API.
  - HTTP/RPC service boundaries: server routes for Flask/FastAPI, Express, axum/actix, Go net/http
    (including Go 1.22 `"METHOD /path"` patterns), and tonic/Python gRPC; client calls for
    requests/httpx, axios/fetch, Go http, and reqwest.
  - Cross-file and cross-repo resolution: cross-file axum handlers, two-sided PyO3 (a Python
    importer to a Rust `#[pymodule]` across files), parameterized route matching (`/users/7` to
    `/users/{id}`), and cross-repo route matching in federated workspaces.
  - Detection runs over masked source (comments, docstrings, string and raw-string contents blanked
    first) with precision guards (the reqwest file-gate, a gRPC `<Name>Client` denylist, and
    per-impl gRPC method resolution).
- **`codegraph eval cross-language`:** single-graph calibration of the cross-language edge layer
  (per-relation counts plus service-connectivity and invocation-resolution precision proxies).

### Changed
- Reverse-impact (`affected`, `predict_impact`, `affected_tests`, `predict_edit`) now traverses the
  four cross-language relations by default, so the blast radius crosses subprocess/FFI/HTTP/gRPC
  boundaries.
- `structural_search` and `describe_node` join the structured-output tools (typed
  `structuredContent` + `outputSchema`); the default MCP server now exposes 24 read-only tools (25
  with `--allow-exec`).
- Documentation: a new "Cross-Language Edges" wiki page, plus updates to the MCP, querying,
  extraction, commands, and languages pages; the assistant skill and MCP `affected` description now
  note cross-language impact.

## [0.2.2] - 2026-07-02

### Added
- **Change forecasting (`codegraph predict`):** new `codegraph-predict` crate. Given the files a
  change touches (or a `git diff`), it composes the existing graph primitives into a single
  forecast: the graph nodes the change defines, the reverse-impact blast radius that depends on
  them, the at-risk tests that exercise the changed code, which edited symbols are public API,
  new import cycles / removed public APIs / dependency deltas (from a time-travel diff), a
  heuristic change-risk score, and a verify checklist. Exposed as the `predict_impact` and
  `affected_tests` MCP tools.
- **Predictive test selection (`affected_tests`):** the tests that exercise the changed code,
  found by walking the reverse-impact set from the changed files and keeping the test nodes
  (detected by path convention). The focused "which tests should I run for this change" view.
- **Co-change mining (evolutionary coupling):** mines git history for files that historically
  change together with the changed files, catching coupling that static analysis misses (e.g. a
  schema and its serializer that share no import but always change together).
- **Edit-impact prediction (`codegraph predict --edit <symbol>`, `predict_edit` MCP tool):** an
  analytic forecast of one symbol edit, classified into "will break" vs "to review". `kind=delete`
  (every dependent breaks), `signature` (callers/type-users break, bare imports go to review), or
  `visibility` (cross-file references break when narrowing to private). Complements `plan_rename`.
- **Speculative execution (`codegraph speculate`):** new `codegraph-sandbox` crate. Applies a
  change in a throwaway git worktree and runs a build/type-check plus the forecast's at-risk tests
  (auto-detecting cargo/go/pytest/npm), reporting real pass/fail. Exposed as a gated `speculate`
  MCP tool: a default server stays strictly read-only with 23 tools, and `serve --allow-exec` adds
  `speculate` as the 24th, non-read-only tool.
- **Forecast evaluation and calibration (`codegraph eval replay`):** new `codegraph-eval` crate.
  Replays `from..HEAD`, re-predicting each non-merge commit from its parent-state graph (built in a
  worktree, cached per SHA) and scoring the prediction against git ground truth: co-edited
  test-selection recall/precision, removed-API detection, and blast-radius selectivity. Writes a
  Markdown/JSON report, records a prediction ledger, and gates CI with `--min-test-recall`.

### Performance
- **The predict MCP tools reuse a cached reverse-impact index.** The server now builds the
  reverse-adjacency once per graph load/reload (next to the query index) instead of rebuilding it
  on every `predict_impact` / `affected_tests` / `speculate` request. Per-request forecast on a
  5k-node graph drops from roughly 1.84ms to 0.92ms; the one-shot CLI path keeps its borrowed
  build and is unchanged. Equivalence tests assert the cached path returns identical results.

## [0.2.1] - 2026-07-01

### Fixed
- **CI `extract-langs` matrix:** the metadata-enrichment integration test ran every
  language's case under each single-language build, so the non-enabled grammars panicked
  and turned the whole matrix red. Each test is now gated on its `lang-*` feature.
- **Refactor plans no longer double-list a site:** the definition and same-file call sites
  are given a precise name-token column, so they dedup against the textual scan instead of
  appearing twice. A trustworthy same-file direct call now lands in the apply set rather
  than review. `move`/`extract` plans no longer render a no-op `rename X -> X`.

### Changed
- **CGQL `.name` is the bare symbol.** A query like `WHERE f.name = "announce"` now matches a
  function whose label is `announce()`; `.name` is consistent across kinds (class labels were
  already bare). Use the existing `=~` operator for a regex/substring match. Results still show
  the full label.

## [0.2.0] - 2026-06-30

### Added
- **Time-travel diff (`codegraph diff <rev1> [rev2]`):** new `codegraph-history` crate builds
  the graph at each git revision in a throwaway worktree (cached per commit SHA) and reports
  added/removed module dependencies, removed APIs, architectural drift, new dependency cycles,
  and change hotspots. `--since <date>` resolves the base from a date; `--report` writes
  Markdown and `--html` a self-contained, theme-aware HTML report.
- **Architectural search with CGQL (`codegraph search`):** new `codegraph-cgql` crate, a
  Cypher-inspired structural query language matching on kind/visibility/loc/fan-in/out/degree/
  community/name/file/lang with `= != < <= > >= =~` and `AND`/`OR`/`NOT`, relationship patterns
  including variable-length paths (`-[:calls*1..3]->`), `count(...)` aggregation, `--explain`
  query plans, and saved queries (`--save`/`--saved`/`--list-saved`). Ships a named-pattern
  library: singleton, factory, observer, service-locator, god-class.
- **Safe refactor (`codegraph refactor`):** new `codegraph-refactor` crate. `rename`, `move`,
  and `extract` resolve a symbol (surfacing ambiguity), compute the blast radius, score each
  edit site by confidence, and emit a `plan.json` + `plan.md` for an AI agent to apply, plus a
  whole-word textual scan for type references the graph does not record as edges and a
  cross-repo `repo` tag on federated sites. CodeGraph never edits source. `refactor verify`
  (and `verify --relocate`) rebuilds and checks the graph held its shape: the symbol was
  renamed/relocated, no references lost, no located nodes dropped, no new cycles.
- **Node metadata enrichment:** code nodes now carry `kind` (class/function/method/...),
  `visibility`, and line-`span`/LOC, surfaced in `get_node`/`get_source`, Cypher/GraphML
  exports, and CGQL. New graph helpers: `fan_in`/`fan_out`/`filter_nodes`/`loc` and an
  iterative Tarjan `strongly_connected_components`.
- **Three new MCP tools (17 -> 20):** `structural_search` (CGQL or a named pattern),
  `time_travel_diff` (graph diff between two revisions), and plan-only `plan_rename` (a
  confidence-scored rename plan; never edits). All read-only.

### Changed
- `codegraph diff`'s base revision (`rev1`) is now optional when `--since` is given.

## [0.1.1] - 2026-06-30

### Added
- **MCP server, protocol 2025-06-18:** the `initialize` reply now negotiates the protocol
  version and advertises structured tool output, prompts, completions, and resource
  subscriptions. Tools carry `outputSchema`/`structuredContent` (for `graph_stats`,
  `god_nodes`, `affected`, `query_graph`) and read-only/open-world annotations.
- **New MCP tools:** `get_source` (return a symbol's actual source, jailed to a trusted
  `--source-root`), `affected` (transitive reverse-impact / blast radius of a change),
  `find_callers` / `find_callees` (directional call navigation), and `working_changes_impact`
  (graph blast radius of your branch's `git diff` against a base, no `gh` required).
- **MCP prompts** (`onboard`, `explain_subsystem`, `assess_pr`, `trace_flow`), **argument
  completions** (`completion/complete` for labels, repo tags, community ids), and **resource
  templates** (`codegraph://node/{label}`, `codegraph://community/{id}`).
- **Resource subscriptions:** an HTTP SSE session receives `notifications/resources/updated`
  when the graph hot-reloads.
- **`serve --source-root`** — trusted root for `get_source` file reads (path-traversal jailed).
- Pagination for `get_community` and `god_nodes` (`offset`/`limit`), and real `cl100k` token
  budgeting for `query_graph` output.
- **.NET project files** (`.csproj/.fsproj/.vbproj/.sln/.slnx`): extract project references,
  NuGet `<PackageReference>`s, and `TargetFramework`/SDK (as `concept` nodes). Project
  references resolve to the referenced project's own file node.
- **Markdown structure** (`.md/.mdx/.qmd`): heading hierarchy as `document` nodes connected
  by `contains` (runs unconditionally, alongside the optional LLM semantic pass).
- **Framework-aware edges:** PHP/Laravel `bound_to` / `uses_config` / `listened_by` /
  `uses_static_prop` / `references_constant`; Dart/Flutter `navigates` (string, object, and
  const routes) plus Riverpod/Bloc `references` and Bloc event/state flow (`calls`). Dart
  framework edges attach to the enclosing method/class.
- **More languages** (regex/delegation fallbacks): Salesforce **Apex** (`.cls/.trigger`),
  **Pascal/Delphi** (`.pas/.pp/.dpr/.dpk/.lpr`), and **Razor/Blazor** (`.razor/.cshtml`,
  via the C# extractor).
- **`codegraph export <format>`** — regenerate any output (json, html, svg, graphml, cypher,
  dot, callflow, tree, 3d, obsidian, wiki, report) from an existing `graph.json` without
  re-extracting; `--repo` scopes to a federated member.
- **Live database push** (off-by-default `push` build feature): `codegraph export neo4j|falkordb
  --push <uri>` streams the graph into a running Neo4j (via `cypher-shell`) or FalkorDB (via the
  `redis` client). Without `--push`, both write the importable `graph.cypher` script.
- **DOT/Graphviz exporter** — `graph.dot` is now written by every `extract` (and via `export dot`).
- **Broader skill installers:** `cursor`, `copilot`, and `kilo` join `claude`/`agents`/`gemini`;
  `codex`/`opencode` alias onto the `AGENTS.md` installer.
- User-facing `README.md`, `LICENSE` (AGPL-3.0-or-later), and this changelog.
- `release` GitHub Actions workflow that builds and attaches prebuilt `codegraph` binaries
  for Linux, macOS, and Windows to each tagged release.
- `query --dfs` — expand the query subgraph depth-first instead of breadth-first (the
  traversal mode previously reachable only via the MCP server).
- `prs --triage` — deterministic ranked view of actionable PRs with graph blast radius
  (no LLM; for LLM summarization use the MCP server's `triage_prs` tool).
- `prs --conflicts` — report PRs that touch the same graph community (merge-order risk).
- Azure OpenAI backend support: deployment-path URL
  (`/openai/deployments/{deployment}/chat/completions?api-version=…`) with an `api-key`
  header, configurable via `AZURE_OPENAI_API_VERSION`.
- `LlmClient::complete_with_content` — transport path for structured/multimodal (vision)
  message content, so image payloads can actually be sent (end-to-end pass wiring pending).
- `CODEGRAPH_LLM_TEMPERATURE` override (numeric, or `none`/`omit`/`default` to omit the
  parameter).

### Changed
- `query_graph` renders its text and structured output from a single graph retrieval.
- The installed skill, the server `initialize` instructions, and the Codex hook now describe
  the full 17-tool MCP surface.

### Fixed
- **Bash `source` resolution:** `source ./lib.sh` now resolves relative to the sourcing
  file's directory (to the target's real file node), so two same-named scripts in different
  directories no longer collapse to one node.
- **detect/extract drift:** 29 file extensions were classified as `Code` but had no
  extractor, inflating corpus stats and silently producing zero nodes. `.mm` now routes to
  the Objective-C extractor; the remaining unextractable extensions are no longer
  classified as code. A new invariant test (`every_detected_code_extension_has_an_extractor`)
  keeps the detect and extract sets from drifting. (`.csproj/.sln/.slnx/.fsproj/
  .vbproj`, `.cls/.trigger`, `.pas/.pp/.dpr/.dpk/.lpr`, and `.razor/.cshtml` are recognized
  again now that their extractors have landed.)
- **Reasoning-model temperature:** requests to OpenAI o1/o3/o4 and gpt-5 models no longer
  send an explicit `temperature` (which those models reject with HTTP 400).
- Azure backend was previously routed through the generic chat-completions path with bearer
  auth and could not reach a real Azure deployment.

[Unreleased]: https://github.com/ColinVaughn/CodeGraph/compare/v0.2.8...HEAD
[0.2.8]: https://github.com/ColinVaughn/CodeGraph/compare/v0.2.7...v0.2.8
[0.2.7]: https://github.com/ColinVaughn/CodeGraph/compare/v0.2.6...v0.2.7
[0.2.6]: https://github.com/ColinVaughn/CodeGraph/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/ColinVaughn/CodeGraph/compare/v0.2.4...v0.2.5
[0.2.1]: https://github.com/ColinVaughn/CodeGraph/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/ColinVaughn/CodeGraph/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/ColinVaughn/CodeGraph/releases/tag/v0.1.1
