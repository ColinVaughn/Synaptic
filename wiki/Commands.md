# Commands

Complete reference for the `codegraph` CLI. Every command is a subcommand of `codegraph` (for example `codegraph extract`, `codegraph query "..."`). Run `codegraph --help` for the generated summary, or `codegraph <command> --help` for a single command.

Most read commands operate on `codegraph-out/graph.json` by default; build it first with [`extract`](#extract). See [Quickstart](Quickstart) for an end-to-end walkthrough.

## Summary

| Command | Purpose |
| --- | --- |
| [`extract`](#extract) | Build the graph for a directory and write `codegraph-out/`. |
| [`export`](#export) | Re-emit an output format from an existing `graph.json` (no re-extraction), or push live to a database. |
| [`query`](#query) | Find a relevant subgraph for a free-text query. |
| [`path`](#path) | Shortest path between two nodes. |
| [`explain`](#explain) | Show a node and its neighbours. |
| [`update`](#update) | Incrementally rebuild after files change (or fully with `--full`). |
| [`watch`](#watch) | Watch the working tree and rebuild on change (debounced). |
| [`affected`](#affected) | Nodes that transitively depend on a node (reverse-impact). |
| [`hook`](#hook) | Manage git hooks and the `graph.json` merge driver. |
| [`serve`](#serve) | Run the MCP server (stdio or HTTP). |
| [`ingest`](#ingest) | Ingest an external source into the graph, or fetch a URL for the next extract. |
| [`install`](#install) | Install the CodeGraph skill for a host assistant. |
| [`uninstall`](#uninstall) | Remove the CodeGraph skill for a platform (or all). |
| [`prs`](#prs) | Graph-aware PR dashboard and per-PR detail. |
| [`skill`](#skill) | Maintain the generated skill artifacts (dev/CI). |
| [`workspace`](#workspace) | Multi-repo / monorepo federation. |
| [`global`](#global) | Manage the cross-repo global graph store (`~/.codegraph`). |
| [`merge-graphs`](#merge-graphs) | Compose several `graph.json` files into one namespaced graph. |
| [`cache`](#cache) | Maintain the on-disk extraction cache. |

There is also an internal `merge-driver` command. It is hidden from `--help` and invoked by git, not users; see [`hook`](#hook).

## extract

Scan a directory, build the knowledge graph, and write the artifact set to `codegraph-out/`.

Syntax:

```sh
codegraph extract [PATH] [--directed] [--obsidian] [--wiki] [--semantic]
```

Arguments and flags:

| Name | Default | Description |
| --- | --- | --- |
| `PATH` | `.` | Root directory to scan. |
| `--directed` | off | Produce a directed graph. |
| `--obsidian` | off | Also write an Obsidian vault (one note per node) under `codegraph-out/obsidian/`. |
| `--wiki` | off | Also write a Markdown wiki under `codegraph-out/wiki/`. |
| `--semantic` | off | Run the LLM semantic pass over documents/papers and enable the LLM dedup tiebreaker. Requires an API key in the environment (for example `OPENAI_API_KEY`). Makes paid API calls. |

The default run is fully offline and needs no API key. It always writes `graph.json`, `graph.html`, `GRAPH_REPORT.md`, `graph.graphml`, `graph.cypher`, `graph.dot`, `callflow.html`, `tree.html`, `graph.svg`, and `graph-3d.html` into `codegraph-out/`. With `--obsidian` and `--wiki` it adds the `obsidian/` and `wiki/` directories. Markdown heading structure is always extracted; the LLM concept pass runs only with `--semantic`.

Example:

```sh
codegraph extract . --directed
```

See [Extraction](Extraction), [Output-Formats](Output-Formats), [Semantic-Analysis](Semantic-Analysis), and [Visualizations](Visualizations).

## export

Regenerate one output format from an existing `graph.json` without re-extracting, or push the graph live to a database.

Syntax:

```sh
codegraph export <FORMAT> [--graph <PATH>] [--out <PATH>] [--repo <TAG>] [--push <URI>] [--user <USER>] [--password <PW>]
```

Arguments and flags:

| Name | Default | Description |
| --- | --- | --- |
| `FORMAT` | required | One of: `json`, `html`, `svg`, `graphml`, `cypher`, `dot`, `callflow` (alias `callflow-html`), `tree`, `3d` (alias `force3d`), `obsidian`, `wiki`, `report`, `neo4j`, `falkordb`. |
| `--graph` | `codegraph-out/graph.json` | Source `graph.json`. |
| `--out` | alongside the source `graph.json` | Output file or directory. |
| `--repo` | none | Scope to one federated member (its `repo` tag) before exporting. |
| `--push` | none | For `neo4j`/`falkordb`: push live to this URI (for example `bolt://localhost:7687` or `falkordb://localhost:6379`) instead of writing the cypher script. Requires building with `--features push`. |
| `--user` | `neo4j` | Auth user for `--push` (Neo4j). |
| `--password` | none | Auth password for `--push` (or set `NEO4J_PASSWORD` / `FALKORDB_PASSWORD`). |

Notes: `report` recomputes communities and analysis from the loaded graph. `export json --repo X` with no `--out` is rejected, because the default output name would overwrite the source `graph.json` with a scoped subgraph; pass `--out` to write the scoped graph elsewhere. Live `--push` requires a build with `--features push`; otherwise `neo4j`/`falkordb` write the cypher script.

Example:

```sh
codegraph export svg --out diagram.svg
codegraph export neo4j --push bolt://localhost:7687 --password secret
```

See [Output-Formats](Output-Formats) and [Workspaces-and-Federation](Workspaces-and-Federation).

## query

Find a relevant subgraph for a free-text query.

Syntax:

```sh
codegraph query <TEXT> [--graph <PATH>] [--max-nodes <N>] [--repo <TAG>] [--dfs]
```

Arguments and flags:

| Name | Default | Description |
| --- | --- | --- |
| `TEXT` | required | Free-text query. |
| `--graph` | `codegraph-out/graph.json` | Source graph. |
| `--max-nodes` | `30` | Maximum nodes in the returned subgraph. |
| `--repo` | none | Scope to one federated member (its `repo` tag). |
| `--dfs` | off | Expand the subgraph depth-first instead of breadth-first (favors deep call chains over broad neighbourhoods). |

Prints the matched seed nodes and the resulting subgraph (nodes and labelled edges). If nothing matches, it reports no matches.

Example:

```sh
codegraph query "auth token refresh" --max-nodes 50
```

See [Querying](Querying).

## path

Shortest path between two nodes, each given by id or label.

Syntax:

```sh
codegraph path <FROM> <TO> [--graph <PATH>] [--repo <TAG>]
```

| Name | Default | Description |
| --- | --- | --- |
| `FROM` | required | Source node id or label. |
| `TO` | required | Target node id or label. |
| `--graph` | `codegraph-out/graph.json` | Source graph. |
| `--repo` | none | Scope to one federated member (its `repo` tag). |

Prints the path as labels joined by arrows, or a message if one or both endpoints cannot be resolved or no path exists.

Example:

```sh
codegraph path "LoginController" "Database"
```

See [Querying](Querying).

## explain

Show a node and its immediate neighbours.

Syntax:

```sh
codegraph explain <NODE> [--graph <PATH>] [--repo <TAG>]
```

| Name | Default | Description |
| --- | --- | --- |
| `NODE` | required | Node id or label. |
| `--graph` | `codegraph-out/graph.json` | Source graph. |
| `--repo` | none | Scope to one federated member (its `repo` tag). |

Prints the node's label, source file, community (if assigned), and each neighbour with the edge direction and relation.

Example:

```sh
codegraph explain "PaymentService"
```

See [Querying](Querying).

## update

Incrementally rebuild the graph after files change, or do a full rebuild.

Syntax:

```sh
codegraph update [PATHS...] [--full] [--directed] [--force]
```

| Name | Default | Description |
| --- | --- | --- |
| `PATHS...` | none | Changed file paths (repo-relative). Empty plus no `--full` triggers a full rebuild. |
| `--full` | off | Rebuild every code file from scratch (preserves semantic nodes). |
| `--directed` | off | Build directed when there is no existing graph to inherit from. |
| `--force` | off | Bypass the shrink guard. |

Behavior: when no paths are given on the command line (and not `--full`), changed paths are read from the `CODEGRAPH_CHANGED` environment variable (newline-delimited), which is how the post-commit hook passes them. The command inherits the existing graph and its `directed` flag when present, serializes concurrent rebuilds with a lock (queuing paths if another rebuild holds it), and writes the same artifact set as [`extract`](#extract).

Example:

```sh
codegraph update src/auth.rs src/db.rs
codegraph update --full
```

See [Incremental-Updates](Incremental-Updates).

## watch

Watch the working tree and rebuild incrementally on change, debounced.

Syntax:

```sh
codegraph watch [--directed] [--force]
```

| Name | Default | Description |
| --- | --- | --- |
| `--directed` | off | Build directed when there is no existing graph to inherit from. |
| `--force` | off | Bypass the shrink guard on each rebuild. |

Watches the current directory recursively, debounces a burst of saves into one rebuild, ignores the output/VCS/build subtrees (so writing `graph.json` cannot self-trigger), and rebuilds on changed code and Markdown (`.md`, `.mdx`, `.qmd`) files. Stop with Ctrl-C.

Example:

```sh
codegraph watch --directed
```

See [Incremental-Updates](Incremental-Updates).

## affected

List nodes that transitively depend on a node (reverse-impact analysis).

Syntax:

```sh
codegraph affected <NODE> [--graph <PATH>] [--depth <N>] [--relation <REL>]...
```

| Name | Default | Description |
| --- | --- | --- |
| `NODE` | required | Node id, label, bare name, source file, or unique label substring. |
| `--graph` | `codegraph-out/graph.json` | Source graph. |
| `--depth` | `2` | Max hops to walk backward. |
| `--relation` | structural impact relations | Restrict to these edge relations; repeatable. |

When no `--relation` is given, the default structural impact relations are: `calls`, `references`, `imports`, `imports_from`, `re_exports`, `inherits`, `extends`, `implements`, `uses`, `mixes_in`, `embeds`, `depends_on`, `reads_from`. Containment relations (such as `contains`/`method`) are intentionally excluded. Output lists each affected node with the relation it was reached through and its source location.

Example:

```sh
codegraph affected src/config.rs --depth 3
codegraph affected "User" --relation calls --relation references
```

See [Querying](Querying) and [Analysis-and-Reports](Analysis-and-Reports).

## hook

Manage git hooks (post-commit/post-checkout) and the `graph.json` merge driver.

Syntax:

```sh
codegraph hook <ACTION>
```

Actions:

| Action | Description |
| --- | --- |
| `install` | Install the hooks and register the merge driver (idempotent). |
| `uninstall` | Remove the hooks (and the CodeGraph blocks from any shared hook files). |
| `status` | Show which hooks are currently installed. |

Each action prints the per-hook installed/not-installed state. The hooks keep `graph.json` current after commits and checkouts; the merge driver union-composes both sides so `graph.json` never conflicts during merges.

Example:

```sh
codegraph hook install
codegraph hook status
```

See [Incremental-Updates](Incremental-Updates).

## serve

Run the MCP server exposing read-only graph tools (and PR tools) to an AI assistant.

Syntax:

```sh
codegraph serve [--graph <PATH>] [--http <ADDR>] [--api-key <KEY>] [--source-root <DIR>]
```

| Name | Default | Description |
| --- | --- | --- |
| `--graph` | `codegraph-out/graph.json` | Graph to serve. |
| `--http` | none (stdio) | Serve over HTTP at this address (for example `127.0.0.1:8765`) instead of stdio. The MCP endpoint is `/mcp`. |
| `--api-key` | none | Require this API key for HTTP requests (or set `CODEGRAPH_API_KEY`). |
| `--source-root` | dir above `codegraph-out/` | Trusted root for resolving a node's source file in the `get_source` tool (path-traversal jailed). |

Defaults to stdio transport. The MCP server reports protocol `2025-06-18` and exposes 17 read-only tools, prompts, completions, resource templates/subscriptions, and structured tool output. When serving HTTP on a wildcard address with no API key, it prints a warning.

Example:

```sh
codegraph serve
codegraph serve --http 127.0.0.1:8765 --api-key secret
```

See [MCP-Server](MCP-Server) and [Assistant-Integration](Assistant-Integration).

## ingest

Ingest an external source into the graph, or fetch a URL into `codegraph-out/ingested/` for the next extract.

Syntax:

```sh
codegraph ingest <SOURCE> [SOURCE-ARGS]
```

Sources:

| Source | Argument | Description |
| --- | --- | --- |
| `cargo` | `<PATH>` | A Cargo workspace root; adds crate nodes and internal-dependency edges. |
| `mcp` | `<FILE>` | An MCP config file (`.mcp.json`, `claude_desktop_config.json`, etc.). |
| `scip` | `<FILE>` | A SCIP-index JSON file (simplified shape); adds symbol nodes and edges. |
| `pg` | `[DSN]` (default empty) | A live PostgreSQL database; adds table/view/function nodes and foreign-key edges. Empty DSN uses `PG*` env vars. Requires `--features pg`. |
| `url` | `<URL>` | A URL, fetched (SSRF-guarded) into `codegraph-out/ingested/`. |
| `office` | `<FILE>` | An office spreadsheet (`.xlsx`/`.xls`/`.ods`), converted to markdown in `codegraph-out/ingested/`. Requires `--features office`. |
| `gws` | `<FILE>` | A Google-Workspace pointer (`.gdoc`/`.gsheet`/`.gslides`), exported to markdown via the `gws` CLI. Requires `--features gws`. |
| `media` | `<FILE>` | A local audio/video file, transcribed to markdown. Requires `--features media`. |

The `cargo`, `mcp`, `scip`, and `pg` sources merge their nodes/edges into `codegraph-out/graph.json` and rewrite the artifacts. The `url`, `office`, `gws`, and `media` sources write into `codegraph-out/ingested/` and require a follow-up `codegraph extract` (or `update`) to index. Feature-gated sources error with a rebuild hint when the feature is not compiled in.

Example:

```sh
codegraph ingest cargo .
codegraph ingest url https://example.com/spec.html
```

See [Ingestion](Ingestion).

## install

Install the CodeGraph skill for a host assistant in the current directory.

Syntax:

```sh
codegraph install [PLATFORM] [--global]
```

| Name | Default | Description |
| --- | --- | --- |
| `PLATFORM` | `claude` | One of: `claude`, `agents`, `codex`, `opencode`, `gemini`, `cursor`, `copilot`, `kilo`. |
| `--global` | off | Codex only: register the MCP server in the global `~/.codex/config.toml` (per-repo server) for the Codex desktop app, instead of the project `.codex/`. |

`codex` gets extra wiring: a native MCP server and a `SessionStart` hook (project
`.codex/` by default, or a global per-repo server with `--global` for the desktop
app). Prints the files written.

Examples:

```sh
codegraph install claude
codegraph install codex            # Codex CLI (project .codex/)
codegraph install codex --global   # Codex desktop app (global ~/.codex)
```

See [Assistant-Integration](Assistant-Integration).

## uninstall

Remove the CodeGraph skill for a platform.

Syntax:

```sh
codegraph uninstall [PLATFORM] [--all] [--global]
```

| Name | Default | Description |
| --- | --- | --- |
| `PLATFORM` | `claude` | The platform to remove (same set as [`install`](#install)). |
| `--all` | off | Uninstall from every supported platform (ignores `PLATFORM`). |
| `--global` | off | Codex only: remove this repo's server from the global `~/.codex/config.toml`. |

Example:

```sh
codegraph uninstall cursor
codegraph uninstall codex --global
codegraph uninstall --all
```

See [Assistant-Integration](Assistant-Integration).

## prs

Graph-aware PR dashboard, single-PR detail, triage, and conflict views. Requires the `gh` CLI.

Syntax:

```sh
codegraph prs [NUMBER] [--repo <OWNER/NAME>] [--base <BRANCH>] [--graph <PATH>] [--triage] [--conflicts]
```

| Name | Default | Description |
| --- | --- | --- |
| `NUMBER` | none | PR number for a detailed view; omit for the dashboard. |
| `--repo` | current directory's repo | Target repo `owner/name`. |
| `--base` | the repo's default branch | Base branch to filter to. |
| `--graph` | `codegraph-out/graph.json` | Graph used for blast-radius (communities + node count). |
| `--triage` | off | Ranked actionable PRs with blast radius (deterministic; no LLM). |
| `--conflicts` | off | PRs that touch the same graph community (merge-order risk). |

With no number and no flags it prints the open-PR dashboard. A number shows that PR's detail including graph blast radius. `--conflicts` and `--triage` are dashboard-level views and take precedence over a PR number. Graph blast radius is attached only when a `graph.json` is present. For LLM-summarized triage, use the MCP server's `triage_prs` tool.

Example:

```sh
codegraph prs
codegraph prs 142
codegraph prs --triage
```

See [PR-Dashboard](PR-Dashboard).

## skill

Maintain the generated skill artifacts (development/CI). Checks for drift against the committed snapshots, or re-blesses them after an intentional change.

Syntax:

```sh
codegraph skill <ACTION>
```

| Action | Description |
| --- | --- |
| `check` | Re-render the skill artifacts and fail if they differ from the committed `expected/` snapshots (CI anti-drift guard). |
| `bless` | Rewrite the committed `expected/` snapshots from the current render. |

Example:

```sh
codegraph skill check
```

See [Development](Development).

## workspace

Multi-repo / monorepo federation: discover members, build a federated graph, and scope queries to a repo.

Syntax:

```sh
codegraph workspace <ACTION>
```

Actions:

| Action | Syntax | Description |
| --- | --- | --- |
| `init` | `init [--scan-repos [DIR]] [--depth <N>] [--max <N>]` | Write `codegraph-workspace.toml`, auto-discovering members. `--scan-repos` also scans a parent dir for sibling git repos and appends `[[repos]]` entries (bare flag scans the parent of the current repo). `--depth` defaults to `3`, `--max` defaults to `50`. |
| `add` | `add <TARGET>` | Add a member: an existing local path becomes a `members` entry, a git URL becomes a `[[repos]]` entry. |
| `discover` | `discover [PATH] [--depth <N>] [--max <N>]` | Scan a parent dir for sibling git repos and federate them without writing a manifest. `PATH` defaults to the parent of the current repo; `--depth` defaults to `3`, `--max` to `50`. |
| `build` | `build [--changed] [--directed]` | Build all members and federate into `codegraph-out/graph.json`. `--changed` only rebuilds members that changed; `--directed` produces a directed federated graph. |
| `federate` | `federate <DIR>` | Compose from a directory of published `<member>/graph.json` artifacts. |
| `sync` | `sync` | Pull remote git members, then rebuild deltas. |
| `status` | `status` | Show each member's change status (no build). |
| `list` | `list` | List the workspace members (local members and remote repos). |

`build`, `federate`, `discover`, and `sync` write the federated graph artifacts plus a `surfaces/` directory of per-member export surfaces, and print a build summary (node/edge/community counts, cross-repo links, per-member sizes).

Example:

```sh
codegraph workspace init --scan-repos
codegraph workspace build --directed
codegraph workspace status
```

See [Workspaces-and-Federation](Workspaces-and-Federation).

## global

Manage the cross-repo global graph store at `~/.codegraph`.

Syntax:

```sh
codegraph global <ACTION>
```

| Action | Syntax | Description |
| --- | --- | --- |
| `add` | `add <GRAPH> [--as <TAG>]` | Add (or update) a repo's `graph.json` under a tag. The tag defaults to the graph's grandparent directory name. Skipped when the source is unchanged. |
| `remove` | `remove <TAG>` | Remove a repo's nodes from the global store. |
| `list` | `list` | List the repos in the store with node/edge counts and source path. |
| `path` | `path` | Print the global graph path. |

Example:

```sh
codegraph global add ./codegraph-out/graph.json --as backend
codegraph global list
```

See [Workspaces-and-Federation](Workspaces-and-Federation).

## merge-graphs

Compose several `graph.json` files into one namespaced graph.

Syntax:

```sh
codegraph merge-graphs <GRAPHS...> [--out <PATH>]
```

| Name | Default | Description |
| --- | --- | --- |
| `GRAPHS...` | required (at least one) | The `graph.json` files to merge. Each file's tag is its grandparent directory name. |
| `--out` | `codegraph-out/merged-graph.json` | Output path. |

Prints the number of graphs merged, the output path, node/edge totals, and the tags. Errors if no graphs are given.

Example:

```sh
codegraph merge-graphs repo-a/codegraph-out/graph.json repo-b/codegraph-out/graph.json --out merged.json
```

See [Workspaces-and-Federation](Workspaces-and-Federation).

## cache

Maintain the on-disk extraction cache at `codegraph-out/cache`.

Syntax:

```sh
codegraph cache <ACTION>
```

Action `clear`:

```sh
codegraph cache clear [PATH] [--recursive]
```

| Name | Default | Description |
| --- | --- | --- |
| `PATH` | `.` | Repo/workspace root whose `codegraph-out/cache` to remove. |
| `--recursive` | off | Also remove every `codegraph-out/cache` found beneath `PATH` (federated member caches), via a bounded, noise-pruned walk that skips `node_modules` and `.git`. |

The AST cache normally self-invalidates when extractors change; use `clear` for a guaranteed cold start or suspected corruption. Only the regenerable `codegraph-out/cache` subtree is ever removed.

Example:

```sh
codegraph cache clear
codegraph cache clear . --recursive
```

See [Extraction](Extraction).

## merge-driver (internal)

`codegraph merge-driver <BASE> <CURRENT> <OTHER>` is a git merge driver for `graph.json`. It is hidden from `--help` and invoked by git as `%O %A %B`, not by users. It union-composes both sides into `CURRENT` so `graph.json` never conflicts. It is registered automatically by `codegraph hook install`.
