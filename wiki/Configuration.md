# Configuration

Synaptic is configured through three things: build-time **feature flags**, runtime
**environment variables**, and on-disk **config / output files**. There is no dotenv loading;
environment variables are read directly from the process environment.

## Feature flags (build time)

The integration features below are off by default; enable them at build time (see
[Installation](Installation)). The language extractors, `cross-language`, and
`cache-binary` are **on** by default.

| Feature | Enables | Used by |
|---|---|---|
| `pg` | Postgres schema introspection | `synaptic ingest pg` |
| `push` | Live Neo4j/FalkorDB export | `synaptic export neo4j\|falkordb --push` |
| `office` | Spreadsheet ingest | `synaptic ingest office` |
| `gws` | Google-Workspace ingest | `synaptic ingest gws` |
| `media` | Audio/video transcription and YouTube URL ingest | `synaptic ingest media` |
| `live-explain` | Live database `EXPLAIN` for sequential-scan detection | `synaptic sql audit --explain` |

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
OpenAI, DeepSeek, Azure OpenAI, Bedrock, Ollama. Set `SYNAPTIC_BACKEND` to force one. See
[Semantic Analysis](Semantic-Analysis).

| Variable | Purpose |
|---|---|
| `SYNAPTIC_BACKEND` | Force a specific backend (the only way to select `claude-cli`) |
| `SYNAPTIC_LLM_TEMPERATURE` | Temperature override; `none`/`omit`/`default` omits the parameter |
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
| `SYNAPTIC_API_KEY` | Bearer token for the HTTP MCP server (fallback for `--api-key`) |
| `SYNAPTIC_QUERY_LOG` | Path to write the server query log |
| `SYNAPTIC_QUERY_LOG_DISABLE` | Disable the query log (`1`/`true`/`yes`) |
| `SYNAPTIC_CHANGED` | Newline-delimited changed-file list read by `update` (used by the git hook) |
| `NEO4J_PASSWORD`, `FALKORDB_PASSWORD` | Credentials for `export --push` |
| `SYNAPTIC_GWS_CMD` | Google-Workspace CLI name (default `gws`) |
| `SYNAPTIC_TRANSCRIBE_CMD` | Transcription CLI (default `whisper`) |
| `SYNAPTIC_WHISPER_MODEL` | Whisper model (default `base`) |

### Other

| Variable | Purpose |
|---|---|
| `HOME` / `USERPROFILE` | Locate the global store `~/.synaptic` (falls back to `.synaptic` in the working directory) |
| `SYNAPTIC_SKIP_HOOK` | Skip the installed git hook for one invocation (`1`) |
| `SYNAPTIC_UPDATE_CHECK` | Set to `0` to force the opt-in background update notice off, regardless of config. See [Updating](Updating) |
| `GITHUB_TOKEN` | Optional. Raises the GitHub API rate limit for the `self-update` release lookup |

## Config and output files

| Path | Read/Written | Role |
|---|---|---|
| `.synapticignore` | read | Extra ignore rules, layered per directory; takes precedence over `.gitignore` on conflicts |
| `.gitignore` | read | Honored during discovery |
| `synaptic-workspace.toml` | read/written | Workspace manifest (`[workspace]` members, `[[repos]]`); written by `workspace init`. See [Workspaces and Federation](Workspaces-and-Federation) |
| `synaptic-out/` | written | All output: `graph.json`, `GRAPH_REPORT.md`, visualizations, exports, `ingested/`, `surfaces/` |
| `synaptic-out/cache/ast/` | written | Per-file AST cache, keyed by content; auto-invalidated when extractor logic or enabled languages change. Clear with `synaptic cache clear` |
| `synaptic-out/cache/semantic/` | written | Semantic-pass response cache |
| `~/.synaptic/` | read/written | Global cross-repo store (`global-graph.json`, `global-manifest.json`) |
| `~/.synaptic/update.toml` | read/written | Opt-in self-update state: `enabled` (background notice) and `last_check` (24h throttle). Written by `synaptic self-update --enable`/`--disable`. See [Updating](Updating) |
| `.claude/settings.json` | read/written | `PreToolUse` hooks installed by `synaptic install` (Claude). See [Assistant Integration](Assistant-Integration) |
| `CLAUDE.md` / `AGENTS.md` / `GEMINI.md` and per-platform skill files | written | Assistant instruction sections written by `install` |
| `.codex/config.toml` / `.codex/hooks.json` (+ `~/.codex/config.toml`) | read/written | Codex MCP server + `SessionStart` hook from `synaptic install codex` (project, or global with `--global`). See [Assistant Integration](Assistant-Integration) |

## Notes

- A code-only `extract` never reads any of the LLM variables and never makes a network call.
  They matter only for `extract --semantic` and `ingest`.
- The AST cache lives under `synaptic-out/`, so deleting that directory resets everything.
