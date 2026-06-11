# Changelog

All notable changes to CodeGraph are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
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
- User-facing `README.md`, `LICENSE` (MIT), and this changelog.
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

### Fixed
- **Bash `source` resolution:** `source ./lib.sh` now resolves relative to the sourcing
  file's directory (to the target's real file node), so two same-named scripts in different
  directories no longer collapse to one node.
- **detect/extract drift:** 29 file extensions were classified as `Code` but had no
  extractor, inflating corpus stats and silently producing zero nodes. `.mm` now routes to
  the Objective-C extractor; the remaining unextractable extensions are no longer
  classified as code. A new invariant test (`every_detected_code_extension_has_an_extractor`)
  keeps the detect and extract sets from drifting. (Phase 6 re-added `.csproj/.sln/.slnx/.fsproj/
  .vbproj`, `.cls/.trigger`, `.pas/.pp/.dpr/.dpk/.lpr`, and `.razor/.cshtml` as their
  extractors landed.)
- **Reasoning-model temperature:** requests to OpenAI o1/o3/o4 and gpt-5 models no longer
  send an explicit `temperature` (which those models reject with HTTP 400).
- Azure backend was previously routed through the generic chat-completions path with bearer
  auth and could not reach a real Azure deployment.

[Unreleased]: https://keepachangelog.com/
