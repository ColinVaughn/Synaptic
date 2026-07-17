# Development

This page covers building, testing, and the repository layout for contributors.

## Toolchain

- **Rust 1.96**, pinned in `rust-toolchain.toml` (with `rustfmt` and `clippy` components).
- Edition 2021. The workspace `rust-version` is `1.96`.

## Build, test, lint

These are the exact commands CI runs:

```sh
# Format check
cargo fmt --all --check

# Lint (warnings are errors)
cargo clippy --workspace --all-targets --all-features -- -D warnings

# Full test suite
cargo test --workspace --all-features
```

A release build of just the binary:

```sh
cargo build --release --locked -p synaptic
```

## Per-language testing

Every language extractor can be built and tested in isolation so a grammar bump that
silently drops nodes or edges fails on its own:

```sh
cargo test -p synaptic-extract --no-default-features --features lang-rust
```

CI runs this across a matrix of grammar-backed languages. See [Languages](Languages).

## Continuous integration

`.github/workflows/ci.yml` runs on every push and pull request with three jobs:

- **lint** - `cargo fmt --all --check` and `cargo clippy ... -D warnings`.
- **test** - `cargo test --workspace --all-features` on Linux, macOS, and Windows.
- **extract-langs** - a matrix that tests each grammar-backed language on its own
  (`--no-default-features --features lang-<name>`).

`.github/workflows/release.yml` runs on `v*` tags (and manual dispatch): it cross-compiles
the binary for Linux (`x86_64`), macOS (`x86_64` and `aarch64`), and Windows (`x86_64`),
packages each with the README/LICENSE/CHANGELOG, and publishes a GitHub Release.

## Benchmarks

Several crates ship Criterion benchmarks (for example `synaptic-extract`,
`synaptic-detect`, `synaptic-output`). Run them with:

```sh
cargo bench
```

The graph-performance release gates can also be run independently:

```sh
cargo bench -p synaptic-graph --bench graph -- graph/duplicate_edge_provenance
cargo bench -p synaptic-graph --bench graph -- graph/scaling/cluster/10000
cargo bench -p synaptic-incremental --bench incremental
cargo bench -p synaptic-workspace --bench workspace -- workspace/compose/16x500
```

The duplicate-provenance benchmark intentionally runs the former repeated-
materialization comparison only through 1,000 sites; the linear accumulator is
also measured at 10,000 sites so the full benchmark remains practical.

## Repository layout

```
crates/
  synaptic-core/         data model + graph.json DTO + validation
  synaptic-detect/       discovery, classification, ignore rules
  synaptic-extract/      tree-sitter + regex extractors (lang-* features)
  synaptic-graph/        build, dedup, clustering, analysis
  synaptic-semantic/     LLM semantic pass
  synaptic-llm/          LLM client + provider registry
  synaptic-query/        query / path / explain / affected
  synaptic-output/       graph.json + viewers + exports
  synaptic-report/       GRAPH_REPORT.md
  synaptic-ingest/       external-source ingestion
  synaptic-server/       MCP server + REST
  synaptic-prs/          PR dashboard
  synaptic-incremental/  incremental rebuild, watch, hooks
  synaptic-workspace/    multi-repo federation
  synaptic-skillgen/     assistant skill + hooks generation
bin/
  synaptic/              the CLI
```

See [Architecture](Architecture) for what each crate does and how the pipeline fits together.

## Conventions

- All shared dependencies are declared once in the root `Cargo.toml` `[workspace.dependencies]`
  and referenced with `workspace = true` from member crates.
- The graph output is deterministic; tests assert byte-stable `graph.json` where relevant, so
  avoid introducing nondeterministic iteration order.
- Edges carry an explicit confidence level; prefer `INFERRED`/`AMBIGUOUS` over `EXTRACTED`
  when a relationship is heuristic.
