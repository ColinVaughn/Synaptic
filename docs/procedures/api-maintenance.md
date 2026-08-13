# API maintenance

Synaptic API Maintainer detects source-grounded breaking API changes, joins them to
the repository's current API usage graph, generates a bounded repair in an isolated
worktree, verifies it, and can open or update one draft pull request. Stripe is a
configured adapter, not a special orchestration path; the same pipeline is exercised
by the Pager fixture vendor.

The feature is opt-in. It never auto-merges, and the default MCP server remains
read-only. `init`, `inventory`, `scan`, and `impact` do not modify repository source or
GitHub. `repair` writes only in an isolated worktree. `publish` is the only stage that
should receive repository write credentials.

## Quick start

```text
synaptic api init
# Edit .synaptic/api-maintenance.toml and add an enabled vendor.
synaptic extract .
synaptic api discover --json
synaptic api coverage --json
synaptic api check-plan --root . --json --require-complete
synaptic api inventory --json
synaptic api scan --offline --json
synaptic api impact --event <event-id> --json
synaptic api repair --event <event-id> --dry-run --json
```

`coverage` is the configuration-independent honesty check. It reads the existing
graph, package manifests/SBOMs, and optional redacted evidence and reports every
literal external HTTP call, SDK call, non-HTTP boundary, service record, exact API
binding, and dynamic-dispatch hazard it can currently observe. It
does not require `.synaptic/api-maintenance.toml`: unknown providers remain
`observed` with explicit provider/model/source/binding/version gaps. A configured
owner without an exact operation mapping remains `identified`; only an existing
high-confidence `uses_api` edge reaches `bound`. `complete` means that the evidence
present in that graph has no unresolved coverage gaps, not that unexecuted dynamic
behavior or an undocumented provider change has been proven absent.
Runtime and optional dependencies without SDK-call evidence remain visible as
`usage_classification` gaps; development dependencies are inventory-only negative
controls. Use `synaptic api coverage --require-complete` in policy/CI to exit non-zero
while any reported gap remains.

Every extraction also writes `contract-discovery.json` and a disabled, report-only
`candidate-profile.toml`. Review the inferred source/vendor identities before moving
an entry into `.synaptic/api-maintenance.toml`; discovery never enables monitoring by
itself. Contract readers auto-detect OpenAPI, AsyncAPI, GraphQL SDL/introspection,
Protobuf/gRPC source, WSDL, Smithy, and OpenRPC by content. Formats without a complete
native compatibility policy are explicitly partial and review-only.

Sanitized OTLP can be supplied with repeated `--runtime-evidence <file>` arguments.
Versioned synthetic canary/error summaries use `--behavioral-evidence <file>` and may
create review candidates only. Conventional evidence can be placed under
`.synaptic/runtime-evidence/` and `.synaptic/behavioral-evidence/`. Neither importer
persists arbitrary attributes, headers, queries, payloads, or credentials.

A non-dry repair requires an agent command containing `{request}`. The command reads
the generated request and must emit only:

```json
{"unified_diff":"diff --git ...","rationale":"why this is the minimal migration"}
```

Patch generation and project commands run with network disabled. A platform network
guard is mandatory and is passed as repeated `--network-guard` argv values. Synaptic
fails closed if no guard is configured; it does not claim that an environment is
isolated merely because credentials were scrubbed.

## Configuration

`.synaptic/api-maintenance.toml` is versioned and validated before use:

```toml
schema = 1
mode = "draft_pr"
base_branch = "main"
max_files = 12
max_changed_lines = 800
max_attempts = 3
max_risk_score = 80
allowed_paths = ["src/", "tests/"]
allow_workflow_changes = false
allow_generated_changes = false
require_resolved_version = true
require_graph_invariants = true
require_tests = true

[commands]
check = "npm run typecheck"
test = "npm test -- {files}"
policy = ["npm run lint", "npm run security-check"]

[publish]
labels = ["dependencies", "api-migration", "synaptic"]
reviewers = ["platform-team"]

[[vendors]]
id = "stripe"
enabled = true
packages = ["npm:stripe", "pypi:stripe"]
hosts = ["api.stripe.com"]
auto_repair_confidence = 0.92

[[vendors.sdk_bindings]]
package = "npm:stripe"
member = "customers.create"
method = "POST"
path = "/v1/customers"

[[vendors.sources]]
kind = "open_api"
uri = "https://vendor.example/openapi.json"
affected_versions = ">=10.0.0,<20.0.0"
max_bytes = 10485760
min_poll_interval_seconds = 300

[[vendors.sources]]
kind = "changelog"
uri = "https://vendor.example/changelog.json"
max_bytes = 1048576

[[vendors.sources]]
kind = "package_release"
uri = "https://vendor.example/sdk-surface.json"
package = "npm:stripe"
affected_versions = ">=10.0.0,<20.0.0"

[[vendors.sources]]
kind = "static_contract"
path = "fixtures/vendor-openapi.json"
affected_versions = "*"
```

`webhook` reads a local, bounded JSON envelope with `schema` (currently `1`), `vendor`,
`revision`, `occurred_at`, `content_type`, `content_digest`, and `contract`. The vendor
must match the configured source, the content type must be JSON/YAML, unknown fields
are rejected, and the BLAKE3 digest of the embedded contract must match before the
artifact enters normalization.
Remote sources support conditional requests, explicit content types and byte caps,
per-source polling intervals, and an integrity lock. Changelog prose is sanitized and
can produce only a review candidate unless structured evidence corroborates it.

To add a vendor, supply package/host matchers, source data, and SDK member mappings.
Use `member = "default"` for a directly invoked JavaScript default import or
CommonJS export; the mapping remains stable when the local import alias changes.
For Go, use the full imported package path in `sdk_bindings.package` and the
module path from `go.mod` in `vendors.packages`. Subpackage calls inherit the
resolved version from the longest segment-aligned module prefix.
For Rust, use the Cargo package coordinate in both locations. Member chains use
dot-separated canonical `use` paths: for example,
`use serenity::prelude::*; Client::builder(...)` maps to
`cargo:serenity#prelude.Client.builder`. Extraction resolves explicit aliases,
brace imports, conservative glob-prelude types, typed parameters, and clients
created by associated functions. Standard-library types and locally declared
modules/items are excluded before vendor binding.
For ecosystems whose registry coordinate differs from the namespace used in
source, declare one or more `imports` on each rule. Namespace matching is
segment-aligned and ambiguity fails closed:

```toml
[[vendors.sdk_bindings]]
package = "maven:com.stripe:stripe-java"
imports = ["com.stripe"]
member = "StripeClient.v1.customers.create"
method = "POST"
path = "/v1/customers"
```

SDK call extraction covers npm JavaScript/TypeScript (including Vue, Svelte and
Astro scripts), PyPI Python, Go modules, Cargo Rust, Maven/Gradle Java, Kotlin,
Groovy and Scala, NuGet C#/Razor, Composer PHP, RubyGems, SwiftPM/CocoaPods,
Dart pub, Hex Elixir, LuaRocks, Julia Pkg, Zig packages, PowerShell modules,
Conan/vcpkg C/C++, CocoaPods Objective-C, Salesforce Apex, NuGet-backed Pascal,
Fortran fpm, and explicitly created COM objects in ASP. Inventory reads the
corresponding manifests and lockfiles, plus CodeQL packs. JSON, YAML, HCL, SQL,
Verilog, CodeQL query sources, project metadata, and Markdown remain structural
inputs rather than SDK-call languages; Synaptic does not fabricate calls for
them.
Do not add vendor branches to extraction, relevance, repair, or publishing. If a
vendor exposes a structurally different source, implement the narrow `VendorAdapter`
contract and reuse the scan/event pipeline.

## Build and test discovery

`api check-plan` recursively inventories every independently verifiable project. It
does not stop after the first root marker. The output records each project root,
manifest provenance, exact check/test commands, uncovered capabilities, scan count,
and whether a safety limit truncated discovery. `--require-complete` exits non-zero
when any capability is unresolved; it previews commands but never executes them.

Automatic detection currently covers Cargo, Go modules, Python/pytest, npm, pnpm,
Yarn, Bun, Deno, Gradle, Maven, SBT, Mill, .NET solutions/projects, SwiftPM,
Composer, Bundler/Rake/RSpec, Dart, Flutter, Mix, Julia Pkg, Zig, Fortran fpm,
CMake/CTest, Meson, Make, PowerShell/Pester, LuaRocks/Busted, Bash/Bats,
Pascal/Lazarus/Delphi, Terraform, and CodeQL packs. Package-manager scripts are read
from their manifests; an npm placeholder test is not accepted as a real suite.
Native workspaces and solutions own their child projects, avoiding duplicate builds.

Discovery is deterministic and bounded (depth, directory, project, manifest-size,
and symlink limits). Dependency trees, VCS metadata, build outputs, and generated
caches are skipped. Ambiguous solutions/build systems, missing test targets, unsafe
filenames, unsupported source-only projects, and environments that need credentials
(such as Apex) become localized gaps. Explicit `[commands].check` and
`[commands].test` values resolve the corresponding gaps. A relevant unresolved gap
is `inconclusive`, never a successful verification.

## Graph model and applicability

The overlay adds these vendor-neutral facts:

| Source | Relation | Target | Impact traversal |
| --- | --- | --- | --- |
| Code symbol | `uses_api` | `api_operation` | Yes |
| `api_operation` | `provided_by` | `api_vendor` | No |
| Existing package node | `sdk_for` | `api_vendor` | No |

Direct HTTP bindings require a literal absolute URL plus configured host, method, and
path. SDK bindings require an imported package plus either a static member mapping or
exact generator-supplied vendor/protocol/method/path/operation metadata. Similar names
and computed member access remain unresolved. An event can auto-repair only
after vendor, version, observed usage, confidence, and allowed-scope gates all pass.
Unknown versions or ambiguous ownership produce `review_required`; an unused SDK or an
out-of-range installed version produces `not_applicable`.

Reverse impact starts at only the changed operation nodes, then selects wrappers,
callers, tests, history co-changes, repository memory, and dynamic-dispatch hazards.
The resulting repair brief has hard file, source-byte, evidence, and graph-node budgets.

## Verification and publication

The verifier requires all five gate groups to pass; `inconclusive` is not success:

1. patch application, path/size/permission policy, dependency consistency;
2. incremental/full graph parity and API binding invariants;
3. graph-selected tests and every relevant detected/configured build;
4. configured lint, schema, integration, and security policy commands;
5. final risk forecast, cycle detection, and public-API preservation.

Patch policy rejects traversal, symlink escape, submodules, binaries, executable-bit
changes, secrets, protected workflow/ownership/security files, unrelated generated
artifacts, and unreasoned scope expansion by default. Up to three retries receive only
the immutable brief, prior patch, and bounded failure report.

Before an agent is invoked, the isolated base worktree runs the same relevant project
checks and tests. A failing command (including unavailable tooling) stops repair as a
failure; unresolved discovery stops it as inconclusive. This prevents an agent patch
from being blamed for pre-existing failures. After patch application, the
graph-selected projects and all policy gates run again. Commands execute in their
detected project roots with network disabled and bounded output/time.

Publishing commits and pushes the deterministic
`synaptic/api/<vendor>/<event-prefix>` branch, then creates or updates a draft PR with
the hidden event/base marker. A run is keyed by repository identity, base SHA, event ID,
and policy digest. Neither a failed nor an inconclusive run is publishable.

## Artifacts and schemas

All paths are relative to the repository root:

| Path | Purpose |
| --- | --- |
| `.synaptic/api-maintenance/events/*.json` | Immutable normalized breaking events |
| `.synaptic/api-maintenance/contracts/<vendor>/*.json` | Canonical contracts by digest |
| `.synaptic/api-maintenance/artifacts/*.bin` | Bounded raw source cache by digest |
| `.synaptic/api-maintenance/source-lock.json` | Revision, digest, validators, and poll time |
| `.synaptic/api-maintenance/runs/*.json` | Idempotent run ledger and terminal state |
| `synaptic-out/api-maintenance/coverage.json` | External-surface ledger and evidence windows |
| `synaptic-out/api-maintenance/contract-discovery.json` | Parsed and explicitly rejected local contracts |
| `synaptic-out/api-maintenance/candidate-profile.toml` | Disabled review overlay for discovered contracts |
| `synaptic-out/api-maintenance/behavioral-evidence.json` | Redacted canary/error review evidence when supplied |
| `synaptic-out/api-maintenance/<run>/repair-brief.json` | Agent input boundary |
| `synaptic-out/api-maintenance/<run>/baseline-verification.json` | Pre-agent build/test baseline |
| `synaptic-out/api-maintenance/<run>/repair-outcome.json` | Attempts and failures |
| `synaptic-out/api-maintenance/<run>/verification.json` | Conclusive gate results |
| `synaptic-out/api-maintenance/<run>/proposed.patch` | Verified unified diff |
| `synaptic-out/api-maintenance/<run>/pr.json` | Draft PR identity and action |

Versioned JSON Schemas live in `crates/synaptic-api/schemas/`. The offline replay
corpus, labeled Node/Python repositories, hostile text, examples, and two vendor
contracts live in `crates/synaptic-api/tests/fixtures/api-maintenance/`.

## Hosted worker and threat boundary

The public engine defines stable tenant-scoped job identities, a bounded idempotent
queue, cancellation tokens, exponential retry decisions, sanitized observability
events, stage-specific credential scopes, and federated coordination plans. The
private platform remains responsible for scheduling, installation credentials,
billing, and durable queue infrastructure.

Each claim is filtered by authenticated tenant. An assigned workspace must be an
absolute descendant of the tenant/repository root. A federated event creates one
repair job per repository and a distinct coordination group per tenant. Fetch, repair,
and test receive no repository write credential; only publish receives a credential
scoped to that exact tenant and repository.

Treat vendor contracts, changelogs, agent output, project commands, Git state, and PR
metadata as untrusted. Source fetching uses SSRF/redirect defenses and bounded streams.
Release HTML/scripts/command-shaped instructions are removed. Logs, PR bodies, and
worker events are bounded and redact secret-shaped content. Source revision reuse with
a different digest, run identity disagreement, and duplicate-event races fail closed.

## Validation and performance

The offline corpus is network-free and enforces at least 95% scoped direct/SDK
localization precision and recall, correct applicability labels, wrapper-to-test impact,
dynamic-call non-binding, unused dependency suppression, wrong-version suppression,
and second-vendor parity. `HistoricalEvaluationReport` records observation recall,
identity precision, modeled/monitored coverage, binding and event precision/recall,
test recall, detection time, repair verification, classification/applicability, file
scope, attempt pass rates, duplicate PR rate, context size, runtime, and model cost;
its launch gate prioritizes precision and duplicate safety.

The complete 0.9.0 Windows release-candidate benchmark matrix passed on
2026-08-01. Representative confidence intervals from the first uninterrupted
workspace run were:

| Workload | Release-candidate interval |
| --- | ---: |
| Normalize and diff 1,000 operations | 17.75-18.28 ms |
| Match 1,000 renamed operations | 3.53-3.58 ms |
| Assess relevance over 10,000 bindings | 12.07-12.29 ms |
| Analyze static coverage over 10,000 observations | 37.77-38.57 ms |
| Join runtime coverage over 10,000 observations | 17.63-17.95 ms |
| Bind 200 dependencies into a 40,000-node graph | 9.92-10.19 ms |
| Recursively plan 100 / 1,000 / 5,000 directories | 2.25-2.33 / 23.87-24.74 / 123.86-128.23 ms |

These are diagnostic host baselines, not portable SLAs. Immediate repeated runs
after the full matrix showed thermal/scheduler shifts without a code change, so
confirm comparative Criterion warnings on an otherwise idle, stable host.
Reproduce the focused API measurements with:

```text
cargo bench -p synaptic-api --bench api_maintenance --locked
```

Run every maintained workspace benchmark with:

```text
cargo bench --workspace --all-features --locked
```

Run the release gates with:

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```
