# Architecture

CodeGraph is a Rust workspace: **15 library crates** under `crates/*` plus the `codegraph`
CLI under `bin/`. The workspace uses edition 2021 and a pinned Rust 1.95 toolchain, with
dependencies centralized in the root `Cargo.toml`.

## The pipeline

A normal `extract` run flows through these stages:

```
detect  ->  extract  ->  graph build  ->  cluster + analyze  ->  output
                                   \-> (optional) semantic pass
```

1. **detect** - walk the directory, classify files, apply ignore rules, and skip sensitive
   files. See [Extraction](Extraction).
2. **extract** - run tree-sitter (or a regex extractor) per file to produce nodes and edges.
   Results are cached per file so unchanged files skip re-parsing. See [Languages](Languages).
3. **graph build** - assemble nodes/edges into a graph, resolve symbols across files, and
   deduplicate.
4. **cluster + analyze** - detect communities, compute betweenness and god nodes, find
   import cycles and surprising connections. See [Analysis and Reports](Analysis-and-Reports).
5. **output** - write `graph.json` and the report, visualizations, and exports. See
   [Output Formats](Output-Formats).

The optional **semantic pass** (`--semantic`) sends documents and papers to an LLM to add
concept nodes and to break ties during dedup. See [Semantic Analysis](Semantic-Analysis).

Querying and serving read `graph.json` back; they do not re-extract. See [Querying](Querying)
and [MCP Server](MCP-Server).

## Crates

| Crate | Responsibility |
|---|---|
| `codegraph-core` | The shared data contract: `NodeId`, `FileType`, `Confidence`, `Node`, `Edge`, `Hyperedge`, the `graph.json` node-link DTO, id generation, and schema validation |
| `codegraph-detect` | File discovery, classification, ignore handling (`.codegraphignore` / `.gitignore`), manifest building, and sensitive-file detection |
| `codegraph-extract` | Tree-sitter (and regex) extractors that turn source files into core nodes/edges; languages gated behind `lang-*` features; per-file AST cache |
| `codegraph-graph` | Graph assembly: build, symbol resolution, dedup (MinHash/LSH), clustering and community detection, betweenness, analysis |
| `codegraph-semantic` | The LLM semantic pass: documents and papers to concept nodes, plus the optional dedup tiebreaker |
| `codegraph-llm` | Pluggable LLM client layer: provider registry with env auto-detect, response cache, JSON repair, token-budget chunking, adaptive retry |
| `codegraph-query` | Query (IDF-scored subgraph retrieval), shortest path, node explanation, and reverse impact |
| `codegraph-output` | Output writers: `graph.json`, HTML viewers, SVG, GraphML, Cypher, DOT, Mermaid call-flow, D3 tree, Obsidian, wiki, and live database push |
| `codegraph-report` | The `GRAPH_REPORT.md` generator |
| `codegraph-ingest` | External-source ingestion: URL (SSRF-guarded), MCP config, Cargo, Postgres, SCIP, office, media |
| `codegraph-server` | The MCP server (read-only graph and PR tools) over stdio and HTTP, plus a small REST surface |
| `codegraph-prs` | Graph-aware PR dashboard: classification, CI rollup, blast radius, conflict grouping |
| `codegraph-incremental` | Changed-files rebuild engine plus git integration (hooks, merge driver, watch, concurrency lock) |
| `codegraph-workspace` | Multi-repo / monorepo federation: member discovery, namespacing, cross-repo resolution, global store, `merge-graphs` |
| `codegraph-skillgen` | Generates and installs the host-assistant integration: the Claude skill file + `.claude/settings.json` hooks, the always-on instruction blocks (`AGENTS.md`/`GEMINI.md`/etc.), and the Codex MCP server + `SessionStart` hook config (project `.codex/` or global `~/.codex/`) |
| `bin/codegraph` | The CLI that wires the crates into commands |

## Data model

The graph is a node-link structure serialized to `graph.json` (NetworkX-compatible shape).

- **Nodes** have an `id`, a `label`, a `file_type` (one of `code`, `document`, `paper`,
  `image`, `rationale`, `concept`), and a `source_file`. Optional fields include
  `source_location`, `community`, and `repo` (for federated graphs).
- **Edges** (serialized under `links`) have a `source`, `target`, `relation` (for example
  `calls`, `imports`, `imports_from`, `inherits`, `implements`, `references`, `contains`,
  `depends_on`, `reads_from`), and a `confidence` of `EXTRACTED`, `INFERRED`, or
  `AMBIGUOUS`. Cross-repo edges are flagged with `cross_repo`.

The exact JSON shape and every export format are documented in [Output Formats](Output-Formats).

## Determinism

Extraction parallelizes across files but collects results in a stable order, so `graph.json`
is byte-identical for the same input regardless of thread scheduling. Community numbers are
assigned deterministically (community 0 is the largest) and kept stable across incremental
rebuilds.

## Where to go next

- Configuration knobs and files: [Configuration](Configuration)
- Building, testing, and CI: [Development](Development)
