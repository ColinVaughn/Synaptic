# Synaptic

<p align="center">
  <a href="https://discord.gg/ytX7R2PbNz"><img src="https://img.shields.io/badge/Discord-Join%20the%20community-5865F2?logo=discord&logoColor=white&style=for-the-badge" alt="Join our Discord"></a>
  <a href="https://github.com/ColinVaughn/Synaptic/releases"><img src="https://img.shields.io/github/v/release/ColinVaughn/Synaptic?style=for-the-badge" alt="Latest release"></a>
</p>

<p align="center">
  <a href="https://discord.gg/ytX7R2PbNz"><img src="https://invidget.switchblade.xyz/ytX7R2PbNz" alt="Synaptic Discord invite"></a>
</p>

Synaptic turns any folder of code into a persistent, queryable knowledge graph, then lets
you (or an AI assistant) query that compact graph instead of re-reading the whole codebase.
It extracts symbols and relationships across 30+ languages with tree-sitter, clusters them
into communities, surfaces the structurally important pieces, and writes machine-readable
graphs, human-readable reports, and interactive visualizations.

It is a single static Rust binary (`synaptic`) with no runtime, plus an MCP server so a
coding assistant can consult the graph before grepping or reading files.

## 60-second tour

```sh
synaptic extract .                      # build synaptic-out/ for the current directory
synaptic query "authentication flow"    # get a relevant subgraph back
synaptic explain parse_config           # show a node and its neighbours
synaptic affected parse_config          # what would changing it break?
synaptic search "MATCH (c:class) WHERE c.loc > 500 RETURN c"   # structural query (SYNQL)
synaptic diff HEAD~10 HEAD              # how the graph changed between two revisions
synaptic refactor rename old_name --to new_name   # a safe, confidence-scored rename plan
synaptic serve                          # expose the graph to an assistant over MCP
```

See [Quickstart](Quickstart) for a fuller walk-through and [Installation](Installation) to
get the binary.

## What Synaptic gives you

- A canonical `graph.json` you can query without reading source.
- A Markdown report of god nodes, surprising connections, import cycles, and suggested
  questions ([Analysis and Reports](Analysis-and-Reports)).
- 2D, 3D, and SVG visualizations plus GraphML / Cypher / DOT / Obsidian / wiki exports
  ([Output Formats](Output-Formats), [Visualizations](Visualizations)).
- Graph queries: relevant-subgraph search, shortest path, and reverse impact
  ([Querying](Querying)).
- Structural search with SYNQL, a Cypher-inspired query language matching on
  kind/visibility/loc/fan-in-out plus named architectural patterns ([Commands](Commands)).
- Time-travel diff between two git revisions: added/removed dependencies, removed
  APIs, architectural drift, new cycles, and change hotspots ([Commands](Commands)).
- Safe refactor: confidence-scored `rename`/`move`/`extract` plans for an agent to
  apply, with a graph-invariant `verify` afterwards ([Commands](Commands)).
- Change forecasting: `synaptic predict` reports a change's blast radius, public
  APIs at risk, the tests that exercise it, new cycles, and a verify checklist
  before you edit; `--edit "<kind>:<symbol>"` forecasts a described edit before any
  code is written ([Commands](Commands)).
- Speculative execution: `synaptic speculate` applies a pending change in a
  throwaway git worktree and actually runs the at-risk tests plus a build/type-
  check, reporting real pass/fail (the ground-truth half of prediction; CLI-only
  by default, exposed over MCP only with `serve --allow-exec`) ([Commands](Commands)).
- Forecast evaluation: `synaptic eval replay` replays history, re-predicts each
  commit from its parent state, and scores the prediction against git ground
  truth (co-edited tests, removed APIs) so forecast quality is regression-testable
  and CI-gateable ([Commands](Commands)).
- An MCP server (26 read-only tools) and one-command assistant integration
  ([MCP Server](MCP-Server), [Assistant Integration](Assistant-Integration)).
- Multi-repo federation with real cross-repo edge resolution
  ([Workspaces and Federation](Workspaces-and-Federation)).

## Documentation map

**Getting started:** [Installation](Installation) - [Quickstart](Quickstart) - [Updating](Updating)

**Concepts:** [Architecture](Architecture) - [Languages](Languages) -
[Synaptic vs Other Tools](Synaptic-vs-Other-Tools)

**Using Synaptic:** [Commands](Commands) - [Extraction](Extraction) - [Querying](Querying)
- [SQL Auditing](SQL-Auditing) - [Analysis and Reports](Analysis-and-Reports) -
[Output Formats](Output-Formats) - [Visualizations](Visualizations)

**Integrations:** [MCP Server](MCP-Server) - [Assistant Integration](Assistant-Integration)
- [Ingestion](Ingestion) - [Semantic Analysis](Semantic-Analysis)

**Scaling:** [Workspaces and Federation](Workspaces-and-Federation) -
[Incremental Updates](Incremental-Updates) - [PR Dashboard](PR-Dashboard)

**Reference:** [Configuration](Configuration) - [Development](Development)

## Design principles

- **Offline by default.** A code-only corpus makes no network calls. The only network
  features are opt-in and explicit: the `--semantic` pass over documents calls an LLM
  ([Semantic Analysis](Semantic-Analysis)), and `self-update` (with its optional daily
  notice) contacts GitHub ([Updating](Updating)).
- **Auditable.** Every edge carries a confidence level (`EXTRACTED`, `INFERRED`,
  `AMBIGUOUS`).
- **Deterministic.** The same input produces the same graph; ids and community numbers are
  stable across rebuilds.

## Community

Questions, ideas, or want to show what you built? Join us on
[Discord](https://discord.gg/ytX7R2PbNz).

## Star History

<a href="https://star-history.com/#ColinVaughn/Synaptic&Date">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=ColinVaughn/Synaptic&type=Date&theme=dark" />
    <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=ColinVaughn/Synaptic&type=Date" />
    <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=ColinVaughn/Synaptic&type=Date" />
  </picture>
</a>
