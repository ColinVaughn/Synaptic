# CodeGraph

Turn any folder of code into a persistent, queryable **knowledge graph** — then query
that compact graph instead of re-reading the whole codebase. CodeGraph extracts symbols
and relationships across 30+ languages with [tree-sitter](https://tree-sitter.github.io/),
clusters them into communities, surfaces the structurally important pieces, and writes
both machine-readable graphs and human-readable reports/visualizations.

It's a single static Rust binary (`codegraph`) — no runtime, no interpreter — plus an MCP
server so an AI coding assistant can consult the graph before grepping or reading files.

> **Status:** active development, with first-class monorepo / multi-repo federation.

---

## Why

- **Token economy.** Querying a compact graph costs a fraction of feeding raw files to an
  LLM, so an assistant can answer "what calls this?" or "what would this change break?"
  without loading the repo.
- **Structural clarity.** God nodes, surprising cross-module connections, import cycles,
  and community structure are computed for you.
- **Confidence you can audit.** Every inferred relationship is tagged `EXTRACTED`,
  `INFERRED`, or `AMBIGUOUS`.
- **Scales past one repo.** A workspace can federate many repos with real cross-repo edge
  resolution (export surfaces + import/tsconfig/module-federation aliases).

## Install

CodeGraph builds with a stable Rust toolchain (pinned to 1.95 via
[rust-toolchain.toml](rust-toolchain.toml)).

```sh
# From a clone — installs the `codegraph` binary onto your PATH:
cargo install --path bin/codegraph

# …or just build it in-tree:
cargo build --release      # → target/release/codegraph
```

Prebuilt binaries for Linux/macOS/Windows are attached to each tagged
[GitHub Release](../../releases) (see the `release` workflow). Optional integrations are
behind feature flags (off by default): `pg` (Postgres introspection), `push` (live
Neo4j/FalkorDB export), and `office`/`gws`/`media` (spreadsheet / Google-Workspace /
audio-video ingest) — e.g. `cargo install --path bin/codegraph --features pg,push`.

## Quickstart

```sh
# 1. Build the graph for the current directory → codegraph-out/
codegraph extract .

# 2. Ask the graph a question (returns a relevant subgraph)
codegraph query "authentication flow"

# 3. What would changing a symbol break? (reverse-impact)
codegraph affected parse_config
```

`extract` honors `.codegraphignore` / `.gitignore` and skips sensitive files
(`.env`, keys). A code-only corpus runs fully offline; the optional LLM semantic pass over
docs/papers (`extract --semantic`) needs an API key (e.g. `OPENAI_API_KEY`).

## Output artifacts (`codegraph-out/`)

| Artifact | What it is |
|---|---|
| `graph.json` | Full graph (node-link JSON) — query it without re-reading files |
| `GRAPH_REPORT.md` | God nodes, surprising connections, suggested questions, import cycles |
| `graph.html` | Interactive 2D explorer (search + community color) |
| `graph-3d.html` | Interactive 3D force graph (search, relation toggles, federation colors) |
| `graph.svg` | Static layout (Barnes–Hut, component-packed, asset-shaped) |
| `graph.graphml` / `graph.cypher` / `graph.dot` | Import into Gephi / Neo4j / Graphviz |
| `callflow.html` / `tree.html` | Mermaid call-flow + D3 file tree |
| `obsidian/`, `wiki/` | Obsidian vault / Markdown wiki (with `--obsidian` / `--wiki`) |

## Command reference

| Command | What it does |
|---|---|
| `extract [path]` | Build the graph and write `codegraph-out/`. Flags: `--directed`, `--obsidian`, `--wiki`, `--semantic` |
| `export <format>` | Re-emit a format from an existing `graph.json` (no rebuild): `json`/`html`/`svg`/`graphml`/`cypher`/`dot`/`callflow`/`tree`/`3d`/`obsidian`/`wiki`/`report`. Flags: `--repo` (scope to a federated member); `neo4j`/`falkordb` write a cypher script, or push live with `--push <uri>` (needs the `push` build feature) |
| `query <text>` | Return a relevant subgraph. Flags: `--max-nodes`, `--repo`, `--dfs` (depth-first traversal) |
| `path <from> <to>` | Shortest path between two nodes |
| `explain <node>` | Show a node and its neighbours |
| `affected <node>` | Nodes that (transitively) depend on a node. Flags: `--depth`, `--relation` |
| `update [paths…]` | Incrementally rebuild after files change (`--full` for a full rebuild) |
| `serve` | Run the MCP server (stdio, or `--http <addr> --api-key <key>`) |
| `prs [number]` | Graph-aware PR dashboard / detail. Flags: `--triage` (ranked actionable PRs), `--conflicts` (PRs sharing a graph community), `--base`, `--repo` |
| `workspace <action>` | Multi-repo / monorepo federation (`init`/`add`/`discover`/`build`/`federate`/`sync`/`status`/`list`) |
| `global <action>` | The cross-repo global graph store (`~/.codegraph`) |
| `merge-graphs <graphs…>` | Compose several `graph.json` files into one namespaced graph |
| `ingest <source>` | Ingest an external source (cargo / mcp / scip / pg / url; `office` / `gws` / `media` behind feature flags) |
| `hook <action>` | Manage git hooks + the `graph.json` merge driver |
| `install` / `uninstall [platform]` | Install the CodeGraph skill for a host assistant (`claude` \| `agents` \| `codex` \| `opencode` \| `gemini` \| `cursor` \| `copilot` \| `kilo`) |
| `cache <action>` | Maintain the on-disk extraction cache |

Run `codegraph <command> --help` for the full flag list.

## Use it from an AI assistant (MCP)

```sh
codegraph serve                       # stdio MCP server for a local assistant
codegraph serve --http 127.0.0.1:8765 --api-key "$CODEGRAPH_API_KEY"   # team HTTP server
```

The server exposes read-only graph tools (`query_graph`, `get_node`, `get_neighbors`,
`get_community`, `god_nodes`, `graph_stats`, `shortest_path`, federation `list_repos` /
`repo_stats`) plus PR tools, and a small REST surface (`/api/stats`, `/api/query`, …) for
non-MCP clients. `codegraph install` wires a `PreToolUse` hook so the assistant consults
the graph before grepping/reading.

## Languages

30+ languages via tree-sitter, each built and tested in isolation in CI: Python,
JavaScript/TypeScript (+ JSX/TSX, Vue/Svelte/Astro), Go, Rust, Java, C#, Kotlin, Swift, C,
C++, Objective-C, Ruby, PHP, Scala, Groovy, Lua, Dart, Elixir, Julia, Zig, Bash,
PowerShell, Verilog, Fortran, Classic ASP, and regex/delegation fallbacks for Salesforce
Apex, Pascal/Delphi, and Razor/Blazor. Plus data/infra and project formats: SQL, JSON,
YAML, HCL/Terraform, .NET project files (`.csproj`/`.sln`/`.slnx`), and Markdown structure
(heading hierarchy). Framework-aware edges for PHP/Laravel and Dart/Flutter.

## Development

```sh
cargo test --workspace          # all tests
cargo fmt --all --check         # formatting (enforced in CI)
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

The codebase is a 15-crate workspace (`crates/*`) plus the `codegraph` binary (`bin/`).
CI builds each language grammar in isolation so a grammar bump that silently drops
nodes/edges fails on its own.

## License

MIT — see [LICENSE](LICENSE).
