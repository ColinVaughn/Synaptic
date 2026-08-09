# Extraction

`synaptic extract` turns a directory of source code into a knowledge graph. It
discovers files, parses each supported language into nodes and edges, resolves
cross-file references, clusters the result into communities, and writes a set of
artifacts under `synaptic-out/`.

```
synaptic extract .
synaptic extract path/to/project
synaptic extract . --directed --wiki
```

This page covers the discovery and parsing stages: which files are found, which
are skipped, how ignore files are honored, how sensitive files are excluded, and
how the on-disk AST cache works. For how parsed facts become the final graph and
reports, see [Analysis-and-Reports] and [Output-Formats]. For the optional
LLM-driven concept layer, see [Semantic-Analysis].

## Pipeline overview

1. Discover and classify every file under the root.
2. Read and parse each code file in parallel, producing per-file nodes, edges,
   raw calls, and import records.
3. Resolve relative and alias imports to real file nodes.
4. Extract Markdown heading structure (always, independent of `--semantic`).
5. Optionally run the LLM semantic pass over documents and papers (`--semantic`).
6. Build the graph, resolve cross-file calls, deduplicate entities, cluster into
   communities, and analyze.
7. Write artifacts into `synaptic-out/`.

## Node metadata: kind, visibility, span, signature

Code nodes carry structured metadata beyond their label and location:

- **`kind`** — what the node is: `class`, `interface`, `trait`, `struct`, `enum`,
  `function`, `method`, and so on (the `Other` fallback when a declaration can't be
  classified).
- **`visibility`** — `public`, `protected`, `private`, or `internal`, read from
  language modifiers (Java/C#/Kotlin/Swift/TS), Rust `pub`, Go name capitalization,
  or the Python `_name` convention. Absent when a language has no visibility concept.
- **`span`** — the full source range (`start_line`, `start_col`, `end_line`,
  `end_col`), from which lines-of-code (`loc`) is derived.
- **`signature`** — for functions and methods, the captured parameter list and
  return type: `params` (each a `name` plus an optional `type_ref`), an optional
  `return_type`, and a `raw` verbatim header. Parameter *names* are captured
  wherever the grammar exposes them (the config-driven languages plus Go and
  Rust); parameter and return *types* only when the source annotates them, with
  the `raw` header always kept as a fallback so a description is never empty.
- **`dynamic_sites`** — reflection / dynamic-dispatch sites found in the node's
  body (each a `kind`, `line`, optional string-literal `key`, and a source
  `snippet`). These feed the honesty caveat and the
  [`dynamic_hazards`](MCP-Server#dynamic_hazards) tool; see
  [Cross-Language Edges](Cross-Language-Edges#dynamic-dispatch).
- **`dynamically_referenced`** — set when an evidence-link resolved a reflection
  site's literal key to this node, i.e. it may be reachable only at runtime.

These are populated for the config-driven languages (Python, JavaScript/TypeScript,
Java, C#, Kotlin, Swift, C, C++, PHP, Scala, Groovy) and for Go and Rust. Other
languages omit the fields rather than guessing, so consumers treat a missing value
as "unknown". The fields appear in `graph.json` (and in Cypher/GraphML output), and
the MCP `get_node` tool surfaces them. `kind`/`visibility`/`loc` power
[architectural search] and visibility-aware [time-travel] "removed API"
detection; `signature` feeds the MCP `describe_node` tool and the structured
`structural_search` output.

## Cross-language edges

Beyond the per-file parse, an opt-in post-pass detects coupling that no single
language parse can see: subprocess invocations, FFI bindings, HTTP/gRPC service
calls, WebSocket and message-queue exchanges, and (as evidence-linked
metadata) event-bus / IPC / reflection dispatch. These add `invokes` /
`binds_native` / `calls_service` / `handled_by` (plus `dynamic_ref`) edges at
`INFERRED` confidence, across the full language set — Python, JS/TS (incl.
Vue/Svelte/Astro), Go, Rust, Java/Kotlin, C#, PHP, Ruby, C/C++, and shell — so
impact analysis traverses language boundaries. See
[Cross-Language-Edges](Cross-Language-Edges) for the full model.

HTTP boundary discovery includes framework-declared routes as well as routes
whose URL comes from source layout: Next.js App Router and Pages Router,
Supabase/Deno edge functions, and Symfony `#[Route]` attributes. Their handlers
are emitted as `handled_by` edges, so the same graph evidence can establish
vulnerability exposure from an externally reachable entry point.

[architectural search]: Commands
[time-travel]: Commands

## Directory discovery

Discovery walks the root with the `ignore` crate's directory walker. The walk:

- Visits dotfiles and dotted directories (hidden files are not skipped by
  default; the noise and sensitive rules below handle exclusions).
- Honors `.gitignore`, git exclude files, and `.synapticignore` (see below).
- Follows symlinks, but prunes any symlink whose real target resolves outside
  the scan root (an escape and cycle guard).
- Prunes known noise directories so it never descends into them.
- Classifies each remaining file by extension (papers also get a content sniff).

Each code file is read and extracted with its path taken **relative to the
root**, so node ids and `source_file` values are portable across machines and
checkouts. Results are collected in a stable, path-sorted order, so `graph.json`
is deterministic regardless of thread scheduling.

## Skipped directories (noise)

The following directory names are pruned wherever they appear (build output,
caches, dependency trees, and Synaptic's own output):

```
venv  .venv  env  .env  node_modules  __pycache__  .git  dist  build
target  out  site-packages  lib64  .pytest_cache  .mypy_cache  .ruff_cache
.tox  .eggs  synaptic-out  coverage  lcov-report  visual-tests  visual-test
__snapshots__  snapshots  storybook-static  dist-protected  .next  .nuxt
.turbo  .angular  .idea  .cache  .parcel-cache  .svelte-kit  .terraform
.serverless  .synaptic  .worktrees
```

Additional rules:

- Any directory name ending in `_venv`, `_env`, or `.egg-info` is pruned.
- A directory literally named `worktrees` nested inside a dotted directory (for
  example `.git/worktrees/`) is pruned.

## Skipped files (lockfiles)

These lockfiles are never indexed:

```
package-lock.json  yarn.lock  pnpm-lock.yaml  Cargo.lock  poetry.lock
Gemfile.lock  composer.lock  go.sum  go.work.sum
```

## `.synapticignore` and `.gitignore`

Both ignore files are honored, layered per-directory up to the VCS root:

- `.gitignore` rules apply (along with git exclude files). Global gitignore is
  not consulted.
- `.synapticignore` uses the same gitignore syntax (globs, `!` negations).
- On conflicting rules, `.synapticignore` takes precedence over `.gitignore`
  (for example a `!keep.py` re-include in `.synapticignore` wins over a
  `keep.py` exclude in `.gitignore`).
- A subdirectory's `.gitignore` still applies even when a root
  `.synapticignore` exists. The two layer; one does not disable the other.
- A negation cannot rescue a file beneath an already-excluded directory (this is
  standard gitignore semantics).

Example `.synapticignore`:

```
# Skip generated code but keep one file
generated/
!generated/manifest.py

# Skip large data fixtures
fixtures/**/*.json
```

## Sensitive-file skipping

Files that likely contain secrets are skipped during discovery and reported
separately (they are never read into the graph). Three layers decide:

1. **Parent directory** is one of `.ssh`, `.gnupg`, `.aws`, `.gcloud`,
   `secrets`, `.secrets`, `credentials`.
2. **Filename patterns**, including: `.env` / `.envrc` files; key and
   certificate extensions (`.pem`, `.key`, `.p12`, `.pfx`, `.cert`, `.crt`,
   `.der`, `.p8`); SSH key names (`id_rsa`, `id_dsa`, `id_ecdsa`, `id_ed25519`,
   optionally `.pub`); `.netrc`, `.pgpass`, `.htpasswd`; and
   `aws_credentials` / `gcloud_credentials` / `service.account`.
3. **Load-bearing keywords** in the filename: `credential`, `secret`, `passwd`,
   `password`, `private_key`, `token`. A keyword counts only when it ends the
   file stem or appears in a short (two-word-or-fewer) name. Topic words like
   `tokenizer.py` or `password-policy-discussion.md` are not flagged.

## Parallelism

Code files are parsed in parallel with `rayon`. Each file is read and extracted
independently, and per-file results are merged in the original path-sorted order
so the output stays deterministic. The Markdown structural pass runs in parallel
the same way.

## Memory on a large repository

Peak memory tracks the size of the assembled graph rather than the repository's
file count. The graph is fully resident from the moment it is built until the
last artifact is written, and the write phase is where the peak lands.

As a reference point, a 379,212-node / 995,965-edge repository holds a graph of
roughly 1.8 GiB and `extract .` peaks at about 5.5 GiB. Budget several times the
graph's own size — the export view, the shard store, and the indexes are all
live at some point during the write phase.

Knobs, in the order worth reaching for:

| knob | effect |
|---|---|
| `.synapticignore` | Excluding generated trees and large data fixtures is usually the largest single win, because it removes nodes rather than deferring work. |
| `--no-resources` / `--no-columns` | Fewer nodes in the graph itself, which is the term everything else scales against. |
| `SYNAPTIC_EXTRACT_THREADS` (see [Configuration]) | Fewer workers means fewer per-file results in flight. Each worker also reserves 64 MiB of stack, so the pool has a fixed cost before any parsing. |
| `--no-store` | Skips the shard store. Since 0.9.8 the store adds well under a gigabyte, so this is rarely the right lever — and the store is what shard-aware serving reads. |

Whole-graph text exports (GraphML, Cypher, DOT, the 3D HTML viewer) are skipped
above 150,000 nodes: they are unusable at that size and were producing
multi-hundred-megabyte files. `graph.json` and the store are always written. See
[Output-Formats].

## The AST cache

Extraction uses an on-disk per-file cache so an unchanged file skips re-parsing
on a rebuild.

- **Location:** `synaptic-out/cache/ast/v{version}/<key>.mp`. Each entry is the
  serialized extraction result for one file, stored as MessagePack (the default
  `cache-binary` feature; ~36% faster to decode and ~14% smaller than JSON, which
  matters most on column-heavy SQL schemas). Built with `--no-default-features`
  and without `cache-binary`, entries are JSON (`<key>.json`) instead. The two
  formats live on distinct extensions, so a cache written by one build is simply
  a miss (never a misread) for the other.
- **Key:** a BLAKE3 hash of `(relative path, file content)`. The path is part of
  the key because node ids embed it, so two files with identical bytes at
  different paths get distinct entries. Any change to a file's bytes is a cache
  miss.
- **Namespace / invalidation:** the `v{version}` segment is
  `{crate version}-{build fingerprint}`. The build fingerprint is computed at
  compile time from the extract crate's source and its enabled `lang-*`
  features, so the cache namespace rotates automatically whenever the extraction
  logic changes (not only on a version bump). This prevents a warm cache from
  serving stale results after an extractor fix.
- **Best-effort I/O:** any read, write, or parse error on a cache entry falls
  back to a fresh extraction. A corrupt cache never blocks a build.

Other caches live under `synaptic-out/cache/` as well: the semantic LLM cache
(`cache/semantic`) and the change-detection manifest (`cache/manifest.json`),
which records per-file hashes so a later build can report what was added,
changed, or removed since last time. See [Incremental-Updates].

Clear the cache with:

```
synaptic cache clear .
synaptic cache clear . --recursive
```

`cache clear` only ever removes the regenerable `synaptic-out/cache` subtree
(and, with `--recursive`, the same subtree under nested project roots).

## What `synaptic-out/` is

`synaptic-out/` is the output directory created in the scan root. It holds:

- `cache/` - the AST cache, semantic cache, and change-detection manifest.
- The generated graph and reports: `graph.json`, `graph.html`,
  `GRAPH_REPORT.md`, `graph.graphml`, `graph.cypher`, `graph.dot`,
  `callflow.html`, `tree.html`, `graph.svg`, and `graph-3d.html`.
- `obsidian/` when `--obsidian` is passed, `wiki/` when `--wiki` is passed.

`synaptic-out` is itself a pruned noise directory, so re-running extraction
never indexes a previous run's output.

## Relevant flags

- `--directed` - build a directed graph (affects layout and analysis).
- `--obsidian` - also write an Obsidian vault under `synaptic-out/obsidian/`.
- `--wiki` - also write a wiki page set under `synaptic-out/wiki/`.
- `--semantic` - opt in to the LLM semantic pass over documents and papers.
  Requires a configured backend; skipped with a note if none is detected. See
  [Semantic-Analysis].

See [Commands] for the full command list and [Output-Formats] for details on
each artifact. The supported languages and what each extracts are listed in
[Languages].
