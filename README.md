# CodeGraph

Turn any folder of code into a persistent, queryable **knowledge graph**, then query that
compact graph instead of re-reading the whole codebase. CodeGraph extracts symbols and
relationships across 30+ languages with [tree-sitter](https://tree-sitter.github.io/),
clusters them into communities, surfaces the structurally important pieces, and writes
both machine-readable graphs and human-readable reports and visualizations.

It is a single static Rust binary (`codegraph`) with no runtime and no interpreter, plus an
MCP server so an AI coding assistant can consult the graph before grepping or reading files.

---

## Why

- **Token economy.** Querying a compact graph costs a fraction of feeding raw files to an
  LLM, so an assistant can answer "what calls this?" or "what would this change break?"
  without loading the repo.
- **Structural clarity.** God nodes, surprising cross-module connections, import cycles, and
  community structure are computed for you.
- **Confidence you can audit.** Every inferred relationship is tagged `EXTRACTED`,
  `INFERRED`, or `AMBIGUOUS`.
- **Scales past one repo.** A workspace can federate many repos with real cross-repo edge
  resolution (export surfaces plus import / tsconfig / module-federation aliases).
- **Offline by default.** A code-only corpus never makes a network call. The optional
  semantic pass over docs and papers is the only feature that needs an API key.

## Highlights

- **30+ languages** via tree-sitter, each built and tested in isolation in CI, plus
  regex-based extractors for a few formats and script extraction for Vue/Svelte/Astro and
  Razor/Blazor. See [Languages](https://github.com/ColinVaughn/CodeGraph/wiki/Languages).
- **One command to a full graph** plus 2D, 3D, and SVG visualizations, a Markdown report,
  and GraphML / Cypher / DOT / Obsidian / wiki exports. See [Output Formats](https://github.com/ColinVaughn/CodeGraph/wiki/Output-Formats).
- **Graph queries**: relevant-subgraph search, shortest path, node explanation, and
  reverse-impact ("what depends on this"). See [Querying](https://github.com/ColinVaughn/CodeGraph/wiki/Querying).
- **MCP server** (protocol 2025-06-18) exposing 17 read-only tools over stdio or HTTP:
  subgraph search, source reading, reverse-impact, and PR/working-tree blast radius, plus
  prompts, completions, resource subscriptions, and structured tool output. See
  [MCP Server](https://github.com/ColinVaughn/CodeGraph/wiki/MCP-Server).
- **Incremental rebuilds**, file watching, and git hooks keep the graph current. See
  [Incremental Updates](https://github.com/ColinVaughn/CodeGraph/wiki/Incremental-Updates).
- **Graph-aware PR dashboard** with blast radius and merge-order conflict detection. See
  [PR Dashboard](https://github.com/ColinVaughn/CodeGraph/wiki/PR-Dashboard).

## Install

CodeGraph builds with a stable Rust toolchain (pinned to 1.95 via
[rust-toolchain.toml](rust-toolchain.toml)).

```sh
# From a clone, installs the `codegraph` binary onto your PATH:
cargo install --path bin/codegraph

# ...or build it in-tree:
cargo build --release      # -> target/release/codegraph
```

Prebuilt binaries for Linux/macOS/Windows are attached to each tagged
[GitHub Release](../../releases) (see the `release` workflow). Optional integrations are
behind feature flags (off by default): `pg` (Postgres introspection), `push` (live
Neo4j/FalkorDB export), and `office` / `gws` / `media` (spreadsheet / Google-Workspace /
audio-video ingest), e.g. `cargo install --path bin/codegraph --features pg,push`. See
[Installation](https://github.com/ColinVaughn/CodeGraph/wiki/Installation) and [Configuration](https://github.com/ColinVaughn/CodeGraph/wiki/Configuration).

## Quickstart

```sh
# 1. Build the graph for the current directory -> codegraph-out/
codegraph extract .

# 2. Ask the graph a question (returns a relevant subgraph)
codegraph query "authentication flow"

# 3. What would changing a symbol break? (reverse impact)
codegraph affected parse_config

# 4. Serve the graph to an AI assistant over MCP
codegraph serve
```

`extract` honors `.codegraphignore` / `.gitignore` and skips sensitive files (`.env`, keys).
A code-only corpus runs fully offline; the optional LLM semantic pass over docs and papers
(`extract --semantic`) needs an API key (e.g. `OPENAI_API_KEY`). See
[Quickstart](https://github.com/ColinVaughn/CodeGraph/wiki/Quickstart).

## Output artifacts (`codegraph-out/`)

| Artifact | What it is |
|---|---|
| `graph.json` | Full graph (node-link JSON), query it without re-reading files |
| `GRAPH_REPORT.md` | God nodes, surprising connections, suggested questions, import cycles |
| `graph.html` | Interactive 2D explorer (search + community color) |
| `graph-3d.html` | Interactive 3D force graph (search, relation toggles, federation colors) |
| `graph.svg` | Static layout (Barnes-Hut, component-packed, asset-shaped) |
| `graph.graphml` / `graph.cypher` / `graph.dot` | Import into Gephi / Neo4j / Graphviz |
| `callflow.html` / `tree.html` | Mermaid call-flow + D3 file tree |
| `obsidian/`, `wiki/` | Obsidian vault / Markdown wiki (with `--obsidian` / `--wiki`) |

## Commands

| Command | What it does |
|---|---|
| `extract [path]` | Build the graph and write `codegraph-out/`. Flags: `--directed`, `--obsidian`, `--wiki`, `--semantic` |
| `export <format>` | Re-emit a format from an existing `graph.json` (no rebuild) or push live to Neo4j/FalkorDB |
| `query <text>` | Return a relevant subgraph. Flags: `--max-nodes`, `--repo`, `--dfs` |
| `path <from> <to>` | Shortest path between two nodes |
| `explain <node>` | Show a node and its neighbours |
| `affected <node>` | Nodes that (transitively) depend on a node. Flags: `--depth`, `--relation` |
| `update [paths...]` | Incrementally rebuild after files change (`--full` for a full rebuild) |
| `watch` | Rebuild automatically as files change |
| `serve` | Run the MCP server (stdio, or `--http <addr> --api-key <key>`) |
| `prs [number]` | Graph-aware PR dashboard / detail. Flags: `--triage`, `--conflicts`, `--base`, `--repo` |
| `workspace <action>` | Multi-repo / monorepo federation (`init`/`add`/`discover`/`build`/`federate`/`sync`/`status`/`list`) |
| `global <action>` | The cross-repo global graph store (`~/.codegraph`) |
| `merge-graphs <graphs...>` | Compose several `graph.json` files into one namespaced graph |
| `ingest <source>` | Ingest an external source (cargo / mcp / scip / pg / url; `office` / `gws` / `media` behind feature flags) |
| `hook <action>` | Manage git hooks + the `graph.json` merge driver |
| `install` / `uninstall [platform]` | Install the CodeGraph skill for a host assistant |
| `cache <action>` | Maintain the on-disk extraction cache |

The full reference with every flag is in [Commands](https://github.com/ColinVaughn/CodeGraph/wiki/Commands). Run
`codegraph <command> --help` for the flag list at the terminal.

## Use it from an AI assistant (MCP)

```sh
codegraph serve                                                        # stdio MCP server
codegraph serve --http 127.0.0.1:8765 --api-key "$CODEGRAPH_API_KEY"   # HTTP server
```

The server exposes 17 read-only tools: graph navigation (`query_graph`, `get_node`,
`get_source`, `get_neighbors`, `get_community`, `god_nodes`, `graph_stats`, `shortest_path`),
impact analysis (`affected`, `find_callers`, `find_callees`), federation (`list_repos`,
`repo_stats`), and change/PR review (`working_changes_impact`, `list_prs`, `get_pr_impact`,
`triage_prs`). It also serves MCP prompts, argument completions, resource templates and
subscriptions, and a small REST surface (`/api/stats`, `/api/query`, ...) for non-MCP
clients. `codegraph install` wires the graph into a host assistant (a `PreToolUse` hook for
Claude; a native MCP server for Codex, with `codegraph install codex --global` for the Codex
desktop app). See [MCP Server](https://github.com/ColinVaughn/CodeGraph/wiki/MCP-Server) and
[Assistant Integration](https://github.com/ColinVaughn/CodeGraph/wiki/Assistant-Integration).

## Languages

30+ languages via tree-sitter, each built and tested in isolation in CI: Python,
JavaScript/TypeScript (+ JSX/TSX, Vue/Svelte/Astro), Go, Rust, Java, C#, Kotlin, Swift, C,
C++, Objective-C, Ruby, PHP, Scala, Groovy, Lua, Dart, Elixir, Julia, Zig, Bash, PowerShell,
Verilog, Fortran, and regex/delegation extractors for Classic ASP, Salesforce Apex,
Pascal/Delphi, and Razor/Blazor. Plus data and project formats: SQL, JSON, YAML,
HCL/Terraform, .NET project files (`.csproj`/`.sln`/`.slnx`), and Markdown structure.
Framework-aware edges for PHP/Laravel and Dart/Flutter. Full breakdown in
[Languages](https://github.com/ColinVaughn/CodeGraph/wiki/Languages).

## Documentation

The full documentation lives in the [project wiki](https://github.com/ColinVaughn/CodeGraph/wiki):

- **Getting started:** [Home](https://github.com/ColinVaughn/CodeGraph/wiki/Home) - [Installation](https://github.com/ColinVaughn/CodeGraph/wiki/Installation) - [Quickstart](https://github.com/ColinVaughn/CodeGraph/wiki/Quickstart)
- **Concepts:** [Architecture](https://github.com/ColinVaughn/CodeGraph/wiki/Architecture) - [Languages](https://github.com/ColinVaughn/CodeGraph/wiki/Languages)
- **Using it:** [Commands](https://github.com/ColinVaughn/CodeGraph/wiki/Commands) - [Extraction](https://github.com/ColinVaughn/CodeGraph/wiki/Extraction) - [Querying](https://github.com/ColinVaughn/CodeGraph/wiki/Querying) - [Analysis and Reports](https://github.com/ColinVaughn/CodeGraph/wiki/Analysis-and-Reports) - [Output Formats](https://github.com/ColinVaughn/CodeGraph/wiki/Output-Formats) - [Visualizations](https://github.com/ColinVaughn/CodeGraph/wiki/Visualizations)
- **Integrations:** [MCP Server](https://github.com/ColinVaughn/CodeGraph/wiki/MCP-Server) - [Assistant Integration](https://github.com/ColinVaughn/CodeGraph/wiki/Assistant-Integration) - [Ingestion](https://github.com/ColinVaughn/CodeGraph/wiki/Ingestion) - [Semantic Analysis](https://github.com/ColinVaughn/CodeGraph/wiki/Semantic-Analysis)
- **Scaling:** [Workspaces and Federation](https://github.com/ColinVaughn/CodeGraph/wiki/Workspaces-and-Federation) - [Incremental Updates](https://github.com/ColinVaughn/CodeGraph/wiki/Incremental-Updates) - [PR Dashboard](https://github.com/ColinVaughn/CodeGraph/wiki/PR-Dashboard)
- **Reference:** [Configuration](https://github.com/ColinVaughn/CodeGraph/wiki/Configuration) - [Development](https://github.com/ColinVaughn/CodeGraph/wiki/Development)

## Development

```sh
cargo test --workspace --all-features              # all tests
cargo fmt --all --check                            # formatting (enforced in CI)
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

The codebase is 15 library crates (`crates/*`) plus the `codegraph` binary (`bin/`). CI
builds each language grammar in isolation so a grammar bump that silently drops nodes/edges
fails on its own. See [Development](https://github.com/ColinVaughn/CodeGraph/wiki/Development) and [Architecture](https://github.com/ColinVaughn/CodeGraph/wiki/Architecture).

## License

GNU Affero General Public License v3.0 or later (`AGPL-3.0-or-later`), see
[LICENSE](LICENSE). If you run a modified version of CodeGraph as a network service (for
example the HTTP MCP server), the AGPL requires you to offer your modified source to its
users.
