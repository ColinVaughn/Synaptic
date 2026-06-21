# Incremental Updates

Synaptic can rebuild the graph incrementally after files change, watch the working tree and rebuild automatically, and install git hooks (plus a `graph.json` merge driver) so the graph stays current as you commit, check out branches, and merge.

All three features re-extract only the files that changed, merge the fresh AST into the existing `synaptic-out/graph.json`, and preserve everything that did not change, including LLM-produced semantic nodes (see [Semantic-Analysis]). They never run the LLM semantic pass; that is `extract --semantic` only.

See also: [Commands], [Extraction], [Configuration].

## `synaptic update`

Rebuild the graph after files change.

```
synaptic update [PATHS]... [--full] [--directed] [--force]
```

- `PATHS` are the changed files (repo-relative or absolute). Each is re-extracted if it still exists and is a code or Markdown file; otherwise it is treated as deleted and its nodes are evicted.
- `--full` rebuilds every code (and Markdown) file from scratch. This drops stale AST nodes for files that no longer exist and reconciles against the current file set, while preserving semantic/concept nodes.
- `--directed` builds a directed graph only when there is no existing graph to inherit from (otherwise the existing graph's `directed` flag is reused).
- `--force` bypasses the shrink guard (see below).

With no paths and no `--full`, `update` reads the newline-delimited `SYNAPTIC_CHANGED` environment variable (set by the post-commit hook) for the changed-file list. If that is also empty, and there is no existing graph, it performs a full rebuild.

What it does:

1. Acquires a per-repo rebuild lock under `synaptic-out/`. If another rebuild holds the lock, the changed paths are appended to a pending queue and `update` returns; the lock holder drains the queue and covers them. A lockfile older than 600 seconds (a crashed holder) is treated as stale and stolen.
2. Loads the existing `synaptic-out/graph.json` (inheriting its `directed` flag).
3. Re-extracts the target files in parallel, using the on-disk extraction cache.
4. Merges the fresh AST into the existing graph: fresh nodes replace nodes with the same id; unchanged files' AST and all semantic nodes survive; nodes whose source file was evicted are dropped; an existing edge survives only when both endpoints are still live **and** the edge did not originate from a re-extracted file (a re-extracted file's edges come back fresh, so they are replaced rather than union-merged with the old set); hyperedges carry over.
5. Re-resolves cross-file symbols, re-runs entity dedup, re-clusters communities (remapping ids to the previous build for stability), then writes all artifacts.

Outputs: the rebuilt graph plus the standard artifact set (`graph.json`, `graph.html`, `GRAPH_REPORT.md`, `graph.graphml`, `graph.cypher`, `graph.dot`, `callflow.html`, `tree.html`, `graph.svg`, `graph-3d.html`).

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
synaptic watch [--directed] [--force]
```

- Watches the current directory recursively.
- Debounces a burst of saves into a single rebuild, with a roughly 3-second settle window (`DEBOUNCE_MS = 3000`).
- Ignores changes inside output/VCS/build subtrees so the watcher never rebuilds in response to its own output. Ignored directory names: `synaptic-out`, `.git`, `target`, `node_modules`, `.venv`, `venv`, `__pycache__`, `.mypy_cache`, `.pytest_cache`.
- Only code files and Markdown (`.md`/`.mdx`/`.qmd`) edits trigger a rebuild; other edits in a batch are dropped. A burst that is entirely ignored or non-rebuildable produces no rebuild.
- Each batch of changed paths is routed through `update` (which holds the rebuild lock and writes artifacts). `--directed` and `--force` behave as for `update`.

```
Watching /path/to/repo for changes (debounce 3000ms; Ctrl-C to stop)…

Detected 2 changed code file(s) → rebuilding…
```

Stop with Ctrl-C.

## `synaptic serve` auto-freshen (on-query catch-up)

The MCP server (`synaptic serve`) keeps its graph current **without** a live filesystem watcher. There is no background process tailing your edits — instead the graph is refreshed lazily, on the next query.

How it works:

- Each MCP tool call (and the CLI, via the same manifest) first does a cheap staleness check: it compares the working tree against the manifest recorded when the graph was last built (file mtime plus content hash). If files were added, changed, or removed since then, it runs an incremental `update` for exactly those files before answering the query, so the answer reflects your latest edits.
- The check is **debounced** (`SYNAPTIC_SERVE_AUTOFRESH_DEBOUNCE_MS`, default 1000 ms): a burst of queries walks the tree at most once per window.
- It is **skipped for large change sets** (`SYNAPTIC_SERVE_AUTOFRESH_MAX_FILES`, default 500): a branch switch that touches hundreds of files should not block a single query on a near-full rebuild. Use the post-checkout hook or `synaptic update --full` for those.
- Auto-freshen is on by default and can be disabled with `SYNAPTIC_SERVE_AUTOFRESH=0` (also `false`/`no`/`off`).

The practical consequence: a file you edit but never query does not land on disk on its own — `synaptic-out/graph.json` updates on the **next** MCP/CLI query that triggers the staleness walk, not the instant you save. For an agent loop this is transparent (every query refreshes first); if you need the graph written out without issuing a query, run `synaptic update` or use `watch`/`hook` for eager rebuilds.

## `synaptic hook`

Install git hooks and the `graph.json` merge driver so the graph stays current across commits, branch switches, and merges.

```
synaptic hook install
synaptic hook uninstall
synaptic hook status
```

The hooks call the native `synaptic` binary directly (the path is forward-slashed so it works under git's POSIX `sh`, including git-for-Windows).

### Hooks installed

- `post-commit` runs an incremental `update` on the commit's changed files. It is backgrounded so it never blocks the commit, writing its log to `synaptic-out/.rebuild.log`. The changed files are passed via the `SYNAPTIC_CHANGED` environment variable (newline-delimited), never as command arguments, so paths with spaces or glob characters survive intact.
- `post-checkout` runs a full rebuild (`update --full`) on a branch switch (only when the checkout's "branch flag" is set), and only when a `synaptic-out` directory exists. Also backgrounded.

Both hooks:

- Skip when `SYNAPTIC_SKIP_HOOK=1`.
- Skip during rebase, merge, and cherry-pick (they check for `rebase-merge`, `rebase-apply`, `MERGE_HEAD`, `CHERRY_PICK_HEAD` in the git dir).
- Skip when only `synaptic-out/` files changed (anti-loop guard).

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
```
