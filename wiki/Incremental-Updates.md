# Incremental Updates

Synaptic can rebuild the graph incrementally after files change, watch the working tree and rebuild automatically, and install git hooks (plus a `graph.json` merge driver) so the graph stays current as you commit, check out branches, and merge.

All three features re-extract only the files that changed, merge the fresh AST into the existing `synaptic-out/graph.json`, and preserve everything that did not change, including LLM-produced semantic nodes (see [Semantic-Analysis]). They never run the LLM semantic pass; that is `extract --semantic` only.

See also: [Commands], [Extraction], [Configuration].

## `synaptic update`

Rebuild the graph after files change.

```
synaptic update [PATHS]... [--full] [--directed] [--force] [--artifacts]
```

- `PATHS` are the changed files (repo-relative or absolute). Each is re-extracted if it still exists and is a code or Markdown file; otherwise it is treated as deleted and its nodes are evicted.
- `--full` rebuilds every code (and Markdown) file from scratch. This drops stale AST nodes for files that no longer exist and reconciles against the current file set, while preserving semantic/concept nodes.
- `--directed` builds a directed graph only when there is no existing graph to inherit from (otherwise the existing graph's `directed` flag is reused).
- `--force` bypasses the shrink guard (see below).
- `--artifacts` also regenerates the visual/export artifact suite (see Outputs below).

With no paths and no `--full`, `update` reads the newline-delimited `SYNAPTIC_CHANGED` environment variable (set by the post-commit/post-merge hooks) for the changed-file list. If that is also empty, a **bare `synaptic update` catches up from the provenance manifest**: it diffs the working tree against the state recorded at the last build and rebuilds exactly the files that were added, changed, or removed since (the same semantics as the serve catch-up). If nothing changed, it reports `No changes detected since the last build` and exits. With no existing graph, or a graph that has **no provenance manifest** (built by an older binary), it performs a full rebuild — the drift is unknown, and trusting a freshly bootstrapped baseline would mask it.

What it does:

1. Acquires a per-repo rebuild lock under `synaptic-out/`. If another rebuild holds the lock, the changed paths are appended to a pending queue and `update` returns. The lock holder drains the queue **in a loop** — including paths queued while its own rebuild was running — so nothing waits for a later invocation.
2. Loads the existing `synaptic-out/graph.json` (inheriting its `directed` flag).
3. Builds the provenance manifest **before** extracting and persists it only after `graph.json` is written, advancing **only the entries this rebuild ingested** — so a file edited mid-rebuild, or changed on disk but outside the given change set (an uncommitted edit when a hook lists committed files), still diffs as changed later, and provenance never runs ahead of the graph on disk.
4. Re-extracts the target files in parallel, using the on-disk extraction cache. A file that exists but cannot be read (a transient editor/AV lock) keeps its previous nodes and edges, and is dropped from the manifest snapshot so it retries on the next round instead of going silently stale.
5. **Ripple re-resolution**: if the change (re)introduces symbols — a new definition, a rename back, a move to another file — unchanged files whose raw calls reference those names (indexed in `synaptic-out/.callnames.json`) are re-extracted from the AST cache and fed to resolution, so their call edges connect without waiting for those files to change.
6. Merges the fresh AST into the existing graph: fresh nodes replace nodes with the same id; unchanged files' AST and all semantic nodes survive; nodes whose source file was evicted are dropped; an existing edge survives only when both endpoints are still live **and** the edge did not originate from a re-extracted file (a re-extracted file's edges come back fresh, so they are replaced rather than union-merged with the old set). On a full rebuild, edges originating from AST nodes are likewise replaced wholesale, never unioned. Hyperedges carry over.
7. Re-resolves cross-file symbols and re-runs the cross-language passes and entity dedup (chained, with a single final graph build). Communities: a small delta keeps the previous assignment exactly and places only the new nodes with their neighbours; a large delta or full rebuild re-clusters from scratch (remapped to the previous ids for stability).

Outputs: `graph.json` (written atomically) plus the provenance manifest and call-name sidecar. With `--artifacts`, also the visual/export suite (`graph.html`, `GRAPH_REPORT.md`, `graph.graphml`, `graph.cypher`, `graph.dot`, `callflow.html`, `tree.html`, `graph.svg`, `graph-3d.html`) — skipped by default because an update runs on every save in the watch/hook flows, where regenerating them dominates the cost.

### The shrink guard

A rebuild that would reduce the node count without an explicit deletion (a removed/missing file) or `--force` is refused, to catch accidental data loss. Use `--force` to allow a legitimate shrink.

### No-change short-circuit

If the rebuilt topology (node id set plus `(source, target, relation)` edge triples) equals the prior graph, `update` reuses the previous community assignment, skips re-clustering, and does not rewrite the artifacts:

```
No changes — graph is up to date (1234 nodes).
```

### What changes trigger a rebuild

`update` and `watch` re-extract code files (any language Synaptic classifies as Code; see [Languages]) and Markdown documents (`.md`, `.mdx`, `.qmd`), matching `synaptic extract`. Markdown is included because heading hierarchy gets structural extraction. Other file types are not re-extracted (though a deleted file of any type listed in `PATHS` still evicts its nodes).

## `synaptic watch`

Watch the working tree and rebuild incrementally on each change.

```
synaptic watch [--directed] [--force] [--artifacts] [--debounce-ms <n>]
```

- Watches the current directory recursively. At startup it first **catches up** on anything that changed while it was not running (a bare-`update` manifest diff), so the graph is current before the first event.
- Debounces a burst of saves into a single rebuild; the settle window defaults to 3000 ms and is configurable with `--debounce-ms` or `SYNAPTIC_WATCH_DEBOUNCE_MS`.
- Ignores changes inside output/VCS/build/dependency subtrees so the watcher never rebuilds in response to its own output. The ignore rules are detect's own noise rules (`synaptic-out`, `.git`, `target`, `node_modules`, `dist`, `.next`, `coverage`, virtualenvs, ...), applied to repo-relative paths so a checkout under a directory that happens to carry a noise name is unaffected.
- Only code files and Markdown (`.md`/`.mdx`/`.qmd`) edits trigger a rebuild; other edits in a batch are dropped. A burst that is entirely ignored or non-rebuildable produces no rebuild.
- Each batch of changed paths is routed through `update` (which holds the rebuild lock). `--directed`, `--force`, and `--artifacts` behave as for `update` (by default each rebuild writes `graph.json` only).

```
Watching /path/to/repo for changes (debounce 3000ms; Ctrl-C to stop)…

Detected 2 changed code file(s) → rebuilding…
```

Stop with Ctrl-C.

## `synaptic serve` auto-freshen (on-query catch-up)

The MCP server (`synaptic serve`) keeps its graph current by refreshing lazily, on the next query — or, with `--watch`, by reacting to filesystem events.

How it works:

- Each MCP tool call (and the CLI, via the same manifest) first does a cheap staleness check: it compares the working tree against the manifest recorded when the graph was last built (file mtime plus content hash; the walk stats files, it does not read them). If files were added, changed, or removed since then, it runs an incremental `update` for exactly those files before answering the query, so the answer reflects your latest edits.
- The check is **debounced** (`SYNAPTIC_SERVE_AUTOFRESH_DEBOUNCE_MS`, default 1000 ms): a burst of queries walks the tree at most once per window.
- **`serve --watch`** (or `SYNAPTIC_SERVE_WATCH=1`) embeds a filesystem watcher and makes the check event-driven instead: no walk and no debounce window per query — a query only pays the staleness check when a relevant file actually changed since the last one. The first query still catches up on edits made before the watcher started. If the watcher cannot start, serve falls back to the debounced walk.
- It is **skipped for large change sets** (`SYNAPTIC_SERVE_AUTOFRESH_MAX_FILES`, default 500): a branch switch that touches hundreds of files should not block a single query on a near-full rebuild. While the graph is being served stale this way, **every tool result carries a `graph is STALE` note** telling the agent to run `synaptic update`; the note clears once the graph refreshes.
- Auto-freshen is on by default and can be disabled with `SYNAPTIC_SERVE_AUTOFRESH=0` (also `false`/`no`/`off`). It disables itself for a **federated** graph (a single-root rebuild would corrupt member ids) — refresh members individually.

The practical consequence: a file you edit but never query does not land on disk on its own — `synaptic-out/graph.json` updates on the **next** MCP/CLI query that triggers the staleness check, not the instant you save. For an agent loop this is transparent (every query refreshes first); if you need the graph written out without issuing a query, run `synaptic update` or use `watch`/`hook` for eager rebuilds.

## `synaptic hook`

Install git hooks and the `graph.json` merge driver so the graph stays current across commits, branch switches, and merges.

```
synaptic hook install
synaptic hook uninstall
synaptic hook status
```

The hooks call the native `synaptic` binary directly (the path is forward-slashed so it works under git's POSIX `sh`, including git-for-Windows).

### Hooks installed

- `post-commit` runs an incremental `update` on the commit's changed files, listed via `git diff-tree --root --no-commit-id --name-only -r -m HEAD` — which also covers the repository's **first commit** (no parent to diff against) and merge commits. It is backgrounded so it never blocks the commit, writing its log to `synaptic-out/.rebuild.log`. The changed files are passed via the `SYNAPTIC_CHANGED` environment variable (newline-delimited), never as command arguments, so paths with spaces or glob characters survive intact.
- `post-checkout` runs a full rebuild (`update --full`) on a branch switch (only when the checkout's "branch flag" is set), and only when a `synaptic-out` directory exists. Also backgrounded.
- `post-merge` runs an incremental `update` on the files a `git merge` / `git pull` brought in (`git diff --name-only ORIG_HEAD HEAD`) — including a **fast-forward**, which fires neither of the other two hooks. Also backgrounded.

All hooks:

- Skip when `SYNAPTIC_SKIP_HOOK=1`.
- Skip when only `synaptic-out/` files changed (anti-loop guard).

`post-commit` additionally skips during rebase, merge, and cherry-pick (it checks for `rebase-merge`, `rebase-apply`, `MERGE_HEAD`, `CHERRY_PICK_HEAD` in the git dir).

### Idempotent and shared-hook-safe

Hook scripts are wrapped in a marker block (`# >>> synaptic hook >>>` ... `# <<< synaptic hook <<<`). Re-running `install` replaces the block in place. If a hook file already exists with foreign content, the Synaptic block is appended and that content is preserved. `uninstall` removes only the Synaptic block; a hook file Synaptic solely created is deleted, while foreign content is left intact.

The install resolves the hooks directory honoring `core.hooksPath` (including Husky 9's `.husky/_` redirect to the parent `.husky/`) and git worktrees. A `core.hooksPath` that escapes the repository root is rejected and the default in-repo hooks directory is used instead (supply-chain hardening).

### The graph.json merge driver

`hook install` also registers a union merge driver for `graph.json`:

- Adds a line to `.gitattributes` (idempotent):

  ```
  synaptic-out/graph.json merge=synaptic
  ```

- Sets git config `merge.synaptic.name` and `merge.synaptic.driver` (the driver invokes `synaptic merge-driver %O %A %B`).

When two branches both rebuilt the graph, git invokes the driver instead of producing a textual conflict. The driver union-composes the two sides (the "other" side wins on a node-id collision; edges union by `(source, target, relation)`; hyperedges union by id) and writes the result back, so `graph.json` never conflicts. The base (`%O`) is unused, since a union cannot lose nodes.

The driver is fail-loud: a corrupt or oversized input (over 50 MB, or a merged graph over 100,000 nodes) returns an error so git surfaces a real conflict rather than silently writing garbage. `synaptic merge-driver` is invoked by git, not by users (it is hidden from the command list).

### `hook status`

Reports which hooks currently contain the Synaptic marker block:

```
  post-commit — installed
  post-checkout — installed
  post-merge — installed
```
