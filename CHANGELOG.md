# Changelog

All notable changes to Synaptic are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> Entries at or before 0.2.12 were released under the project's former name,
> **CodeGraph**, and reference the old `codegraph` command and crate names. They
> are preserved verbatim as historical record.

## [Unreleased]

## [0.3.8] - 2026-06-22

A tooling-quality round from auditing a 9-repo federated workspace: clearer
diagnostics across the SQL auditor, `get_source`, git, and the PR tools, more
machine-readable MCP output, and federated source reading. Verified end-to-end on
both a federated workspace and a single-repo monorepo.

### Added
- **Structured output on four more MCP tools.** `predict_impact`,
  `affected_tests`, `get_neighbors`, and `list_repos` now declare an
  `outputSchema` and return a typed `structuredContent` object beside the text, so
  a client can parse results instead of scraping formatted text (12 structured
  tools total). The two forecast tools build their `ChangeForecast` once and render
  both channels from it. A structured mirror that cannot resolve its node (e.g.
  `get_neighbors` on an ambiguous label) omits `structuredContent` rather than
  emitting a null object.
- **Federated `get_source`.** Serving the global graph
  (`synaptic serve --graph ~/.synaptic/global-graph.json`) reads
  `global-manifest.json` and registers each member repo's own source root, so a
  federated node whose `source_file` points at a sibling repo outside a single
  `--source-root` is read from its real repo. Co-located workspace builds (members
  under one root) already resolved and are unchanged.

### Changed
- **`SEC-INJ-001` distinguishes identifier interpolation from value
  interpolation.** When the interpolation sits in identifier position (a
  table/column name, e.g. `FROM "main"."${table}"`), the remediation now steers to
  a fixed allowlist plus the driver's identifier-quoting helper, instead of
  recommending bound parameters — identifiers cannot be bound as parameters.
- **`get_source` errors name the cause and the root.** Instead of a bare "Source
  not available", the message says whether no source root was configured, the file
  was not found under `<root>` (with a federation hint), or the path resolved
  outside the configured `--source-root`.
- **`working_changes_impact` separates "no changes" from "git unavailable".** A
  clean tree reports `No changes vs <base>.`; a missing/failed git or a directory
  that is not a git repository (e.g. the top-level of a federated workspace)
  reports a distinct "git unavailable or not a git repository ... continues
  offline" message, so the two outcomes are no longer conflated.
- **PR tools soften the offline failure.** When `gh` is missing or
  unauthenticated, `list_prs` / `get_pr_impact` / `triage_prs` note that PR data is
  skipped while the rest of the graph audit continues offline.

### Fixed
- **Duplicate SQL findings.** A code-to-SQL link is emitted once per referenced
  table, so a multi-table or schema-qualified interpolated query (e.g.
  `SELECT COUNT(*) FROM "main"."${table}"`, which links both `main` and `${table}`)
  produced one identical finding per table. The auditor now deduplicates findings
  on `(rule_id, location, query)`, reporting a query once per rule.

## [0.3.7] - 2026-06-21

Two multi-repo federation gaps for .NET/WebSocket products: .NET solution repos
were dropped from federation, and WebSocket coupling between repos was invisible.

### Added
- **Versioned, self-refreshing agent skills.** Installed skill files now carry a
  version stamp (`<!-- synaptic-skill vX.Y.Z -->`), and `synaptic install` records
  each install in `~/.synaptic/skills.toml`. `synaptic self-update` then re-renders
  every recorded skill to the new version automatically (it spawns the freshly
  installed binary so the new content is used), and `synaptic install --refresh`
  does the same on demand. Skills that are byte-identical to what we wrote are
  refreshed in place; hand-edited skills are detected by content hash and left
  untouched (reported so you can re-run `synaptic install <host>` to overwrite);
  entries whose files are gone are dropped. The build-time drift snapshots are
  unaffected — the stamp is added at write time, so a version bump never churns
  `expected/`.
- **WebSocket cross-language edges.** A new detector links a client that opens a
  socket and exchanges JSON command messages (or socket.io events) to the server
  that handles them, across languages and repos. It mints two boundary-node
  kinds — a `ws_endpoint` (keyed by socket path) and a `ws_message` (keyed by the
  application message type / event name) — and connects clients via
  `calls_service` and handlers via `handled_by`, so reverse-impact and
  `affected` / `predict_impact` traverse the socket boundary. Covered: JS/TS raw
  `ws` (`.send({cmd})` / `case` dispatch) and socket.io (`emit`/`on`); C#
  WebSocketSharp / `System.Net.WebSockets` (`AddWebSocketService` + `case`);
  Python `websockets` + python-socketio; Rust tungstenite (endpoint only). All
  edges are `INFERRED`. The command-keyed node is endpoint-independent because the
  connection URL and the message sites routinely live in different files.

### Fixed
- **.NET solution repos are no longer dropped from multi-repo federation.** A repo
  whose root holds only a `.sln` (with the `.csproj` projects in subdirectories —
  the standard layout) failed the manifest check used by the sibling-repo scan and
  was skipped entirely, and even when included it produced no coordinate, so no
  export surface. `.sln` is now a recognized manifest, and the .NET coordinate
  falls back to the first solution project's `AssemblyName`/`RootNamespace` (then
  the `.sln` stem) when there is no root `.csproj`. Such a repo now federates as a
  member with a `dotnet` coordinate and export surface.

### Changed
- The federated-build summary now reports **cross-language** cross-repo links
  (`N extracted, M inferred, K cross-language`). The `extracted`/`inferred`
  counters only ever covered import/coordinate resolution; HTTP/RPC/FFI/WebSocket
  boundaries that span repos are flagged on the edge and were absent from the
  summary, which made a graph with real cross-language coupling read as
  "0 cross-repo links".
- **`graph_stats` reports cross-repo coupling on a federated graph.** The MCP
  `graph_stats` tool (text + structured output) and the `GRAPH_REPORT.md` overview
  now include how many edges span repositories and how many of those are
  cross-language, computed from the loaded graph — so the count is visible to an
  agent or in the report, not only in the one-shot build summary. Both are 0 (and
  the line omitted) for a single-repo graph.

## [0.3.6] - 2026-06-21

A round-3 agent-feedback pass on a11ycore: a real import-resolution bug that hid a
symbol's direct unit tests, plus two usability follow-ups and a discoverability nit.

### Fixed
- **Relative imports that differ only by their `./` vs `../` prefix no longer
  collapse into one phantom module stub.** `make_id` trims leading dots, so a
  sibling `import './foo'` and a `import '../foo'` from a subdirectory hashed to
  the same stub-node id. The cross-file resolver reads each import's specifier
  back from that single shared stub, so it could rebind only one importer's edge
  and stranded the others as unresolved "phantom" nodes (empty source, degree 2).
  In practice a unit test in `__tests__/` importing `../foo` was never linked to
  `foo.ts`, so `affected_tests` / `predict_impact` missed the direct test (and
  could surface a spurious transitive one in its place). Module stubs now fold the
  relative-climb depth into their id, so distinct specifiers stay distinct while
  identical ones still share a node. This also removes the phantom-node graph
  noise from neighbor/community results.

### Changed
- **`predict_edit` now summarizes like its siblings.** Added `limit` (default 20)
  and `verbose`, plus a per-section by-depth rollup in the header
  (`Will break (438) by depth: 1h: 274, 2h: 155, 3h: 9`). It previously emitted
  every dependent uncapped (tens of KB on a hub). The CLI `predict --edit` already
  writes its full report to a file and is unchanged.
- **`working_changes_impact` gained an opt-in `code_only` flag** that counts and
  lists only code nodes, excluding non-code files (`package.json`, lockfiles,
  `.md` docs) to sharpen the blast radius. Default output is unchanged.
- **`speculate` is now discoverable.** The server `initialize` instructions
  explain that it is enabled by starting the server with `synaptic serve
  --allow-exec` (it is otherwise invisible, since it executes commands and so is
  not read-only).

## [0.3.5] - 2026-06-21

A discoverability follow-up to 0.3.4: the `name@file` qualifier and the `god_nodes`
test-coverage annotation now appear in the surfaces an agent actually reads -- MCP
tool schemas, the server `initialize` instructions, the generated skill, and CLI
`--help` -- not just the wiki. No behavior change; the 0.3.4 functionality is the
same, this makes it findable.

### Changed
- **The `name@file-substring` disambiguation qualifier is now documented on every
  name-taking tool**, not just `predict_edit`. It is spelled out in the MCP
  parameter schemas for `get_node`, `get_source`, `get_neighbors`, `describe_node`,
  `shortest_path`, `affected`, `find_callers`, and `find_callees`; in the server's
  `initialize` instructions; in the generated skill (`SKILL.md` / `AGENTS.md` /
  etc.); and in the `explain` / `path` / `affected` CLI help.
- **`god_nodes` advertises its per-hub test count in the structured output schema**
  (`test_count`), and the skill notes that `0 test(s)` flags an untested,
  high-blast-radius hub.
- Tool-description clarity: `get_neighbors` documents the empty-`relation_filter`
  hint, and `audit_sql` documents the severity-then-confidence ranking.
- A guard test now fails if a name-taking tool's schema drops the `@file` hint or
  the `god_nodes` schema loses `test_count`.

### Fixed
- **Docs:** the wiki "Seed resolution" section described a pre-unification resolver
  (claiming `query` / `path` / `explain` used a simpler exact-id-then-exact-label
  lookup) and the old `No unique node match` message. It now documents the shared
  cascade, the `name@file` qualifier, and the candidate list with file + degree.

## [0.3.4] - 2026-06-21

A second round of agent-feedback usability fixes (tested against a real external
repo), plus a dependency bump.

### Added
- **`god_nodes` flags untested hubs.** Each hub is annotated with how many tests
  transitively exercise it -- `N test(s)` in the text output, `test_count` in the
  structured mirror. A high-degree hub with `0 test(s)` (high blast radius, no
  safety net) is surfaced for what it is, without a follow-up `affected_tests`
  call. Because each row costs a reverse-impact walk, a page is capped (`top_n`
  default 10, max 200; page further with `offset`).

### Changed
- **The `name@file-substring` disambiguation qualifier now works across every
  name-taking tool** -- `get_node`, `get_neighbors`, `get_source`, `find_callers`,
  `find_callees`, `shortest_path`, `affected`, and `predict_edit` -- not just
  `predict_edit`. It is parsed in the shared resolver. A node id or label that
  legitimately contains `@` (for example `react@18` or an import specifier like
  `git@github.com`) still resolves as-is: the literal interpretation is tried
  first and the split is only a fallback.
- **Ambiguous-name results list each candidate's file and degree inline** (MCP and
  CLI), so an agent can pick one without a second `get_node` round-trip.
- **`get_neighbors` with a `relation_filter` that matches nothing now names the
  relations the node does have** -- `(none with relation 'calls'; this node has:
  method(11), contains(1))` -- so an empty result is no longer indistinguishable
  from a missing node.
- **SQL audit signal-to-noise.** `PERF-IDX-001` ("likely-foreign-key column not
  indexed"), a pure column-name heuristic at 0.5 confidence, is demoted from High
  to Medium so it no longer outranks evidenced security findings (RLS gaps,
  injection). Findings are now sorted by severity then by confidence (most
  confident first within a tier), and the confidence score is shown in the CLI,
  MCP, and Markdown output.

### Dependencies
- Bumped `zip` 2.4.2 -> 7.2.0.

## [0.3.3] - 2026-06-21

### Fixed
- **Stale edges still accumulated on incremental re-extract (follow-up to
  0.3.2).** The 0.3.2 fix keyed edge eviction on the *edge's* `source_file`, but a
  resolved cross-file call edge can carry a `source_file` normalized differently
  (for example absolute vs repo-relative) from the node it originates from, so the
  stale edge slipped past the filter and a retargeted call (`announce()` ->
  `log()`) still left the old edge behind in the live graph and on disk. Eviction
  is now keyed on the **source node's** file -- the same predicate node eviction
  uses -- so a re-extracted file's outgoing edges are reliably dropped and
  regenerated regardless of how the edge's own `source_file` was normalized. This
  is the path the MCP `serve` auto-freshen takes, so the fix reaches edits made
  mid-session.

### Fixed
- **Stale edges accumulated on incremental re-extract.** When a file was
  re-extracted, an outgoing edge from a surviving node was kept as long as both
  its endpoints still existed, even though the edge originated from the
  re-extracted file. So retargeting a call (for example `announce()` to `log()`)
  left the old edge behind, and these phantom edges silently inflated the blast
  radius reported by `affected`, `predict_impact`, and `affected_tests`. A
  re-extracted file's edges are now replaced rather than union-merged: an existing
  edge survives only when both endpoints are live **and** the edge did not
  originate from an evicted (re-extracted or deleted) file.
- **`time_travel_diff` / `synaptic diff` hotspots that changed only graph nodes**
  (with no line delta) rendered over MCP as a meaningless `+0/-0 lines` row. The
  MCP output now includes node churn (`+A/-B nodes`), matching the CLI.
- **`structural_search` column name was inconsistent.** The `god-class` pattern
  returned a column named `c` (the query binding) while every other pattern
  returned `node`; all patterns now return a single `node` column.

### Added
- **`find_callers` / `find_callees` pagination.** Both tools now lead with the
  true total and a per-relation breakdown (for example `208 Callers of announce
  [calls: 180, references: 28]:`) and cap the list with a `+N more` summary on a
  hub. New `limit` (default 50) and `verbose` (uncapped) parameters, matching
  `affected`.
- **`plan_rename` returns the actual edit sites over MCP.** In addition to the
  summary, the tool now lists each edit site (`file:line:col`, `old -> new`,
  reason, confidence) under `Edits` and the lower-confidence ones under `Review`,
  so an agent can apply a rename without a second round-trip to the CLI's
  `plan.md`. New `limit`/`verbose` parameters cap each section. The per-site
  renderer is now shared with the CLI so the two cannot drift.
- **`working_changes_impact` node/community detail.** A new `verbose` flag
  additionally lists the top touched nodes (ranked by connectivity) and the
  touched communities with labels; `limit` (default 20) caps the node list.
  Default output is unchanged (changed files plus counts).

### Documentation
- Documented the MCP server's on-query auto-freshen ("when updates happen") in
  the Incremental-Updates and MCP-Server wiki pages: it is not a live filesystem
  watcher but a debounced, manifest-based catch-up that runs on the next query,
  and corrected the incremental edge-merge description to match the fix above.

## [0.3.1] - 2026-06-21

### Added
- **Self-update.** A `self-update` command updates the binary in place from the
  latest GitHub release: it downloads the prebuilt archive for your platform,
  verifies its SHA-256 checksum, and prompts before swapping the running binary
  (and its `syn` alias). `--yes` skips the prompt, `--check` only reports
  availability. An **opt-in** background check (`self-update --enable`) prints a
  one-line "update available" notice at most once per day; it is off by default,
  writes to `~/.synaptic/update.toml`, honors `GITHUB_TOKEN` for the API rate
  limit, and can be force-disabled with `SYNAPTIC_UPDATE_CHECK=0`. Release
  archives now publish a `.sha256` sidecar for verification. See the
  [Updating](https://github.com/ColinVaughn/Synaptic/wiki/Updating) wiki page.
- **Auto-freshen for `serve`.** The MCP server now detects files added, changed,
  or removed since the last extraction and runs an incremental rebuild before
  answering a query, so files an agent writes mid-session are queryable without a
  separate `watch` or `update`. The staleness check is debounced (so a burst of
  queries walks the tree once) and runs on both the stdio and HTTP transports.
  On by default; opt out with `SYNAPTIC_SERVE_AUTOFRESH=0`, tune the debounce
  with `SYNAPTIC_SERVE_AUTOFRESH_DEBOUNCE_MS` (default 1000), and cap the catch-up
  with `SYNAPTIC_SERVE_AUTOFRESH_MAX_FILES` (default 500; 0 = unlimited, skipped
  above the cap so a branch switch does not block a query on a near-full rebuild).

### Changed
- Incremental rebuilds (`update`, `watch`, and the new `serve` auto-freshen) now
  allow a bounded graph shrink, so symbol removals (for example deleting a
  method) propagate. The strict shrink guard still applies to full rebuilds.
- `extract` and `update` persist a build-provenance manifest (reusing their
  existing file scan) so `serve` can detect what changed since the last build.

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
