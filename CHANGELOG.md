# Changelog

All notable changes to CodeGraph are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/ColinVaughn/CodeGraph/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/ColinVaughn/CodeGraph/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/ColinVaughn/CodeGraph/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/ColinVaughn/CodeGraph/releases/tag/v0.1.1
