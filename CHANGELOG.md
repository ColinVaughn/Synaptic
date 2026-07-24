# Changelog

All notable changes to Synaptic are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> Entries at or before 0.2.12 were released under the project's former name,
> **CodeGraph**, and reference the old `codegraph` command and crate names. They
> are preserved verbatim as historical record.

## [0.7.0] - 2026-07-23

> **Upgrade note:** this release relicenses the project under FSL-1.1-ALv2, adds
> a workspace-wide watch mode, hardens the sharded store against a tampered
> manifest, and ships license notices with every release artifact. The graph
> schema and the CLI remain compatible with 0.6.4.

### Added

- **`synaptic workspace build --watch` keeps a federated graph live across every
  member repository.** Previously the only watcher was the single-repo `synaptic
  watch`, which at a workspace root rebuilt the whole tree as one flat
  repository and overwrote the federated `graph.json` with un-namespaced,
  un-tagged nodes (duplicating every symbol and disabling cross-repo dedup). The
  new mode watches the workspace tree plus each member checked out *outside* it
  (`[[repos]] path = ...`), collapses overlapping roots so no event is handled
  twice, debounces a burst of saves (`--debounce-ms`,
  `SYNAPTIC_WATCH_DEBOUNCE_MS`), and drains each batch into the existing
  `--changed` incremental federation. Event filtering reuses
  `synaptic-incremental`'s ignore/extractable rules, so writing `graph.json` (or
  a member's own `synaptic-out/cache`) cannot self-trigger. Output names the
  member that changed and the members whose export surface moved; editing
  `synaptic-workspace.toml` re-resolves members and re-registers watchers
  without a restart; a member that cannot be read or watched is reported by tag
  and the watcher keeps running. Each cycle takes the per-repo rebuild lock so a
  concurrent `synaptic update` or git hook cannot interleave its write, and
  workspace state is persisted only after artifacts land.
- **Immutable hosted serving closes the initial-open and listener races.**
  `serve --immutable-graph --expected-graph-sha256 <digest>` parses the exact
  digest-verified bytes, while `--http 127.0.0.1:0 --ready-file <path>`
  publishes the already-bound loopback endpoint through a protected atomic file.
  The ready document is written as complete JSON via an atomic hard link, never
  overwrites an existing path, and refuses a group- or world-writable parent
  directory.
- **Reproducible container images for the public engine.**
  `docker/synaptic-engine.Dockerfile` builds the glibc engine on a non-root
  distroless runtime; `docker/synaptic-engine-worker.Dockerfile` builds a
  musl-linked engine on non-root Alpine for hosted workers. Both build only the
  FSL-licensed Rust workspace from the repository root and never include
  proprietary source; a root `.dockerignore` keeps unrelated trees out of the
  build context.
- **Contributor guidance and a Developer Certificate of Origin check.** New
  `CONTRIBUTING.md` plus a `dco` workflow require a `Signed-off-by` line on
  contributions, matching the source-available licensing model.
- **Release artifacts now carry third-party license notices.** The release
  workflow generates `THIRD_PARTY_LICENSES.html` with a pinned `cargo-about`
  (`about.toml` / `about.hbs`) and ships it alongside `LICENSE` and a new
  `NOTICE` in every distribution archive.

### Changed

- **Synaptic is relicensed under the Functional Source License 1.1 with an
  Apache 2.0 Future License (`FSL-1.1-ALv2`).** This release begins the
  source-available public-engine line: non-competing use is permitted, and each
  version automatically becomes Apache-2.0 licensed two years after it is made
  available. The private Synaptic Platform site and B2B control plane remain
  proprietary. Existing copies and releases retain the licenses under which
  they were received. The `cargo-deny` license allowlist follows, replacing
  `AGPL-3.0-or-later` with `FSL-1.1-ALv2`.
- **A federated build can write `graph.json` without the visual artifact suite.**
  `workspace build --watch` re-federates on every save, where regenerating the
  SVG/3D/HTML/GraphML suite dominates the cost and nobody reads it mid-edit, so
  it is now opt-in via `--artifacts` — matching how `synaptic update`/`watch`
  already gate the same suite. Every non-watch federated build (`build`,
  `federate`, `discover`, `sync`) still writes the full suite as before.

### Security

- **A tampered store manifest can no longer read files outside the store root.**
  Shard filenames from `manifest.json` were joined to the store root without
  validation, so a crafted manifest could reference a parent path, an absolute
  path, or a symlink and have the reader open an arbitrary file. Each shard
  reference must now be a single relative filename that resolves, after
  canonicalization, to a regular file directly inside the store root; symlinks,
  directories, and traversal components are rejected with a manifest error.

## [0.6.4] - 2026-07-17

> **Upgrade note:** this patch tightens MCP protocol conformance, makes live
> graph reads consistently fresh, and removes several large-graph query
> bottlenecks. The graph schema and CLI remain compatible with 0.6.3.

### Added

- **Stateful MCP sessions now implement the complete initialization lifecycle.**
  Streamable HTTP records the negotiated client and protocol version, gates
  requests until `notifications/initialized`, rejects duplicate initialization,
  and supports URI-specific resource subscribe/unsubscribe notifications.
- **MCP regression and performance coverage is substantially broader.** New
  unit, stdio, HTTP, and end-to-end cases cover malformed envelopes, invalid
  initialization, schema validation, prompts and completions, resource URIs,
  cancellation, queue saturation, concurrency, session isolation, subscriptions,
  graph refresh, cached impact traversal, and structured-search limits.

### Changed

- **All graph-backed interfaces share one freshness policy.** MCP tools,
  resources, completion, and REST routes now reload or incrementally freshen the
  graph before reading, and cache entries are scoped to the graph version.
- **Large-graph MCP queries reuse indexed and shared results.** Reverse-impact
  traversal uses a cached adjacency index, structural search computes matches
  once and applies limits before projection, and repository, statistics, and
  neighbor responses share their computed reports. In the validated 50k-node
  fixtures, full impact traversal fell from about 55 ms to 4 ms, structural
  search from 300-430 ms to 56.6-70 ms, and neighbor lookup from a 52.782 ms
  median to 23.008 ms.
- **Completion caching avoids repeated broad scans.** A repeated broad
  50k-node completion request fell from roughly 10.1-10.6 ms to a 21.777 us
  median after the first graph-version-scoped result was cached.
- **Stdio request execution is bounded and control-aware.** A four-worker,
  32-request queue replaces unbounded per-request thread creation; a dedicated
  writer serializes output, while ping, initialization lifecycle, cancellation,
  and busy responses remain available under load.
- **Tool discovery is more concise without losing schemas.** `tools/list`
  descriptions were reduced from approximately 9,427 to 7,380 tokens, and the
  MCP wiki now documents lifecycle, subscription, watch, structured output, and
  error behavior.

### Fixed

- **JSON-RPC and MCP validation now return protocol-correct errors.** Invalid
  envelopes, malformed parameters, unsupported methods, invalid resource URIs,
  prompt/completion errors, and tool failures preserve request ids and use the
  appropriate JSON-RPC or MCP error code instead of successful-looking results.
- **Session and notification behavior is isolated per client.** Rejected or
  id-less initialization no longer allocates sessions, negotiated versions
  cannot drift, unsubscribe stops delivery, and graph updates are sent only to
  sessions subscribed to the changed URI.
- **Cached reverse-impact traversal preserves type-folding semantics.** Member
  roots reached through siblings remain visible while only explicitly excluded
  roots are omitted, matching the one-shot traversal at every tested depth.
- **The dependency audit no longer fails on a yanked lockfile entry.** The
  transitive `spin` crate is updated from 0.9.8 to 0.9.9; `cargo deny check`
  now passes advisories, bans, licenses, and sources.
- **Server benchmarks measure the intended operations.** The benchmark suite
  now avoids setup inside timed regions and includes the affected, structural
  search, neighbor, and completion paths optimized in this release.

## [0.6.3] - 2026-07-16

> **Upgrade note:** this is a graph-performance and connection-correctness patch.
> The graph schema and CLI remain compatible with 0.6.2. Re-run `extract` or
> `workspace build` to regenerate viewers and retain all deduplicated edge sites.

### Changed

- **Graph assembly and federation avoid repeated whole-graph work.** Workspace
  composition now unions all members in one indexed pass; clustering calculates
  eligible-community cohesion in one edge scan; entity-component application
  and community-id remapping use linear/sparse indexes; incremental topology
  comparison borrows graph data instead of cloning and sorting it; and consuming
  graph handoffs move node/edge payloads instead of cloning them. On the audit
  fixtures, 16 x 500-node federation fell from about 136.1 ms to 6.07 ms,
  10k-node topology comparison from 54.92 ms to 9.77 ms, and a 10k-node
  clustering fixture from 192.69 ms to 98.36 ms.
- **Duplicate edge provenance is accumulated once.** Semantic duplicates now
  collect extraction sites in first-seen order with hash-based membership and
  materialize the flattened `sites` metadata once per group. A 1,000-site
  duplicate group fell from 240.74 ms to 0.56 ms; the new path processes 10,000
  sites in about 6.13 ms.
- **Large viewers do less work per interaction and frame.** The 2D viewer
  coalesces filter events into one animation-frame flush, indexes edges by
  relation, and sends only changed visibility records to vis-network. The 3D
  fast path updates and raycasts only visible GPU instances, keeps all fast-path
  edges as GL lines, and reuses Three.js scratch objects instead of allocating
  them on every simulation tick.
- **Federated serving and repository loading are contention-aware.** Bridge
  endpoint incidence indexes replace repeated full bridge scans; cross-shard
  shortest paths build one BFS tree per participating shard; concurrent misses
  for the same cold shard share one materialization (including a shared error,
  with later retry); and declared remote repos load on a dedicated four-thread
  pool while results and errors remain in manifest order.

### Fixed

- **Edge identity keeps relationship context.** Context-bearing connections
  such as GET and POST between the same endpoints no longer collapse into one
  edge. Directed and undirected endpoint canonicalization remain explicit.
- **Deduplication no longer loses source locations.** The primary source site
  stays in the typed fields and every additional extraction site is retained in
  `sites`. Federation now repo-prefixes all of those paths, so MCP source lookup
  cannot resolve an additional site against the wrong member root.
- **Parallel repo loads reject ambiguous cache tags before starting.** Two
  declared repo names that sanitize to the same tag now fail deterministically
  instead of racing on one clone/cache destination.

## [0.6.2] - 2026-07-07

> **Upgrade note:** `extract` and `update` now index structured data/resource
> files (data JSON and `.mcmeta`) as graph nodes by default, so `affected` /
> `predict_impact` / `query_graph` traverse code<->resource links. Pass
> `--no-resources` for the old code-only behavior.

### Added

- **Resource graph (universal, on by default).** Structured data/resource files
  (data JSON and `.mcmeta`) that the config-only JSON extractor used to drop are
  now indexed as one graph node each. Their reference-like string values are bound
  to real targets by a conservative cross-file pass — an existing file path,
  another resource's path-derived logical id (`ns:path`), or a unique code symbol —
  and dropped when nothing resolves, so nothing dangles. A generated resource that
  duplicates a hand-authored one at the same logical path gets a `shadows` edge,
  surfaced by a new `READY-RESOURCE-SHADOW` readiness rule. Because resources are
  now nodes with real edges, `affected` / `predict_impact` / `query_graph` traverse
  code<->resource, so a datapack/resource consumer shows up in a symbol's blast
  radius. Framework-agnostic: a Minecraft `ResourceLocation` is just one instance
  of the universal path-derived-id shape — no MC-specific schemas. Opt out with
  `synaptic extract --no-resources` (also on `update`).
- **Port/readiness audit.** Added the `synaptic-readiness` crate, the
  `synaptic audit readiness` CLI command, and the read-only MCP
  `readiness_audit` tool. The audit ranks graph-linked framework stubs,
  sentinel returns, placeholders, generated-resource noise, and project metadata
  into a structured report
  with severity, subsystem, confidence, remediation, and graph-impact scoring.
  MCP now advertises 30 read-only tools by default, or 31 with `--allow-exec`.

### Changed

- **Ambiguous-name responses hand back a copy-ready qualifier.** When a name
  resolves to several nodes, each candidate is now listed as a paste-ready
  `label@file` reference (or the node id when it has no source file) that
  resolves straight back to that exact node, instead of an id + file column the
  caller had to reassemble into a `name@file` qualifier. Applies to the CLI text,
  the MCP text output, and the structured `candidates` array (new `qualified`
  field, alongside the existing `id`/`file`/`degree`).
- **`query_graph` now applies source-aware node priors.** First-party code stays
  neutral, while docs/rationale, config/resources, tests, and external stubs are
  softly down-ranked when token relevance is otherwise similar. Old serialized
  query indexes load with neutral priors for compatibility.
- **Dropped the `redb` dependency; legacy v1 shards now fail with a rebuild
  hint.** Dependabot proposed bumping redb 2.6 -> 4.1 (#16), but redb 3.0
  removed the file format redb 2.x wrote, so no current redb can open the v1
  shard files the dependency was kept for. The store is derived data, so the
  dependency is gone instead of pinned: a v1 (redb) shard is detected by its
  file magic and rejected with a "re-run `synaptic extract`" error, and
  re-migrating rewrites a legacy file even when its content hash still matches
  the manifest (the unchanged-skip used to keep it). v2 flat-container stores
  (0.6.1+) are unaffected; `SYNAPTIC_STORE=redb` keeps working as the
  historical name for the sharded backend.

### Security

- Bumped the transitive `crossbeam-epoch` dependency to 0.9.20 to clear
  RUSTSEC-2026-0204 (invalid pointer dereference in a `fmt::Pointer` impl).

## [0.6.1] - 2026-07-05

> **Upgrade note:** `extract` and `workspace build` now write the sharded
> store (`synaptic-out/store/`) by default — pass `--no-store` for the old
> graph.json-only behavior. Existing v1 (redb) stores stay readable; the
> next `extract`/`update` rewrites changed shards in the new 15x-smaller
> v2 format, after which older synaptic binaries will refuse that store
> (graph.json workflows are unaffected either way).

### Changed
- **Shard files are 15x smaller and read faster (store format v2).** A shard
  is now a flat container — msgpack header + deflate-compressed chunks of
  1024 records, index blobs compressed alongside, written in one fsynced
  pass — instead of a redb database with one row per node/edge. Measured on
  this repo's real 9.3 MiB graph (release builds): store 16.57 MiB -> 1.06
  MiB (payload 5.95 -> 0.54 MiB, index blobs 2.38 -> 0.52 MiB, structural
  overhead 8.24 MiB -> 0 — half the old file was redb B-tree/page overhead,
  which `compact()` made worse on small files). Criterion (20k nodes / 30k
  edges): shard read −28.8% (p < 0.001), materialize 126 -> 100 ms; write
  unchanged within noise (p = 0.29). CLI via hyperfine: cold build
  342 ± 27 -> 380 ± 28 ms (the deflate cost, mostly offset by dropping the
  index round-trip: shards no longer re-read from disk to build their
  indexes), no-op refresh unchanged (136 ± 2 -> 142 ± 9 ms). v1 redb shards
  remain readable; any rewrite produces v2. `SYNAPTIC_STORE=redb` keeps its
  (now historical) value name.
- **The sharded redb store is built by default.** `synaptic extract` and
  `synaptic workspace build` now write `synaptic-out/store/` alongside
  `graph.json` (pass `--no-store` to skip; the old `--store` flag remains a
  no-op for compatibility), and `synaptic update` refreshes an existing store
  after each incremental rebuild (unchanged shards are hash-skipped). With the
  store present, serving and unscoped reads pick it automatically
  (`SYNAPTIC_STORE` unchanged), so shard-aware serving and its lifted size
  ceiling no longer need any opt-in. The store directory writes its own
  `.gitignore` so binary shards never land in a commit alongside a tracked
  `graph.json`. Measured on this repo (9.3 MiB graph): ~1.6 s first build,
  ~0.3 s no-op refresh, store ~1.8x graph.json on disk.

## [0.6.0] - 2026-07-05

> **Upgrade note:** the sharded store is opt-in and non-regressive. Existing
> `graph.json` workflows are unchanged. To lift the federation-size ceiling,
> build the store (`synaptic workspace build --store`, or `synaptic migrate`
> on an existing federated `graph.json`) and serve with `SYNAPTIC_STORE=redb`
> (or leave it unset: a store at least as fresh as `graph.json` is preferred
> automatically). Callers/impact/paths follow cross-repo bridge edges
> automatically whenever the store holds them — matching what a federated
> `graph.json` always did — and `SYNAPTIC_CROSS_REPO=0` opts a serve or query
> into per-repo isolation.

### Added
- **Shard-aware federated serve (redb store).** `synaptic serve` over a
  multi-shard store no longer materializes the union graph: shards load on
  demand behind a bounded LRU (`SYNAPTIC_SHARD_LRU`, default 8) and every MCP
  tool fans out per shard. Whole-graph aggregates (`graph_stats`, `god_nodes`,
  `query_graph` ranking with a global document-frequency index, communities,
  `structural_search` with deferred `LIMIT`, repo counts) stream shards and
  are provably equal to running on the union; seed tools (callers/callees/
  references/affected/predict/rename/SQL audit) resolve a symbol's owning
  shard and walk there. Removes the federation-size ceiling: memory is bounded
  by the LRU working set, not the sum of all members.
- **Cross-repo walks are on by default, auto-detected.** `SYNAPTIC_CROSS_REPO`
  is tri-state: unset detects (walks follow the bridge exactly when the store
  holds bridge edges — callers/neighbors annotate hits `[cross-repo]`,
  `affected` crosses once and continues in the neighbor repo, `shortest_path`
  may take one bridge hop), `0` isolates per repo, `1` forces on. `graph_stats`
  and a CLI stderr note report the traversal state, and a refused cross-repo
  path names the actual reason (isolation vs no bridge edges at all).
- **Per-shard store guards** are env-configurable: `SYNAPTIC_MAX_SHARD_MB` /
  `SYNAPTIC_MAX_SHARD_NODES` (defaults 2 GiB / 5M nodes; `0` disables),
  alongside the graph.json caps below.
- **Configurable graph safety caps.** The 50 MiB byte cap and 100,000-node cap
  that guard the merge driver, federation, global-store, and remote-subgraph
  loads are now env-overridable: `SYNAPTIC_MAX_GRAPH_MB` and
  `SYNAPTIC_MAX_NODES` (`0` disables a cap; unset or unparseable values keep
  the defaults). Both caps live in one place (`synaptic-core`) instead of
  being duplicated per crate.
- **Write-time cap warnings.** `synaptic extract`/`update` now warn on stderr
  when the `graph.json` they just wrote exceeds the effective caps, naming the
  env override — previously an over-cap graph extracted fine and only failed
  later, at merge or federation time, with no recourse.

### Changed
- Cap violations now report the actual size, the effective limit, and the env
  var that raises it (previously a bare "exceeds the cap" with hard-coded
  limits). Remote subgraph fetches additionally enforce the node cap, matching
  local artifact loads.
- Dependencies (grouped Dependabot update #15): `zip` 7.2 -> 8.6,
  `rust_xlsxwriter` 0.95 -> 0.96, `tree-sitter` 0.26.10, `redis` 1.3,
  `ignore` 0.4.27.

## [0.5.0] - 2026-07-03

> **Upgrade note:** `synaptic update`/`watch` now write `graph.json` (+ the
> provenance manifest) only — pass `--artifacts` if you consume the
> HTML/SVG/GraphML artifacts from incremental rebuilds. A bare `synaptic
> update` now performs a manifest catch-up instead of a full rebuild (use
> `--full` for the old behavior). Re-run `synaptic hook install` to pick up
> the new `post-merge` hook and the root/merge-commit `post-commit` fix —
> hook scripts are embedded at install time and do not update themselves.

### Added
- **Ripple re-resolution**: an incremental update that (re)introduces a symbol
  — a new definition, a rename back, a move to another file — now re-links
  calls from **unchanged** files to it. A per-file called-name sidecar
  (`synaptic-out/.callnames.json`, seeded by `extract` and every rebuild)
  indexes which files reference which names; matching files are re-extracted
  from the AST cache and fed to resolution. Previously such edges only
  reappeared when the calling file itself changed or on a full rebuild.
- **`serve --watch`** (or `SYNAPTIC_SERVE_WATCH=1`): the MCP server embeds a
  filesystem watcher, making staleness detection event-driven — queries skip
  the walk-per-query check and the 1s debounce window entirely, and the first
  query still catches up on pre-watch edits.
- **In-band staleness note**: when more files changed than the autofresh cap,
  every MCP tool result now starts with a `graph is STALE` note telling the
  agent to run `synaptic update` (previously this was only printed to stderr,
  which the model never sees). Clears automatically once the graph refreshes.
- **`post-merge` hook**: `synaptic hook install` now also covers `git merge` /
  `git pull` — including fast-forwards, which fire neither post-commit nor
  post-checkout.
- **`update`/`watch --artifacts`** and **`watch --debounce-ms <n>`** (also
  `SYNAPTIC_WATCH_DEBOUNCE_MS`).

### Changed
- **Bare `synaptic update` now catches up from the manifest diff** — it
  rebuilds exactly the files that changed since the last build instead of
  silently running a full rebuild. `update --full` remains the explicit
  from-scratch rebuild. When no provenance manifest exists (a graph built by
  an older binary), it falls back to a full rebuild rather than trusting a
  freshly bootstrapped baseline that would mask existing drift.
- **`update`/`watch` write `graph.json` + the provenance manifest only** by
  default; the visual/export artifact suite (HTML, SVG, 3D, GraphML, Cypher,
  ...) is regenerated with `--artifacts`. `extract` still writes everything.
- **`watch` catches up at startup** (manifest diff) before entering the event
  loop, so edits made while it wasn't running are ingested; its ignore rules
  now delegate to detect's noise list (dist, .next, coverage, ... are skipped
  like node_modules always was).
- Incremental updates place a small delta's new nodes into their neighbours'
  communities instead of re-clustering the whole graph (exact community-id
  stability while coding); full rebuilds and large deltas still re-cluster.
- Staleness checks and rebuild scans no longer read every file to count corpus
  words (that work only feeds `extract`'s size hint): the serve catch-up walk
  went from O(repo bytes) to O(stats), and one-file updates shed ~7 full-graph
  clone+rebuild passes by chaining the resolution passes on owned vectors.

### Fixed
- **Mid-rebuild edits could go permanently stale**: builds stamped the
  provenance manifest from a *post*-build disk walk, so a file edited while
  the build ran was recorded as seen without ever being extracted. Every build
  path (`extract`, `update`, serve catch-up) now snapshots the manifest before
  extraction and persists it only after the graph is written — and an
  incremental update advances **only the entries it actually ingested**, so a
  file changed on disk but outside the given change set (e.g. an uncommitted
  edit when the post-commit hook lists committed files) still diffs as changed
  later instead of being silently stamped as seen.
- **Full rebuilds no longer union stale edges**: a retargeted call (e.g. across
  a branch switch, where post-checkout runs `update --full`) used to keep its
  phantom old edge because both endpoints were still live. Only
  extraction-owned edges (both endpoints AST) are replaced; edges touching
  semantic/concept nodes are preserved.
- The staleness note clears correctly under `serve --watch` after an external
  `synaptic update` (the cap trip re-arms the event flag), and the embedded
  watcher recovers from dropped/overflowed OS events by falling back to a
  manifest catch-up (`watch` does the same).
- The pending-changes queue is claimed by rename instead of read-then-delete,
  so a path queued concurrently with a drain can no longer be deleted unread;
  a crashed holder's claimed batch is absorbed by the next drain.
- **A transiently unreadable changed file no longer loses its symbols**: a
  read failure (editor/AV lock — common on Windows) kept evicting the file's
  nodes with nothing to replace them. Its prior nodes and edges are now kept,
  and the file is dropped from the manifest so it retries next round.
- **Paths queued while a rebuild ran are drained by that rebuild** instead of
  sitting in `.pending_changes` until some later update.
- `graph.json` is written atomically (temp + rename) everywhere, so concurrent
  readers can no longer observe a truncated file.
- Auto-freshen disables itself for a federated graph (a single-root rebuild
  would corrupt member ids); refresh members individually.
- The watcher filters repo-relative paths, so a checkout under a directory
  named like a noise dir (e.g. `/build/app`) no longer ignores the whole tree.
- **Office ingestion (`--features office`) no longer depends on `calamine`** —
  `.xlsx`/`.ods` workbooks are now read by an in-house zip + XML reader built
  on dependencies already in the tree. This removes `quick-xml` (flagged by
  RUSTSEC-2026-0194 and RUSTSEC-2026-0195 with no fixed upstream path) from the
  dependency graph entirely instead of ignoring the advisories.
- Legacy binary `.xls` workbooks are no longer readable (they came along for
  free with calamine); `synaptic ingest office` now rejects them with a clear
  convert-to-`.xlsx` error.

## [0.4.0] - 2026-07-03

> **Upgrade note:** route (and queue/WS/IPC/event channel) node ids changed to a
> collision-safe canonical format. Graphs extracted with an older binary will
> not join boundary nodes with newly extracted ones -- re-extract every member
> (`synaptic extract .` / workspace rebuild) after upgrading.

### Added
- **HTTP detection for C#, Java/Kotlin, PHP, and Ruby** — previously only
  Python, JS/TS, Go, and Rust participated. Servers: ASP.NET Core minimal APIs
  (`MapGet/...`) and attribute routing (`[HttpVerb]` composed with
  `[Route("api/[controller]")]`), Spring `@GetMapping`/`@RequestMapping` (class
  prefix composed) and JAX-RS `@GET`+`@Path`, Laravel `Route::verb`, Sinatra /
  Rails routes. Clients: `HttpClient`, RestTemplate / `java.net.http` / OkHttp /
  Retrofit, Guzzle and the Laravel `Http::` facade, `Net::HTTP` / Faraday /
  HTTParty.
- **More web frameworks** on the already-supported languages: Go
  gin/echo/chi/fiber verb methods and gorilla `.Methods("X")`; Node any-receiver
  Express and NestJS `@Controller`/`@Get`; Python Django `urlpatterns`, aiohttp,
  and `urllib`.
- **gRPC servers and clients across five languages** (was tonic servers plus
  Rust/Python clients only): Python `Servicer` subclasses / `add_..._to_server`,
  Go `Register<Svc>Server` / `New<Svc>Client`, Java `ImplBase` / `new*Stub`, C#
  `Svc.SvcBase` / `new Svc.SvcClient`, JS `new <Svc>Client` (gated on
  `@grpc/grpc-js`).
- **Message-queue / pub-sub tier** — a new `queue #<topic>` boundary node
  (producers `calls_service`, consumers `handled_by`) for Kafka (kafka-python,
  kafkajs, Spring `@KafkaListener`/`KafkaTemplate`), RabbitMQ (pika, amqplib),
  NATS, Redis pub/sub, and Celery task queues (`@app.task` meets
  `send_task`/`.delay()` at `queue #task:<name>`). Every pattern is gated on its
  library's token so a generic `.publish(`/`.subscribe(` never fires alone.
- **WebSocket breadth**: Go gorilla/nhooyr `Dial` clients, Java/Jakarta
  `@ServerEndpoint` servers, and per-SITE client/server role resolution (a proxy
  file that both serves and connects no longer gets one half inverted).
- **FFI / subprocess breadth**: .NET P/Invoke (`[DllImport]`/`[LibraryImport]`),
  cffi `dlopen`, Rust `#[no_mangle] extern "C"` exports and ctypes call-sites
  meeting at shared `c_symbol:<name>` sinks; Java `ProcessBuilder`/`Runtime.exec`,
  C# `Process.Start`/`ProcessStartInfo`, and C/C++ `system`/`popen` invocations.
- **Vue/Svelte/Astro single-file components are scanned** — `<script>` blocks
  (and Astro frontmatter) now run through every JS/TS detector with byte offsets
  preserved, so SFC `fetch`/`axios`/socket calls produce boundary edges.
- **Shell scripts**: `curl` (honoring `-X`/`--request`) and `wget` are HTTP
  client edges; interpreter (`python tools/x.py`) and `./script` runner lines are
  `invokes` edges that resolve to in-repo files.
- **New boundary affordances**: a SYNQL `node_type` field (boundary stubs are
  selectable at last) and a `dangling-endpoints` pattern that lists one-sided
  boundaries (half-open clients / unconsumed servers). The eval corpus gains
  per-family cross-language fixtures (gRPC, message queue, PyO3, WebSocket) with
  distractors, and calibration now covers every boundary family with a per-type
  two-sidedness breakdown (`two_sided_by_type`).

### Changed
- **Route (and queue / WebSocket / IPC / event-channel) node ids use a
  collision-safe canonical format.** `/a-b` no longer collides with `/a/b`, a
  literal `/users/id` no longer merges with the `/users/{id}` template, and
  whether a shared node is a template no longer depends on file order. Equivalent
  route templates across frameworks (`:id` / `{id}` / `<int:id>`) intentionally
  share ONE node, so a polyglot migration links server-to-server. **Re-extract
  after upgrading** (see the note above).
- **Route keys compose same-file mount/constructor prefixes** (FastAPI
  `APIRouter`/`include_router`, Flask `Blueprint`/`register_blueprint`, Express
  `app.use`, axum `.nest`), and clients are read in their modern spellings:
  template-literal and f-string URLs (`{param}` segments), single-file
  constants, and instance clients (`axios.create({baseURL})`,
  `httpx.Client(base_url=...)`, `requests.Session()`). An absolute URL's
  authority now rides on the client edge as context (`GET api.github.com`).
- **`graph_stats` / `GRAPH_REPORT` count cross-language coupling by relation**
  (HTTP/RPC/FFI/WebSocket/queue/SQL boundaries), so a polyglot single repo shows
  its coupling; the count is no longer described as, or nested under, the
  federated cross-repo count.
- **Cross-language boundary edges are visible everywhere impact is surfaced.**
  Reverse impact (`affected`, `predict_impact`, `affected_tests`) now traverses
  `queries`/`writes_to`/`calls_proc`, so a schema change reaches the code that
  reads or writes the table; `find_callers`/`find_callees` list boundary callers
  (a route/queue/IPC channel that a handler is `handled_by`) instead of
  answering "(none)"; `get_neighbors` renders per-edge context and a
  `[cross-repo]` marker; `query_graph` marks boundary nodes `(boundary)`;
  `describe_node` counts `binds_native`; `predict_edit` flags a wire-contract
  review (not certain breakage) for runtime-boundary dependents; and CLI
  `synaptic path` annotates each hop with its relation and direction.
- **Federation resolves cross-repo boundaries.** The compose step runs the
  PyO3, subprocess-command, and SQL passes over the merged graph (each has both
  sides only once members are federated), dedups external nodes by a typed
  canonical identity (`_node_type` + canonical/case-folded label, so a `command`
  stub never merges with a SQL `table`), flags `cross_repo` independent of member
  composition order, and prefers a same-repo handler match for ambiguous names.
  `dynamic_ref` and the SQL relations joined the cross-repo-flaggable set.

### Fixed
- **Extraction no longer crashes on non-ASCII source near a match** — three
  byte-offset windows (fetch options, WebSocket role detection, axum route spans)
  could split a multi-byte character and panic the whole `synaptic extract` run;
  all are now char-boundary-safe and length-bounded.
- **False positives removed**: an Express `res.send({ type: ... })` response is
  not a WebSocket message; a clap `Command::new("app")` builder is not a
  subprocess; `app.get('port')` is not a route; client wrappers
  (`this.http.post('/x', body)`, `api.post('/x', opts)`) and Ruby request specs
  are not servers; a commented `# curl ...` in a shell script is masked; gRPC
  codegen files are skipped; and Django `include(...)`, consul `kv.Get(...)`,
  C# property `+=`, and non-producer `.send(...)` no longer mint spurious edges.
- **Correctness**: HTTP methods are recorded faithfully (`fetch` options, Flask
  method lists, chained axum `get(h).post(h2)` handlers, Go `PostForm`); JNI
  names demangle (`Java_pkg_Cls_do_1work` → `jni:do_work`); C# events link across
  files; relative (`./api/x`) and concatenated (`'/users/' + id`) client URLs key
  the right route; a bare `/` no longer mints a route node; and
  `resolve_pyo3_imports` is idempotent across the per-member and federation
  passes.

## [0.3.15] - 2026-06-28

### Added
- **`query_graph` flags external-stub nodes.** A node that is an unresolved import
  target or third-party package (`file_type: code` but no source file) now carries
  `external_stub: true` in the structured channel and an ` (external)` marker in the
  text rendering, so an agent does not mistake it for a navigable symbol or try to
  open it with `get_source`. Emitted only when true to keep the output terse.

### Changed
- **Type "size" now uses *effective* LOC across the size-aware surfaces.** A
  class/struct/trait/enum/interface/protocol's bare span covers only its
  declaration -- its methods live in separate nodes (a Rust `impl` block, a C#
  partial class, a Go receiver method), so the declaration span undercounts the
  type's real footprint. `KnowledgeGraph::effective_loc` folds the members reached
  via `contains`/`method` edges in the same file into the count, and the `god-class`
  SYNQL pattern (and `c.loc` in SynQL) now use it. The `god-class` pattern also
  matches any type-like kind (struct/trait/enum/interface/protocol), not just
  `class`, so it fires on Rust/Go codebases instead of silently missing them.
- **`shortest_path` annotates each hop with its connecting relation**
  (`A -[calls]-> B -[uses]-> C`), so a path built from low-signal `references`
  (type) edges is self-evident rather than reading like an authoritative call chain.
  When several edges connect two hops the most meaningful relation is chosen
  deterministically (calls > inheritance > imports > uses/depends > references).
- **`find_callees` notes when a symbol has no in-graph call targets.** If every
  outgoing edge is a type/reference use rather than a real call, the output now says
  so explicitly instead of a bare count that reads like "this function calls N
  things" (calls into std / third-party symbols are not graph nodes).

### Fixed
- **C++ template parameters are no longer mistaken for types.** In a class or
  function template, the placeholder names (`T`, `U`, `Ts`, ...) were emitted as
  real type-reference nodes and edges -- so a templated file produced phantom `T`/`U`
  nodes and bogus `references` edges from every member, return, and parameter that
  used them. The extractor now collects the parameters from every enclosing
  `template_declaration` (so member templates see both lists) and skips them in
  parameter/return/field type references and in base-class clauses. Inheriting from
  a templated base (`class Stack : public Container<T>`) already worked and is
  covered by a new test.
- **Inline Rust unit tests are recognized as test code.** A `#[test]` /
  `#[tokio::test]` / `#[rstest]` function, or any function inside a `#[cfg(test)]`
  module, living in an ordinary `src/` file is now marked as a test (a new
  `_is_test` extraction flag that `Node::is_test` consults), which the source-path
  heuristic alone could not see. Attributes that merely contain the word "test"
  (e.g. `#[doc = "...test..."]`) are not matched.
- **`god_nodes` no longer surfaces Rust standard-library types.** `String`, `Vec`,
  `Option`, `Result`, `Box`, `Arc`, `HashMap`, and other ubiquitous std types
  accumulate large type-reference degree but are not architectural hubs; they are
  now filtered like the existing Python/JSON builtin noise, leaving real first-party
  symbols.
- **SQL auditor skips query text captured in test code.** A query-text finding
  (injection, `SELECT *`, ...) firing on a fixture query inside a `#[test]` /
  `#[cfg(test)]` function (or a test-path file) is a false positive -- the SQL is
  test scaffolding, not a real call site -- and is now suppressed.

## [0.3.14] - 2026-06-24

### Added
- **`find_references` tool + `synaptic references` CLI (alias `refs`):** the
  find-all-references view of a symbol -- every incoming use, including the
  imports, `implements`/`inherits`, and type uses that `find_callers` (calls only)
  omits, plus cross-language coupling and reflection refs, with a per-relation
  breakdown. Aimed at types/interfaces/enums, where a caller-only view comes up
  short. References are to the symbol itself (a type's members are not folded in),
  and a cross-repo use surfaces the same as a local one on a federated graph.
- **File outline via `structural_search`'s new `file` param (and `synaptic search
  --file <path>`):** list every symbol defined in a file, ordered by line, with no
  SYNQL query. The path matches literally and works on a federated graph's
  `tag/`-prefixed paths -- a bare path matches the file across every member, a
  tag-qualified path scopes to one.

### Changed
- **Further reduced the token cost of the AI-tooling surface** by removing
  cross-surface duplication left after 0.3.13, with no behavior change (the trims
  are skill/description text and were verified not to regress tool selection). The
  skill body's parallel CLI and MCP prose lists collapse into one `CLI <-> MCP`
  capability table (~56% smaller); the dynamic-dispatch, `name@file`,
  cross-language, and query-before-grep guidance now has one canonical home with
  short pointers elsewhere; the skill frontmatter description and the always-on
  block are tightened; `SERVER_INSTRUCTIONS` gains a "change impact -- pick by
  input" map that disambiguates the five impact tools in a line; the repeated
  `name@file` parameter text is deduped; and `predict_edit.kind` gains a
  machine-checkable `enum`.

## [0.3.13] - 2026-06-24

### Changed
- **Lower token cost for AI agents that drive the MCP server.** An agent reads the
  text a tool returns plus the server's instructions on every turn, so the default
  output is now leaner without losing any information on request:
  - **`query_graph` is terse by default.** A "where is X" question returns a ranked
    list of the most relevant symbols (no edges) instead of the whole subgraph;
    pass `full=true` for all budget-bounded nodes plus their edges. The default
    `token_budget` drops 2000 -> 1200, the structured channel is bounded to the same
    kept nodes (it was uncapped), and `full` mode caps edges to about twice the
    node count.
  - **`get_neighbors` no longer dumps every neighbour.** A hub is capped (default
    `limit` 50) with a `+N more` summary and a `verbose` escape hatch, mirroring
    `find_callers`/`affected`; the structured mirror gains `total`/`truncated`.
  - **`audit_sql` prints one line per finding by default** (`[severity] rule_id @
    location (conf) title`); `verbose` adds each finding's detail and fix.
  - **Leaner defaults:** `structural_search` `limit` 50 -> 25, `dynamic_hazards`
    `max_results` 100 -> 30.
  - **Trimmed the per-session surface:** the server `instructions` are ~40% shorter
    (every load-bearing fact kept), and repeated boilerplate in the tool/parameter
    descriptions was collapsed.

### Added
- **`SYNAPTIC_CONCISE` environment variable and `serve --concise` flag.** One
  switch lowers the default list/budget sizes across the tools for token-tight
  sessions (`token_budget` 1200, list limits to 20, `dynamic_hazards` to 20,
  `get_community` to 40, `top_n` to 6, `context_lines` to 25). An explicit per-call
  argument always wins, so nothing is lost.

## [0.3.12] - 2026-06-23

### Added
- **Dynamic-dispatch awareness, so "0 dependents" stops reading as "safe to
  change."** Static analysis cannot see reflection, event buses, or fully-dynamic
  dispatch, so a symbol reached only that way looked like a safe leaf. This release
  detects what it can and is honest about the rest:
  - **Event-bus edges (real coupling).** A Node `EventEmitter` (`.emit` / `.on` /
    `.once` / `.addListener`, gated on an `EventEmitter` token to avoid firing on
    ordinary `.on`), DOM `CustomEvent` (`dispatchEvent(new CustomEvent('e'))` +
    `addEventListener('e')`, standard DOM events excluded), and C# events
    (`Foo?.Invoke(` + `Foo += handler`, gated on a real `event` declaration) now
    mint a channel-keyed `event #<name>` boundary node, so a publisher and a
    cross-file/cross-repo subscriber meet in the graph and a handler reached only
    across the bus is no longer a 0-caller island.
  - **Reflection / dynamic-dispatch site catalog.** Computed-member calls,
    `Reflect.*`, dispatch tables, `eval` / `new Function`, dynamic `import()`, .NET
    `GetMethod` / `Activator.CreateInstance`, Python `getattr` / `importlib`, and
    JVM `Class.forName` / `getMethod` are recorded as `dynamic_sites` on the
    enclosing node (no graph-node churn).
  - **Evidence-links.** When such a site's name is a string literal that resolves to
    exactly one symbol, a low-confidence `dynamic_ref` edge is added so the target
    shows up as a (caveated) dependent; ambiguous or computed names stay
    catalog-only.
  - **Honest caveat.** `affected`, `get_node`, `describe_node`, and the CLI
    `affected` / `explain` attach a `dynamic_caveat` to a 0-static-dependent symbol
    whose scope uses dynamic dispatch, and `god_nodes` flags a hub that is reachable
    via reflection.
  - **New `dynamic_hazards` MCP tool** and **`synaptic hazards` CLI command** list
    the sites (filter by `repo` / `path_glob` / `kind` / `target`); `graph_stats`
    now reports `dynamic_sites` / `dynamic_sites_opaque` / `dynamic_refs_linked`.
    Read-only tool count is now 28 (29 with `--allow-exec`).

### Fixed
- **`search_text` now prunes cache-only output directories.** The output-dir
  exclusion added in 0.3.11 matched `synaptic-out/` by name and a custom `--out`
  dir by its `graph.json` + `.manifest.json` pair, but missed a directory holding
  only the AST cache (`cache/ast/v<version>/<hash>.json`) with no graph manifest
  beside it -- for example a predecessor tool's output dir or a cache-only layout.
  Those hash-keyed cache files embed extracted source text, so a content search
  surfaced them as junk hits that buried real source. The walk now also prunes any
  directory containing a `cache/ast/` subtree, a name-independent signature of
  generated cache.

## [0.3.11] - 2026-06-22

A `search_text` quality release: three fixes from a federated-repo field test that
make naive content searches as clean as ripgrep's, plus an installer cosmetic fix.

### Fixed
- **`search_text` no longer searches Synaptic's own output.** The content walk now
  prunes any `synaptic-out/` directory and any custom `--out` dir identified by the
  `graph.json` + `.manifest.json` pair an extraction writes, so generated graph
  artifacts, exports (`.dot`/`.svg`/`.graphml`/...), and `graph.json.bak*` backups
  can no longer drown real source hits (previously a search could return several
  junk artifact matches per real one when the output dir was not gitignored). A
  genuine source file merely named `graph.json` stays searchable.
- **`search_text` repo filter works over a single parent source root.** A
  multi-repo graph served with one `--source-root` and no per-member roots (no
  global-manifest) accepted a `repo` tag from `list_repos` but returned zero files
  and reported `repo: null` on every hit. The filter now falls back to the
  member's subtree under `<source-root>/<tag>` when the graph knows that member,
  and every hit derives its `repo` from the enclosing node (or the graph-path's
  member prefix) instead of null.
- **Installer hook text is plain ASCII.** The generated `.claude/settings.json`
  Read|Glob hook carried a mojibake em-dash in one `additionalContext` string
  (cosmetic; the hook still functioned). It is now `--`, and the
  `generated_artifacts_are_plain_ascii` guard was extended to scan the generated
  hook payloads (Claude `settings.json` hooks and the Codex `HOOK_SCRIPT`) so the
  whole installed surface stays ASCII-only.

### Changed
- **`search_text` matching is now smart-case.** With `case_sensitive` omitted, a
  pattern is matched case-insensitively only when it has no uppercase letter, so
  `todo` stays broad while `TODO`/`FIXME`/`HACK` are precise -- cutting false
  positives such as a lowercase "todos" matching `TODO` or a base64 blob matching
  `HACK`. `case_sensitive=true`/`false` still forces a mode explicitly. Lowercase
  queries are unchanged.

## [0.3.10] - 2026-06-22

A patch release: a security-advisory dependency bump, a cross-platform CI fix, and
agent-surface documentation completeness for the 0.3.9 tooling.

### Security
- **`quinn-proto` 0.11.14 -> 0.11.15** (RUSTSEC-2026-0185, remote memory exhaustion
  from unbounded out-of-order stream reassembly, high). A lockfile-only bump:
  `quinn-proto` is an orphan lock entry not reachable from any workspace crate's
  build graph, so no shipped binary ever contained it; this clears the
  `cargo-audit` / `cargo-deny` advisory gate.

### Fixed
- **Cross-platform jail test.** `jail_allows_inside_rejects_escape` asserted that a
  path escaping the source-root resolves to `Missing`, which holds on Windows (the
  escape target does not exist) but not on Linux, where `../../etc/passwd` exists and
  the jail correctly returns `OutsideRoot`. The jail code was always sound -- it
  never returns `Found` for an escape -- so the test now accepts either rejection
  reason; the precise Missing-vs-OutsideRoot split stays pinned in a sibling test
  with a controlled file. Unblocks the `test (ubuntu-latest)` CI job.

### Documentation
- **The generated skill now describes the 0.3.9 tooling.** The installed Synaptic
  skill (`synaptic skill install`) gained `search_text` -- the text complement to
  `structural_search`, with each hit attributed to the enclosing node -- in its MCP
  tool list and its trigger description, and notes `get_source`'s new `file` +
  `lines` arbitrary-range read. (`show_sites` stays deferred to the tool schema, like
  other per-tool parameters.) Snapshots re-blessed.
- **`describe_node`'s `outputSchema` is complete.** It now declares the `members` /
  `member_count` fields it returns for a class/type and the `ambiguous` /
  `candidates` / `query` disambiguation shape it returns for an unresolved name,
  matching `get_node` / `affected`.

## [0.3.9] - 2026-06-22

Two themes. First, two new "reach for the graph, not a shell" capabilities that
close the gaps where an assistant would otherwise drop to `grep` or open files by
hand: a content/text search that attributes every hit to a graph node, and
source-reading that no longer needs a symbol to anchor on. Second, the
federated-workspace audit follow-up: the 0.3.8 assessment was generated against
the **0.3.7** binary, so several of its findings were already fixed in 0.3.8
(`audit_sql` duplicate findings, the `working_changes_impact` clean-vs-not-a-repo
message, and cross-tool ambiguity refusal — all re-confirmed correct here); this
round fixes the genuinely open ones, each re-verified live on the same 9-repo
workspace.

### Added
- **`get_source` reads an arbitrary file range, not just a symbol.** Pass `file`
  (repo-relative, or `tag/path` in a federated graph) with an optional `lines`
  range (`"108-140"`, or a single `"108"` for a `context_lines` window) to read a
  region that is not a single node — a config block, or the lines around a
  `search_text` hit — through the same source-root jail and federation routing as
  the symbol path. Reading logic no longer requires a graph node to anchor on.
- **`show_sites` on `find_callers` / `find_callees` / `get_neighbors`.** With
  `show_sites=true`, each listed caller/callee/neighbor is annotated with the
  actual source line of its call/reference site (`at file:line: <code>`, read from
  the jail, long lines truncated). This turns the graph's "A calls B" into "A
  calls B at this exact line" without a second `get_source` round-trip — the
  precise bridge from a structural edge to the code that judges it (e.g. is the
  call's argument-building regex fragile?). Text-view enrichment; the structured
  mirrors are unchanged.
- **`search_text` — content (text/regex) search over the source, attributed to the
  graph.** `structural_search` matches the graph (kinds, loc, fan-in/out, symbol
  names) and by design cannot see file content, so anything text-shaped — string
  literals, config values, log messages, a TODO's wording, error strings, magic
  numbers — was invisible to the MCP surface. The new `search_text` tool fills that
  gap: a regex (or `literal`) search over the actual source files, case-insensitive
  by default, that **routes through the same per-repo source roots and containment
  jail as `get_source`**. On a federated/monorepo graph it searches every member
  (honoring each repo's `.gitignore`/`.synapticignore`), or one member via `repo`;
  filter files with `path_glob`, cap with `max_results`. Crucially, **every hit is
  attributed to the graph node whose body encloses it**, so a matched line is a
  pivot: from a fragile regex literal straight to `affected`/`find_callers` on the
  function that contains it. Backed by ripgrep's matcher/searcher core
  (`grep-searcher`/`grep-regex`) over the `ignore` walker. Text + structured
  (`{pattern,total,truncated,files_scanned,hits:[{repo,file,line,col,match,line_text,node}]}`);
  the read-only tool surface is now 27 tools, 14 of them structured. The server
  instructions and tool descriptions tell agents to reach for `search_text` (not a
  raw shell grep) for text-shaped questions, since only it knows the federation
  topology and resolves each hit back to a symbol.
- **Electron IPC modelled as cross-process edges.** A main-process handler invoked
  only over IPC (`ipcMain.handle('ch', fn)`) previously had no static caller, so it
  read as dead code. The cross-language pass now emits a channel-keyed `ipc #<ch>`
  boundary node: senders (`ipcRenderer.invoke`/`send`, `webContents.send`)
  `calls_service` it and handlers (`ipcMain.handle`/`on`, renderer `ipcRenderer.on`)
  are reached from it via `handled_by`, so renderer↔main calls connect in the graph
  and `affected` / `find_callers` / `shortest_path` cross the IPC boundary. Mirrors
  the existing WebSocket/socket.io detector; JS/TS, gated on an Electron IPC API
  token. This narrows the static-analysis coverage gap noted in 0.3.8 — custom
  event buses and reflection are still not traced.

### Fixed
- **Calls inside anonymous callbacks are no longer lost.** The generic call pass
  stopped at every nested function boundary, so a call made inside an inline arrow /
  function expression (`ipcMain.handle('ch', () => helper())`, `arr.map(x => f(x))`,
  `.then(() => g())`) was never attributed to anything — leaving the callee with 0
  callers. It now recurses into anonymous callbacks (whose calls belong to the
  enclosing named function) while still skipping named nested functions that get
  their own node. Combined with the IPC channels above, this is what makes a
  delegated IPC handler reachable: on the live workspace, a helper that an IPC
  handler delegated to went from **0 callers** to its real caller, and `affected`
  on it now reaches the renderer-side invoke sites across the IPC boundary
  (renderer/preload → `ipc #<channel>` → handler-registrar → the helper).
- **Class/type reverse-impact no longer collapses to ~0.** A class's callers attach
  to its methods, not the bare type symbol, so `affected`, `find_callers`,
  `find_callees`, and `describe_node` on a class previously returned almost nothing
  — reading as "safe to change" when it is not. They now fold the type's members in
  (seeding the reverse-impact walk from the class plus its methods) and label the
  result as aggregated; `describe_node` lists a type's members. On the live
  workspace, `affected` on the top god-class went from an empty/ambiguous result to
  **65 dependents** with a `class with 124 members; impact aggregated…` note. The
  fold is shared by the MCP server and the CLI `affected` command (new
  `synaptic-query` `type_member_ids` / `affected_rooted` / `affected_including_members`).
- **Structured `affected` no longer reports a misleading `total: 0` for an
  ambiguous name.** The structured channel used a silent best-match resolver while
  the text refused; both now use the unified resolver. An unresolved name returns
  `resolved: false` with `ambiguous` + `candidates` (or `found: false`), matching
  the text. `describe_node`'s structured mirror does the same and adds
  `members` / `member_count` for a type. `get_node` gained a `structuredContent`
  mirror too (node metadata, or the same `ambiguous`+`candidates` shape), so it is
  no longer the one name-taking tool that surfaces ambiguity as text only
  (13 tools now carry an `outputSchema`).
- **Edge `source_file` paths are normalized to forward slashes.** Node paths were
  normalized at build but edge paths kept Windows backslashes, so `audit_sql`
  locations rendered `repo/src\main\file.js`. Fixed at graph build (the root cause —
  a freshly extracted member now has zero backslashed edge paths) and again when an
  `audit_sql` finding is emitted, so a pre-existing graph also renders clean paths.
- **`get_community` no longer lists noise.** External import stubs (third-party
  packages with no source file) and non-code-symbol nodes (captured TODO/NOTE
  rationale comments, markdown headings, config keys) are filtered from community
  membership, so a community lists the real code symbols of a subsystem.

### Changed
- **`god_nodes` degree is labelled as centrality, not dependence.** The text, the
  output schema, and the server instructions now state that `degree` counts all
  connections (including a class's member edges) — structural size/centrality, not
  how many things depend on a symbol; use `affected` for blast radius. Per-row
  wording changed from `N edges` to `N connections`.
- **`list_repos` surfaces per-repo freshness.** When a `workspace-state.json` sits
  beside the graph, each repo carries a `source_hash` (text `src <hash>`, structured
  `source_hash`) so per-repo drift in a federation is visible.
- **MCP instructions document the static-analysis coverage limit.** A handler
  reached only via runtime dispatch (IPC / WebSocket / event bus / reflection) can
  show 0 callers; the server instructions, the `affected` / `find_callers`
  descriptions, and the wiki now call this out alongside the inline-unit-test note.

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
