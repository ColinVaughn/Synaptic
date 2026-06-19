# Configuration

CodeGraph is configured through three things: build-time **feature flags**, runtime
**environment variables**, and on-disk **config / output files**. There is no dotenv loading;
environment variables are read directly from the process environment.

## Feature flags (build time)

The integration features below are off by default; enable them at build time (see
[Installation](Installation)). The language extractors, `cross-language`, and
`cache-binary` are **on** by default.

| Feature | Enables | Used by |
|---|---|---|
| `pg` | Postgres schema introspection | `codegraph ingest pg` |
| `push` | Live Neo4j/FalkorDB export | `codegraph export neo4j\|falkordb --push` |
| `office` | Spreadsheet ingest | `codegraph ingest office` |
| `gws` | Google-Workspace ingest | `codegraph ingest gws` |
| `media` | Audio/video transcription and YouTube URL ingest | `codegraph ingest media` |
| `live-explain` | Live database `EXPLAIN` for sequential-scan detection | `codegraph sql audit --explain` |

Two non-language features are **on by default** and can be turned off with
`--no-default-features` (then re-list the `lang-*` features you want):

| Feature (default on) | Effect |
|---|---|
| `cross-language` | Post-extraction passes that infer cross-language edges (FFI, subprocess, HTTP/RPC, code->SQL). |
| `cache-binary` | Stores the per-file AST cache as MessagePack instead of JSON — faster to decode and smaller, which helps most on column-heavy SQL schemas. |

Language support is controlled by 38 `lang-*` features, all on by default. To compile a
single language (used in CI), build with `--no-default-features --features lang-<name>`. See
[Languages](Languages) and [Development](Development).

## Environment variables

### LLM backend selection (semantic pass)

The semantic pass auto-detects a backend by checking these in order: Gemini, Kimi, Anthropic,
OpenAI, DeepSeek, Azure OpenAI, Bedrock, Ollama. Set `CODEGRAPH_BACKEND` to force one. See
[Semantic Analysis](Semantic-Analysis).

| Variable | Purpose |
|---|---|
| `CODEGRAPH_BACKEND` | Force a specific backend (the only way to select `claude-cli`) |
| `CODEGRAPH_LLM_TEMPERATURE` | Temperature override; `none`/`omit`/`default` omits the parameter |
| `OPENAI_API_KEY`, `OPENAI_MODEL`, `OPENAI_BASE_URL` | OpenAI-compatible backend |
| `GEMINI_API_KEY` / `GOOGLE_API_KEY`, `GEMINI_MODEL` | Gemini |
| `MOONSHOT_API_KEY`, `MOONSHOT_MODEL` | Kimi (Moonshot) |
| `DEEPSEEK_API_KEY`, `DEEPSEEK_MODEL` | DeepSeek |
| `ANTHROPIC_API_KEY`, `ANTHROPIC_MODEL`, `ANTHROPIC_BASE_URL` | Native Anthropic |
| `AZURE_OPENAI_API_KEY`, `AZURE_OPENAI_ENDPOINT`, `AZURE_OPENAI_DEPLOYMENT`, `AZURE_OPENAI_API_VERSION` | Azure OpenAI |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_REGION` / `AWS_DEFAULT_REGION`, `BEDROCK_MODEL` | AWS Bedrock |
| `OLLAMA_BASE_URL`, `OLLAMA_API_KEY`, `OLLAMA_MODEL` | Ollama (local; presence opts in) |
| `CLAUDE_CLI_MODEL` | Model for the `claude` CLI backend |

### Server, ingestion, and database push

| Variable | Purpose |
|---|---|
| `CODEGRAPH_API_KEY` | Bearer token for the HTTP MCP server (fallback for `--api-key`) |
| `CODEGRAPH_QUERY_LOG` | Path to write the server query log |
| `CODEGRAPH_QUERY_LOG_DISABLE` | Disable the query log (`1`/`true`/`yes`) |
| `CODEGRAPH_CHANGED` | Newline-delimited changed-file list read by `update` (used by the git hook) |
| `NEO4J_PASSWORD`, `FALKORDB_PASSWORD` | Credentials for `export --push` |
| `CODEGRAPH_GWS_CMD` | Google-Workspace CLI name (default `gws`) |
| `CODEGRAPH_TRANSCRIBE_CMD` | Transcription CLI (default `whisper`) |
| `CODEGRAPH_WHISPER_MODEL` | Whisper model (default `base`) |

### Other

| Variable | Purpose |
|---|---|
| `HOME` / `USERPROFILE` | Locate the global store `~/.codegraph` (falls back to `.codegraph` in the working directory) |
| `CODEGRAPH_SKIP_HOOK` | Skip the installed git hook for one invocation (`1`) |

## Config and output files

| Path | Read/Written | Role |
|---|---|---|
| `.codegraphignore` | read | Extra ignore rules, layered per directory; takes precedence over `.gitignore` on conflicts |
| `.gitignore` | read | Honored during discovery |
| `codegraph-workspace.toml` | read/written | Workspace manifest (`[workspace]` members, `[[repos]]`); written by `workspace init`. See [Workspaces and Federation](Workspaces-and-Federation) |
| `codegraph-out/` | written | All output: `graph.json`, `GRAPH_REPORT.md`, visualizations, exports, `ingested/`, `surfaces/` |
| `codegraph-out/cache/ast/` | written | Per-file AST cache, keyed by content; auto-invalidated when extractor logic or enabled languages change. Clear with `codegraph cache clear` |
| `codegraph-out/cache/semantic/` | written | Semantic-pass response cache |
| `~/.codegraph/` | read/written | Global cross-repo store (`global-graph.json`, `global-manifest.json`) |
| `.claude/settings.json` | read/written | `PreToolUse` hooks installed by `codegraph install` (Claude). See [Assistant Integration](Assistant-Integration) |
| `CLAUDE.md` / `AGENTS.md` / `GEMINI.md` and per-platform skill files | written | Assistant instruction sections written by `install` |
| `.codex/config.toml` / `.codex/hooks.json` (+ `~/.codex/config.toml`) | read/written | Codex MCP server + `SessionStart` hook from `codegraph install codex` (project, or global with `--global`). See [Assistant Integration](Assistant-Integration) |

## Notes

- A code-only `extract` never reads any of the LLM variables and never makes a network call.
  They matter only for `extract --semantic` and `ingest`.
- The AST cache lives under `codegraph-out/`, so deleting that directory resets everything.
