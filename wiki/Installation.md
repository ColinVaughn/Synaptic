# Installation

Synaptic's engine and terminal workflow are a single static Rust binary named `synaptic`.
There is no runtime or interpreter to install alongside it. The optional `synaptic-ui`
desktop addon provides visual setup and a searchable interface to the complete CLI.

## Requirements

- A stable Rust toolchain. The repo pins **Rust 1.97.1** via `rust-toolchain.toml`, so a
  `rustup`-managed environment will select it automatically.
- Git, if you plan to use the PR dashboard, git hooks, or git-based workspace members.

## Build from source

```sh
# Install the `synaptic` binary onto your PATH:
cargo install --path bin/synaptic

# Optional native UI (uses `synaptic` from the same directory or PATH):
cargo install --path bin/synaptic-ui

# ...or build it in-tree:
cargo build --release -p synaptic -p synaptic-ui
```

## Prebuilt binaries

Tagged releases attach prebuilt binaries for Linux (`x86_64`), macOS (`x86_64` and
`aarch64`), and Windows (`x86_64`) to the [GitHub Releases](../../releases) page. Each
archive bundles `synaptic`, its `syn` alias, the optional `synaptic-ui` executable, and the
README, LICENSE, and CHANGELOG. Each operating system gets its own native build; the same UI
source and workflow are used on all three platforms.

Launch `synaptic-ui` from any directory. Open **App** and choose **Add to applications** to make it
searchable from Windows Start, macOS Applications, or the Linux application menu without an
administrator account. Then choose a workspace folder. See [Desktop UI](Desktop-UI) for the
federation and MCP setup flow.

## Updating

Once installed, update in place with:

```sh
synaptic self-update
```

This checks the latest [GitHub Release](../../releases), and if it is newer, prompts you
before downloading the prebuilt archive for your platform, verifying its checksum, and
replacing the running binary (and its `syn` alias). Updating is **opt-in** — Synaptic never
checks or replaces itself on its own. To get a once-a-day "update available" reminder on
ordinary commands, opt in with `synaptic self-update --enable` (off by default, throttled,
and printed only to stderr).

A `cargo install` / source build can self-update too, but the swap installs the
default-feature prebuilt binary; rebuild from source if you depend on extra features. See
[Updating](Updating) for the full walkthrough and [`self-update`](Commands#self-update) for
the flag reference.

## Optional features

Several integrations are gated behind Cargo features and are **off by default**, so the
default build stays small and dependency-light. Enable the ones you need at build time:

| Feature | Enables |
|---|---|
| `pg` | `synaptic ingest pg` (live Postgres schema introspection) |
| `push` | `synaptic export neo4j\|falkordb --push <uri>` (live database export) |
| `office` | `synaptic ingest office` (spreadsheet ingest) |
| `gws` | `synaptic ingest gws` (Google-Workspace ingest) |
| `media` | `synaptic ingest media` (audio/video transcription, also YouTube URL ingest) |

```sh
# Example: build with Postgres ingest and live database push:
cargo install --path bin/synaptic --features pg,push
```

If you run a feature-gated subcommand on a build that lacks the feature, Synaptic prints a
clear error telling you which feature to rebuild with. See [Ingestion](Ingestion) and
[Output Formats](Output-Formats) for what each feature unlocks.

## Languages

All language extractors are compiled into the default build (39 `lang-*` features on by
default). You do not need to enable anything per language to extract a mixed-language repo.
See [Languages](Languages) for the full list and [Development](Development) for building a
single language in isolation.

## Verify

```sh
synaptic --help
synaptic extract .
```

The first `extract` writes a `synaptic-out/` directory next to your code. See
[Quickstart](Quickstart) next.
