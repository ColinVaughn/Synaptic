# Ingestion

`synaptic ingest` brings external sources into the graph. There are two shapes:

- Shape B: the source is introspected directly into graph nodes and edges, which are merged into `synaptic-out/graph.json` and the graph is rebuilt and re-written. (cargo, mcp, scip, pg)
- Shape A: the source is fetched/converted into a Markdown file under `synaptic-out/ingested/`, which the next `synaptic extract` or `synaptic update` indexes through the normal extraction pass. (url, office, gws, media)

Some sources are behind cargo build features that are off by default. Build with the feature to enable them; otherwise the subcommand stays visible in `--help` but errors with a rebuild hint. See [Configuration].

See also: [Commands], [Extraction], [Incremental-Updates].

## Usage

```
synaptic ingest <SOURCE> [ARGS]
```

| Source | Shape | Feature | Ingests |
| --- | --- | --- | --- |
| `cargo <PATH>` | B | (built in) | Cargo workspace crates + internal deps |
| `mcp <FILE>` | B | (built in) | MCP server config |
| `scip <FILE>` | B | (built in) | SCIP-style symbol index |
| `pg [DSN]` | B | `pg` | Live PostgreSQL schema |
| `url <URL>` | A | (built in) | A web page / paper / tweet / repo / PDF / image |
| `office <FILE>` | A | `office` | Spreadsheet (.xlsx/.xls/.ods) |
| `gws <FILE>` | A | `gws` | Google-Workspace doc pointer |
| `media <FILE>` | A | `media` | Local audio/video transcript |

## cargo

Introspect a Cargo workspace. Emits one `crate:<name>` node per workspace member and a `crate_depends_on` edge for each workspace-internal dependency (external registry dependencies are dropped, since they are not graph nodes).

```
synaptic ingest cargo .
```

## mcp

Read an MCP config file (`.mcp.json`, `claude_desktop_config.json`, `mcp.json`, `mcp_servers.json`) and turn its `mcpServers` map into nodes: a config-file node containing per-server nodes, each referencing globally-scoped command / package / env-var nodes.

```
synaptic ingest mcp .mcp.json
```

Security: environment-variable values are never read, only their names become `env_var` nodes; positional `args` are never persisted (a recognized npm/pypi package id is extracted from them, but paths and secrets are not). Labels are sanitized and the file is capped at 1 MiB.

## scip

Ingest a simplified SCIP-style JSON index (not the official protobuf). Builds a symbol-to-node index across all documents, emits one node per symbol, and emits relationship edges (`scip_impl`, `scip_typed`, `scip_def`, `scip_ref`). A relationship target that is external or ambiguous gets a stub `external` node so the edge never dangles.

```
synaptic ingest scip scip-index.json
```

The SCIP index is treated as untrusted: every label and metadata value is sanitized.

## pg (feature: `pg`)

Introspect a live PostgreSQL database's `information_schema`. Emits a schema-root node that `contains` one node per base table, view, and function/procedure, plus a `references` edge per foreign key (carrying the constraint and column lists in metadata). Queries run read-only.

```
synaptic ingest pg "postgresql://user@host/db"
synaptic ingest pg        # empty DSN: use the PG* environment variables
```

The shared `source_file` for these nodes is a credential-free virtual DSN (`postgresql://{host}/{dbname}`); connection-error messages are reduced to one line so a DSN or credentials cannot leak. Requires a build with `--features pg`.

## url

Fetch a URL into `synaptic-out/ingested/` for the next extract to index.

```
synaptic ingest url https://arxiv.org/abs/2401.00001
synaptic ingest url https://github.com/owner/repo
```

The URL is classified by host/extension:

- Tweet (twitter.com / x.com), arXiv, and GitHub use structured endpoints (oEmbed / Atom API / REST API) for cleaner content, falling back to generic HTML scraping on any failure.
- A generic web page is scraped to readable text (scripts/styles dropped, tags stripped, entities decoded, capped at 12,000 characters).
- A PDF or image is downloaded verbatim.
- The page/paper/tweet/repo are written as Markdown with YAML frontmatter (title, source URL, type).
- YouTube URLs are handled by the `media` feature (they shell out to `yt-dlp`); without it, the URL ingest reports that the `media` feature is required.

### SSRF / security screening

URL fetching is SSRF-guarded:

- Only `http`/`https` schemes are allowed.
- Cloud-metadata hosts (`metadata.google.internal`, `metadata.google.com`) are blocked.
- Every resolved IP must be public: private, loopback, link-local, broadcast, documentation, unspecified, RFC 6598 carrier-grade NAT, IETF/benchmarking ranges, and reserved ranges are rejected. IPv4-mapped IPv6 and NAT64-embedded addresses are unwrapped and judged on the embedded IPv4 (so a mapped internal target cannot bypass the check).
- Each redirect hop is re-validated, and responses are streamed into a bounded buffer (50 MB for binary downloads, 10 MB for text/HTML) so a dishonest `Content-Length` or a chunked response cannot exhaust memory.
- Frontmatter values are escaped as quoted YAML scalars (including Unicode line breaks and control characters) so a hostile title or URL cannot inject sibling keys.

## office (feature: `office`)

Convert a spreadsheet to Markdown (a `## Sheet` heading per worksheet, ` | `-joined non-empty rows) and write it into `synaptic-out/ingested/` for the next extract. Uses the pure-Rust `calamine` reader, no external tools.

```
synaptic ingest office data.xlsx
```

Requires a build with `--features office`.

## gws (feature: `gws`)

Export a Google-Workspace document to Markdown. `.gdoc`/`.gsheet`/`.gslides` files are tiny JSON pointers (an id and URL) created by Google Drive desktop sync, not the content. Synaptic parses the pointer and shells out to the externally-installed `gws` CLI (overridable via `SYNAPTIC_GWS_CMD`) to export the real document into `synaptic-out/ingested/`.

```
synaptic ingest gws plan.gdoc
```

Requires a build with `--features gws`, plus the `gws` CLI installed and authenticated.

## media (feature: `media`)

Transcribe a local audio/video file to a Markdown transcript in `synaptic-out/ingested/`. Shells out to a transcription CLI (`whisper` by default, overridable via `SYNAPTIC_TRANSCRIBE_CMD`; model via `SYNAPTIC_WHISPER_MODEL`, default `base`). YouTube URLs (via `synaptic ingest url`) instead use `yt-dlp` to fetch subtitles as WebVTT, which are parsed to plain text.

```
synaptic ingest media talk.mp3
```

Requires a build with `--features media`, plus the relevant external tool (`whisper` and/or `yt-dlp`) on `PATH`. Intermediate transcript files are written to an isolated temp directory so only the final `.md` lands in the ingested directory.

## After a shape-A ingest

Shape-A sources (url, office, gws, media) only write a Markdown file. Run an extract or update to index it:

```
synaptic ingest url https://arxiv.org/abs/2401.00001
synaptic extract .
```
