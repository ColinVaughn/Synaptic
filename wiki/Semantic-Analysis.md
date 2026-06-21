# Semantic Analysis

The semantic pass enriches the graph with LLM-derived concepts and resolves ambiguous duplicates that the deterministic pipeline cannot. It is opt-in via `synaptic extract --semantic`, needs an LLM backend API key, and makes paid API calls (except for local/subscription backends).

```
synaptic extract . --semantic
```

When `--semantic` is set but no backend key is detected, or backend init fails, the pass is skipped with a note and extraction continues normally. The pass is never run by `update`, `watch`, or the git hooks; those preserve existing semantic nodes but do not generate new ones (see [Incremental-Updates]).

See also: [Extraction], [Configuration], [MCP-Server].

## What the semantic pass does

With `--semantic` and a working backend, the extract run adds three LLM-driven steps on top of the deterministic build:

1. Concept extraction. Documents and papers (not code) are sent to the model, which returns concept nodes and edges. These are merged into the graph before clustering, so concepts can collapse onto the AST symbols they describe. Documents are chunked to a token budget (60,000 tokens per chunk) and, on context overflow or truncated output, the chunk is recursively bisected (up to depth 3) and the partial results merged.
2. Dedup tiebreaker. The deterministic dedup leaves "ambiguous" concept pairs (fuzzy label similarity in the 75-92 band) unmerged. With an LLM, the model is asked, in batches, whether each pair names the same real-world concept; confirmed pairs are merged. Offline (no LLM), a conservative deterministic rule merges only word-reorderings/duplications and flags the rest for review.
3. Community labeling. Communities are named by the model from their representative member labels. Without a backend, labels stay empty and views fall back to `Community N`.

The output reports what the LLM contributed, for example:

```
Semantic pass: using the claude backend
Semantic pass: +18 concept node(s), +24 edge(s)
Dedup tiebreaker (LLM): merged 3 of 7 ambiguous concept pair(s), 4 left for review
LLM usage (extraction pass): 41200 input + 5300 output tokens (~$0.2031 estimated on claude)
```

## Supported LLM backends

Synaptic selects one backend from the environment. Backends fall into three groups:

- OpenAI-compatible Chat Completions: OpenAI, Gemini (its OpenAI-compat layer), Kimi (Moonshot), DeepSeek, Azure OpenAI, and Ollama (local).
- Native Anthropic Messages API (`POST /v1/messages`).
- AWS Bedrock (Converse API, SigV4-signed; AWS env credentials only).
- claude CLI (the locally-installed Claude Code CLI; opt-in only).

### Selection and priority

The backend is resolved from environment variables in this priority order: gemini, kimi, claude, openai, deepseek, azure, bedrock, ollama. The first one whose credentials are present wins. Set `SYNAPTIC_BACKEND` to force a specific backend by name (`gemini`, `kimi`, `claude`, `openai`, `deepseek`, `azure`, `bedrock`, `ollama`, `claude-cli`). `claude-cli` is never auto-detected; `SYNAPTIC_BACKEND=claude-cli` is the only way to select it.

### Backends and their environment variables

| Backend | API key / credential | Model override (env) | Base URL / endpoint override (env) | Default model |
| --- | --- | --- | --- | --- |
| gemini | `GEMINI_API_KEY` or `GOOGLE_API_KEY` | `GEMINI_MODEL` | (built in) | `gemini-2.5-flash` |
| kimi | `MOONSHOT_API_KEY` | `MOONSHOT_MODEL` | (built in) | `kimi-k2` |
| claude | `ANTHROPIC_API_KEY` | `ANTHROPIC_MODEL` | `ANTHROPIC_BASE_URL` | `claude-sonnet-4-6` |
| openai | `OPENAI_API_KEY` | `OPENAI_MODEL` | `OPENAI_BASE_URL` | `gpt-4.1-mini` |
| deepseek | `DEEPSEEK_API_KEY` | `DEEPSEEK_MODEL` | (built in) | `deepseek-chat` |
| azure | `AZURE_OPENAI_API_KEY` + `AZURE_OPENAI_ENDPOINT` | `AZURE_OPENAI_DEPLOYMENT` | `AZURE_OPENAI_ENDPOINT` | `gpt-4o` |
| bedrock | `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` | `BEDROCK_MODEL` | (region endpoint) | `anthropic.claude-3-5-sonnet-20241022-v2:0` |
| ollama | (keyless; `OLLAMA_API_KEY` optional) | `OLLAMA_MODEL` | `OLLAMA_BASE_URL` | `qwen2.5-coder:7b` |
| claude-cli | (uses your `claude` CLI login) | `CLAUDE_CLI_MODEL` | n/a | (CLI default) |

Notes:

- Azure requires both a key and an endpoint. It addresses the model as a deployment path with an `api-key` header; `AZURE_OPENAI_DEPLOYMENT` is the deployment name, and `AZURE_OPENAI_API_VERSION` overrides the REST API version (default `2024-12-01-preview`).
- Ollama is local and keyless; setting `OLLAMA_BASE_URL` opts it in. It runs serially.
- Bedrock uses AWS environment credentials only (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, optional `AWS_SESSION_TOKEN`, and `AWS_REGION` / `AWS_DEFAULT_REGION`, default region `us-east-1`); AWS profile and instance-role resolution are not supported.
- `SYNAPTIC_LLM_TEMPERATURE` overrides the request temperature for OpenAI-compatible backends; `none`/`omit`/`default` omits the parameter. OpenAI reasoning models (o1/o3/o4, gpt-5) always omit temperature automatically.
- claude-cli routes through the locally-installed Claude Code CLI (`claude -p --output-format json`), so Pro/Max subscribers can run the pass on their plan.

## Response cache

A semantic cache stores the extraction result on disk under `synaptic-out/cache/semantic`, keyed by the corpus content (and per-file by content + relative path). An unchanged corpus on a rebuild reuses the cached result and makes no API call. A cache hit reports zero tokens and zero cost.

## Cost awareness

Each backend has a published per-million-token price for its default model, used to estimate the run's USD spend. Local and subscription backends (Ollama, claude-cli) are zero-rated. The end-of-run line ("LLM usage (extraction pass): ... ~$X estimated on <backend>") reports only the dominant extraction-pass tokens; the small tiebreaker and community-labeling prompts are not metered.

Approximate published rates (USD per million input / output tokens, for each backend's default model):

| Backend | Input | Output |
| --- | --- | --- |
| deepseek | 0.14 | 0.28 |
| openai | 0.40 | 1.60 |
| gemini | 0.50 | 3.00 |
| kimi | 0.74 | 4.66 |
| azure | 2.50 | 10.00 |
| claude | 3.00 | 15.00 |
| bedrock | 3.00 | 15.00 |
| ollama | 0.00 | 0.00 |
| claude-cli | 0.00 | 0.00 |

These are estimates, not invoices: using a non-default model (via a `*_MODEL` override) can change the real price, notably for Azure and Bedrock. Because the pass needs a real API key and makes paid calls, it is opt-in and never runs without `--semantic`.
