# Synaptic pre-release benchmark on six developer-tool repositories

Run date: 2026-07-30  
Host: Windows 10.0.26200, x86_64, 16 logical CPUs  
Binary: optimized `synaptic 0.8.0`, rebuilt from the repository-memory
pre-release worktree at `3f821254305dcdc4c24cd7949bcb0b248c3e4cd8`

This is a workload benchmark of Synaptic **on the source code of six other
developer tools**. It is not a head-to-head performance comparison with those
products.

## Pinned source

| Repository | Pinned SHA |
| --- | --- |
| [Sourcegraph public snapshot](https://github.com/sourcegraph/sourcegraph-public-snapshot) | `c864f15af264f0f456a6d8a83290b5c940715349` |
| [Cody public snapshot](https://github.com/sourcegraph/cody-public-snapshot) | `8e20ac6c1460c08b0db581c0204658112a246eda` |
| [CodeQL](https://github.com/github/codeql) | `7bb0034f4328613ae34acde826c4c5ceafbef5ee` |
| [Joern](https://github.com/joernio/joern) | `80ef1868dbe0ab23566f99dba279026a286c2019` |
| [Aider](https://github.com/Aider-AI/aider) | `5dc9490bb35f9729ef2c95d00a19ccd30c26339c` |
| [graphify](https://github.com/safishamsi/graphify) | `4fe11092ccbe9f543608f140c790f68d5d83cae4` |

The pinned manifest is
[`eval/tool-source-scale-2026-07-30.toml`](tool-source-scale-2026-07-30.toml).

## Extraction scale

The existing `synaptic eval scale` harness ran three repetitions per
repository. Each repetition removes the AST cache, measures a cold build, then
measures a cache-hot build. It also measures a single-file incremental rebuild
against the previous graph. The table reports distinct supported source files
that produced graph nodes and LOC in those files, not every checkout file.

| Repository | Supported files | Supported LOC | Nodes | Edges | Cold median / p95 | Warm median / p95 | Incremental median |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Sourcegraph snapshot | 11,504 | 1,975,437 | 103,317 | 290,662 | 27.14 / 228.66 s | 9.03 / 9.07 s | 4.792 s |
| Cody snapshot | 2,303 | 279,099 | 16,441 | 39,219 | 3.65 / 38.87 s | 1.49 / 1.51 s | 0.800 s |
| CodeQL | 25,846 | 1,567,089 | 269,099 | 582,342 | 151.56 / 167.19 s | 68.57 / 76.79 s | 40.001 s |
| Joern | 1,870 | 1,077,353 | 14,978 | 50,349 | 3.91 / 36.28 s | 1.39 / 1.66 s | 1.003 s |
| Aider | 354 | 168,537 | 4,923 | 7,836 | 1.28 / 6.51 s | 0.83 / 0.89 s | 0.300 s |
| graphify | 722 | 252,195 | 11,307 | 20,948 | 1.77 / 9.63 s | 0.89 / 0.92 s | 0.593 s |
| **Total represented** | **42,599** | **5,319,710** | **420,065** | **991,356** | — | — | — |

The high cold p95 values on Sourcegraph, Cody, Joern, Aider, and graphify are
first-pass outliers. The repositories were partial clones, so initial
materialization and a cold Windows filesystem/antivirus cache affect the first
sample. Median and p95 are both retained rather than dropping that setup cost.

CodeQL needs a separate coverage warning. Most of the repository is written in
QL, which Synaptic does not parse. Its supported-file count comes from the
extractors, fixtures, generated test programs, documentation, and other
supported languages in the checkout. The timing includes discovery and
classification of the whole tree; it is not evidence of QL-language coverage.

That warning describes the original benchmark build. It directly prompted the
QL implementation and follow-up validation below; the original table is kept as
the before-support baseline.

### Follow-up: first-class QL validation

The final review build was rerun against the same pinned CodeQL SHA after adding
`.ql` / `.qll` extraction, QL-aware import and call resolution, a stack-sized
parallel extraction pool, indexed module lookup, and linear-time community
cohesion reporting. No `RUST_MIN_STACK` or `RAYON_STACK_SIZE` override was set.

This was a whole-repository validation run, not a replacement three-repetition
scale benchmark. Its AST cache namespace was cold because the extractor
fingerprint changed; the checkout and operating-system file cache had been
warmed by preceding review runs. Wall time was **144 seconds** from process
start to the final manifest/call-name outputs.

| Measurement | Result |
| --- | ---: |
| Detected files | 42,603 |
| Extracted code files | 37,328 |
| Extracted QL/QLL file nodes | 13,202 |
| QL-tagged file and symbol nodes | 148,902 |
| QL/QLL files parsed without tree-sitter errors | 13,200 (99.985%) |
| Files with parser errors | 2 intentionally incomplete training templates |
| Files with recovered QL signature declarations | 120 |
| Recovered signature declaration nodes | 272 |
| QL import edges | 32,245 |
| QL imports bound to real source files | 30,797 |
| QL imported-module call edges | 10,427 |
| QL unique-fallback call edges | 4,822 |
| QL intra-source predicate call edges | 47,091 |
| Final graph | 423,440 nodes / 920,229 edges / 8,958 communities |
| `graph.json` | 706,764,859 bytes |

The two residual parser-error files are the C++ and Java global-data-flow
training examples under `docs/codeql/ql-training/query-examples/`; both contain
empty predicate bodies represented by `/* TBD */`, so they are exercises rather
than complete valid queries. Current `overlay[...]` annotations are normalized
without changing byte offsets and retained as file metadata. The 120 files
using `signature class/module/predicate` receive an explicit, comment-aware
recovery pass because those declarations are not represented by the upstream
tree-sitter grammar.

The final run also confirmed the scale fixes found during review. The 8,958
community cohesion scores completed in the same second as the HTML graph rather
than repeatedly scanning all 920,229 edges, and extraction completed without a
spawned-worker stack failure. The default 100,000-node and 50 MiB
merge/federation safety caps still warn for this artifact; those limits are
intentional and independent of successful extraction.

## Repository-memory benchmark

The memory run used
[`scripts/benchmark-tool-source-memory.ps1`](../scripts/benchmark-tool-source-memory.ps1).
For each pinned repository it:

1. extracted a current directed graph;
2. ingested 50 real first-parent commits as source-grounded Git episodes;
3. staged the graph aside during Git ingestion so the timing measures episode
   persistence and path lineage rather than parsing the graph 50 times;
4. restored the graph and refreshed document/convention and deterministic
   community memories once;
5. compacted the immutable record store;
6. built up to ten deterministic localization cases from commit intent words
   plus a merge-aware changed path, with the exact `git:<sha>` as the expected
   source; and
7. ran the evaluator five times, reporting median whole-process evaluation
   latency.

| Repository | Cases | Records | Ingest 50 commits | Refresh | Compact | Eval median | R@1 | R@5 | MRR | Mean candidate fraction |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Sourcegraph snapshot | 10 | 2,082 | 19.59 s | 14.01 s | 0.36 s | 727 ms | 100.0% | 100.0% | 1.000 | 3.69% |
| Cody snapshot | 10 | 362 | 18.33 s | 2.38 s | 0.25 s | 177 ms | 100.0% | 100.0% | 1.000 | 23.09% |
| CodeQL | 10 | 8,491 | 35.84 s | 62.19 s | 1.64 s | 4,252 ms | 100.0% | 100.0% | 1.000 | 1.03% |
| Joern | 10 | 293 | 27.01 s | 1.91 s | 0.06 s | 109 ms | 100.0% | 100.0% | 1.000 | 7.75% |
| Aider | 9 | 290 | 19.89 s | 1.68 s | 0.07 s | 138 ms | 66.7% | 88.9% | 0.750 | 17.32% |
| graphify | 10 | 301 | 20.20 s | 1.89 s | 0.07 s | 162 ms | 100.0% | 100.0% | 1.000 | 9.27% |

Pooled over **300 real commits, 11,819 memory records, and 59 localization
cases**:

- recall@1: **94.92%** (56/59);
- recall@5: **98.31%** (58/59);
- mean reciprocal rank: **0.9619**;
- mean candidate fraction: **10.24%**.

The only case missing the top five was Aider commit
`b77180711ccb`. Two other Aider cases ranked at positions two and four. This is
why the pooled result is reported instead of presenting the five perfect
repositories alone.

## Default-configuration and portability findings

The stress run found real boundaries that must accompany the performance
numbers:

1. The Sourcegraph snapshot initially failed checkout on Windows because of a
   path longer than Git's default Windows handling. The pinned checkout
   succeeded after enabling `core.longpaths`.
2. The original CodeQL run overflowed a spawned parsing worker's default stack
   twice and required `RUST_MIN_STACK=67108864`. The follow-up implementation
   moves extraction into its own 64 MiB-stack Rayon pool; the final QL validation
   completed without either stack environment override. A manually observed
   resident-memory peak reached 6.67 GiB while writing the expanded 706.8 MB
   graph, but RSS sampling was not part of the harness and remains diagnostic.
3. The normal CLI correctly refused the two largest graph artifacts at its
   safety boundaries:
   - Sourcegraph: 103,317 nodes and a 171.7 MB `graph.json`;
   - CodeQL: 269,099 nodes and a 438.5 MB `graph.json`.

   Their memory refresh runs explicitly set `SYNAPTIC_MAX_NODES=0` and
   `SYNAPTIC_MAX_GRAPH_MB=0`. Cody, Joern, Aider, and graphify needed no graph
   cap override.
4. Package version remains `0.8.0` in the pre-release worktree. The results
   identify the exact Git base plus the checked-in benchmark manifest and
   script; they should be rerun after the memory changes receive their release
   version and commit.

## What the memory numbers establish

The benchmark shows that deterministic repository memory can localize a known
real commit source across different repository shapes, retain useful
selectivity as generated community summaries increase the record count, and
compact/reload stores containing thousands of records.

It does **not** establish that an LLM writes a given percentage of better code.
The cases are derived from stored commit subjects and changed paths rather than
hand-written from bug reports, and they measure source localization, not patch
correctness. A causal code-quality claim needs a separate agent task benchmark
with blinded patch grading, test outcomes, and memory-on versus memory-off
conditions.

## Reproduction

```powershell
cargo build --release -p synaptic

target\release\synaptic.exe eval scale `
  --manifest eval\tool-source-scale-2026-07-30.toml `
  --reps 3 `
  --cache synaptic-out\bench-tools-2026-07-30 `
  --out synaptic-out\eval\tool-source-scale-2026-07-30

powershell -NoProfile -ExecutionPolicy Bypass `
  -File scripts\benchmark-tool-source-memory.ps1
```

On Windows, enable Git long-path support before the initial Sourcegraph
checkout. Restore the prior Git setting afterward if it was changed only for
this run.
