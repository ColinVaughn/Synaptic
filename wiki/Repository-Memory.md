# Repository Memory

Synaptic repository memory is a durable, revision-aware overlay on the code
graph. It preserves what happened around the graph -- changes, failures,
regressions, decisions, conventions, and verification outcomes -- without
turning summaries into ungrounded graph facts.

The graph remains the current structural truth. Memory records live separately
under `.synaptic/memory/records/` and join back to graph symbols through
revision-aware anchors. Every record must cite at least one source artifact.

## Why an overlay

`graph.json` is a replaceable snapshot of the repository at one revision.
Repository memory has a different lifecycle: it must survive graph rebuilds,
retain obsolete observations for audit, and distinguish an active decision from
one that was superseded. Keeping the stores physically separate prevents stale
history from masquerading as a current static dependency while still allowing
symbol-scoped retrieval.

```
issues / commits / reviews / agents / ADRs
                    |
                    v
       .synaptic/memory/records/*.json
                    |
        revision-aware SymbolAnchor
                    |
                    v
         current Synaptic code graph
```

## Data contract

A memory record contains:

- a stable ID derived from repository identity plus an idempotency key;
- a typed kind, title, compact summary, repository, branch, commit, and
  occurrence/recording timestamps;
- one or more source artifacts, each with a stable URI and optional revision
  and digest;
- affected symbol anchors containing node ID, label, source path, repository,
  revision, and anchor confidence;
- typed old/new symbol transitions for inferred renames across revisions;
- verification status, commands, and notes;
- memory confidence, lifecycle, access scope, private-record owner, and typed
  links to other memories.

Records are immutable observations. A correction is another record with a
`supersedes` link. Normal searches hide records that are retracted, explicitly
superseded, or targeted by an active `supersedes` link.

Supported kinds cover the four memory families:

- **episodic:** `change_episode`, `issue`, `incident`, `pull_request`,
  `review_finding`, `ci_run`, `regression`, `release`, `customer_report`,
  `agent_task`;
- **semantic:** `semantic_summary`, plus decisions and invariants that describe
  stable subsystem responsibility;
- **procedural:** `procedure` and `convention`;
- **negative:** `failed_attempt`, `regression`, rejected `review_finding`, and
  relevant `incident` records.

Typed links include `changed`, `introduced`, `fixed`, `regressed`,
`motivated_by`, `reviewed_by`, `failed_because`, `verified_by`, `supersedes`,
`implements_decision`, `violates_decision`, `similar_to`, `observed_in`,
`rolled_back_by`, `applies_to`, and `derived_from`.

## Persistence guarantees

Each record is serialized independently as versioned JSON. Writes create and
sync a unique temporary file before an atomic rename. Repeating the same
repository/idempotency key with identical content succeeds as
`already_present`; outcome APIs ignore only their server-generated occurrence
and recording timestamps when checking a later retry. Reusing the key with
different source-grounded content fails. This makes hook and agent retries safe.
Schema-version-only differences are normalized during retry comparison, so an
older on-disk record remains idempotent after an upgrade.

The local store is deliberately not a vector database. MVP retrieval is
deterministic lexical ranking plus exact/substring symbol-anchor filtering,
confidence, verification, recency tie-breaking, kind filters, and lifecycle
filtering. The source artifact remains visible in every human-readable MCP
result.

## Automatic and manual capture

`synaptic memory ingest <revision>` resolves the exact commit, records its
subject, timestamp and changed files, and anchors current graph symbols whose
source files changed. Git rename/copy detection retains typed old/new path
lineage, so history remains searchable through a pre-move path. Structurally
equivalent deleted/added declaration lines are also compared
against the current graph. A one-identifier transition such as
`refresh_token` to `refresh_session` produces revision-aware old/new symbol
anchors, making the episode searchable through either name.

Installed `post-commit` and `post-merge` hooks capture
`HEAD` into an environment variable before starting background work, then run:

```sh
synaptic update
synaptic memory ingest "$SYNAPTIC_COMMIT"
synaptic memory refresh
```

Capturing the SHA before the asynchronous work prevents a later commit from
being attributed to the earlier hook event. Git ingestion is idempotent by
commit SHA.

`synaptic memory ingest-docs` scans explicit ADR/decision and
procedure/runbook/playbook directories plus root `CONTRIBUTING.md`,
`DEVELOPMENT.md`, `RELEASING.md`, and `AGENTS.md` conventions. It uses the
first heading as the title, the first prose paragraph as the compact summary,
and the file bytes as a BLAKE3 source digest. An optional metadata line grounds
the document to current graph symbols:

```md
Synaptic-Symbols: MemoryStore, affected
```

A changed document creates a new immutable record that supersedes the previous
record from the same source URI.

`synaptic memory refresh` combines that extraction with deterministic semantic
summaries for every labeled graph community. Summaries cite the graph artifact,
retain its commit, anchor representative symbols, and supersede the prior
community summary when the graph changes.

External systems use `synaptic memory import-artifacts --file <JSON>`. The
`synaptic.memory-artifacts/v1` envelope supports issue, pull-request, review,
CI-run, incident, release, customer-report, regression, failed-attempt, and
agent-task records. Each item requires an external ID, source URI, repository,
timestamp, title, and summary; its exact canonical payload is digested.
Changed versions of the same source supersede earlier observations.

Agents and CI can record a verified outcome explicitly:

```sh
synaptic memory record \
  --idempotency-key agent-task-42 \
  --title "Refresh lock attempt failed" \
  --summary "Holding the session mutex across network I/O deadlocked refresh." \
  --outcome failed \
  --source-uri agent://task/42 \
  --symbol refresh_token \
  --verification-status failed \
  --verification-command "cargo test auth_refresh"
```

Use a stable issue, task, run, or patch identifier as the idempotency key.
`source-uri` should be a resolvable artifact locator, not a description invented
for the memory.

## MCP surface

When `synaptic serve` starts, it opens `<source-root>/.synaptic/memory`. Five
read-only tools are always added:

| Tool | Intended question |
| --- | --- |
| `search_memory` | What source-grounded history is relevant to this query? |
| `explain_history` | What happened previously around this symbol or file? |
| `find_similar_change` | Which earlier changes had similar intent and outcomes? |
| `known_pitfalls` | Which failures, regressions, incidents, or review findings apply? |
| `explain_decision` | Which active ADRs, invariants, conventions, or procedures govern this? |

`record_change_outcome` is not advertised or callable unless the trusted
operator starts the server with `--allow-memory-write`. It requires a stable
idempotency key and source URI. This gate is independent of `--allow-exec`.

All six tools return a concise evidence rendering and typed
`structuredContent`. The record response resolves affected symbol names against
the loaded graph when possible; unresolved names remain lower-confidence raw
anchors instead of being discarded.

`get_node`, `affected`, and `predict_edit` append a bounded repository-memory
evidence block when their subject has anchored history. `predict_impact`,
`working_changes_impact`, and `get_pr_impact` aggregate the same evidence over
their entire changed-file set, deduplicate immutable records, and retain the
matching subjects. Pitfalls sort first, followed by decisions and ordinary
history; every row cites its source. Typed responses mirror the block under
`structuredContent.memory_evidence`.

## Indexed retrieval

The store builds a lazy in-process index over immutable records. Text tokens,
symbol/path tokens, and kinds narrow the candidate set before exact scoring and
lifecycle filtering. Parsed records and indexes are reused across calls.

Each query fingerprints the sorted record filenames, lengths, and modification
times. A record appended by a hook, CLI, or another MCP process invalidates the
cached view deterministically on the next query. The index changes performance,
not ranking or result semantics.

`synaptic memory compact` writes a checksum-verified
`index.compact-v1.json` snapshot. Immutable record files remain the audit source
of truth; a process uses the snapshot only while its full record fingerprint
matches, and safely falls back after any append.

Reads can federate additional stores with `memory search --peer <STORE>` or
`serve --memory-peer <STORE>`. Replicated identical IDs deduplicate; divergent
content under one ID fails closed. `memory export` and `memory sync` exchange a
checksummed `synaptic.memory-bundle/v1` file, preserving source, lifecycle,
ownership, and access scope while filtering exports and imports through the
configured principal.

## Access and trust boundary

Access scope is stored on every record (`private`, `repository`, or a named
workspace). Private records retain an owner. Library, CLI, and MCP reads filter
before totals, candidate diagnostics, supersession, or rendering, so hidden
records do not leak through counts or lifecycle effects. Writes enforce the same
policy.

Omitting principal options keeps trusted local operator behavior. A restricted
server uses `--memory-principal`, repeated `--memory-repository-claim` and
`--memory-workspace-claim`, and optional `--memory-allow-private`. The principal
is server configuration, never a caller-controlled tool argument. Continue to
use API keys and filesystem isolation for HTTP transport security.

## Evaluation

`synaptic memory eval --manifest <JSON>` runs
`synaptic.memory-benchmark/v1` cases against source URIs and reports recall@1,
recall@5, mean reciprocal rank, mean candidate fraction, and per-case misses.
`--min-recall-at-5`, `--min-mrr`, and `--max-candidate-fraction` make the report
a CI gate. The manifest is corpus-neutral, so historical repository incidents
or SWE-bench-style task sets can supply the cases.

Memory summaries are evidence pointers, not authority. Agents should inspect
the cited artifact when a decision is consequential, consider confidence and
verification, and include superseded records only when explicitly investigating
history.

## Implementation completeness

The table is the current implementation contract, not a roadmap presented as
finished work.

| Capability | Status | Current boundary |
| --- | --- | --- |
| Versioned record model, source requirement, lifecycle, typed links, confidence, verification, scope | Complete | Library model and JSON schema are implemented. |
| Durable local persistence and idempotent retry | Complete | Atomic per-record JSON, schema-upgrade retry compatibility, and verified compact snapshots. |
| Deterministic lexical/symbol retrieval and supersession filtering | Complete | No embedding or learned reranker. |
| Exact-revision Git commit ingestion | Complete | Captures commits and graph anchors for changed files. |
| Git file rename/copy lineage | Complete | Stores typed old/new paths and retrieves an episode through either path. |
| Post-commit and post-merge capture | Complete | Installed hooks record the captured SHA and refresh semantic/procedural memory after graph update. |
| CLI memory workflow | Complete | Ingest, import, refresh, authorized/federated search and record, compact, export/sync, status, and eval are implemented. |
| Five read MCP tools plus gated outcome writer | Complete | Tool contracts and structured responses are integration-tested. |
| Current-graph symbol grounding | Complete | MCP outcome writes resolve nodes; Git ingestion anchors changed-file nodes. |
| Rename/move identity across revisions | Complete | File lineage and conservative one-identifier symbol rename inference retain old/new revision anchors. |
| Access-scope enforcement | Complete | Principal policy covers private ownership, repository/workspace claims, reads, diagnostics, supersession, exports, imports, and writes. |
| Issue, PR, review, CI, incident, release, agent-task, and ADR adapters | Complete | Canonical external envelopes and repository document adapters are source-digested, anchored, idempotent, and superseding. |
| Generated semantic subsystem summaries and procedural extraction | Complete | Root conventions and explicit procedure roots are extracted; hooks refresh deterministic community summaries. |
| Memory evidence injected into graph-impact responses | Complete | Single-symbol, explicit file-set, working-tree, and PR-impact responses attach bounded deduplicated evidence. |
| Large-store inverted index, compaction, federation, and team sync | Complete | Cached indexes, external invalidation, verified snapshots, collision-safe peers, and checksummed bundles are implemented. |
| Repository-memory benchmark/evaluation harness | Complete | Corpus-neutral localization cases report recall@1/@5, MRR, selectivity, misses, and enforceable CI thresholds. |

### Second implementation slice

The second implementation pass is complete for its four scoped deliverables:

| Slice | Status |
| --- | --- |
| Git file rename/copy history with old-path retrieval | Complete |
| Source-grounded ADR/procedure discovery, hashing, anchoring, retry, and supersession | Complete |
| Bounded memory evidence on symbol and single-symbol impact responses | Complete |
| Cached inverted retrieval with deterministic external-write invalidation | Complete |

### Completion slice

The final implementation pass completed the remaining production gaps:

| Slice | Status |
| --- | --- |
| Revision-aware symbol rename inference | Complete |
| External issue/PR/review/CI/incident/release/agent adapters | Complete |
| Generated semantic summaries and automatic refresh | Complete |
| Principal-level private/repository/workspace enforcement | Complete |
| File-set, working-tree, and PR evidence aggregation | Complete |
| Verified compaction, federation, and checksummed team sync | Complete |
| Retrieval benchmark metrics and CI gates | Complete |
