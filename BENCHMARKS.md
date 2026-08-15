# Synaptic benchmarks

Synaptic's claims are backed by reproducible benchmarks rather than assertion. There are
four families:

1. **Token economy** — how much smaller a graph query is than reading source (see the README).
2. **Accuracy** — extraction correctness against a hand-labeled corpus (this document).
3. **Scale** — extraction throughput across repository sizes and language families.
4. **Extraction quality at scale** — correctness on 60 real repositories covering every
   shipped language, measured without hand labels and gated against pinned baselines.

All accuracy numbers are exact set-comparison against human-verified labels; nothing here is
estimated or self-reported by the tool.

## Accuracy corpus

Location: `crates/synaptic-eval/corpus/`. Each fixture is a small, hand-written, parseable
source fixture (not a full buildable project) plus a `ground_truth.toml` that encodes only what
a human verified by reading the code. A top-level `manifest.toml` lists the fixtures and groups
them by language family. A preflight resolves every labeled symbol before any metric is
computed and fails the run if any label does not resolve, so a dropped node cannot silently
shrink a denominator (or let a malformed fixture become a misleading oracle).

Run it:

```sh
synaptic eval corpus            # markdown table to stdout + report.json/md
synaptic eval corpus --json     # machine-readable
```

### Ground-truth format

```toml
[[call_edge]]                    # every TRUE caller -> callee (the oracle)
from = "src/lib.rs::handle_request"
to   = "src/router.rs::route"

[[test_link]]                    # a test and the code it covers
test = "test_router.py::test_route"
covers = ["router.py::route"]

[[blast]]                        # a seed change and its TRUE transitive set
seed = "router.py::route"
affects = ["app.py::handle_request", "test_router.py::test_route"]

[[cross_edge]]                   # a cross-language coupling (client -> server/native)
from = "web/src/api.ts::createSession"
to   = "src/routes.rs::create_session"
```

Labels are written as `relative/path::symbol`. The resolver maps each to the node the
extractor produced (matching on source file and bare symbol name), so labels stay readable
while scoring runs against real node ids.

### Metrics

- **Call-edge precision / recall / F1** — extracted `calls` edges vs. the labeled call set.
  The oracle includes cross-file calls the extractor is *not* designed to resolve, and an
  unresolved labeled endpoint counts as a false negative (not a skipped sample), so recall
  reflects the real call graph rather than a self-fulfilling subset.
- **Affected-test recall and precision** — `test_link` labels (a test that MUST be selected when
  a covered symbol changes) give recall; `test_nonlink` labels (a test that must NOT be
  selected) give precision, so recall cannot be bought by selecting every test.
- **Blast-radius recall, distractor exclusion, and set size** — `blast.affects` gives recall;
  `blast.not_affected` distractors that leak into the reverse-impact set are precision failures;
  the reported set size vs. the true affected size shows whether the walk is over-broad. (A
  blast that returns the whole graph would have perfect recall but leak every distractor.)
- **Cross-language precision / recall** — `cross_edge` couplings that MUST connect give recall;
  `cross_nonedge` distractors (look-alike path, method/handler mismatch, client call with no
  server) that DO connect are precision failures. Connection = forward reachability over the
  cross-language relations (client `calls_service` into a path-keyed route node `handled_by` the
  server handler).

Reverse-impact uses the same relation vocabulary (`DEFAULT_AFFECTED_RELATIONS`) a consumer of
the affected/predict tools sees, so the benchmark measures real reachability. A preflight
resolves every labeled symbol first and fails the run if any does not resolve.

### Current results (11 fixtures, 6 language families, 42 labeled symbols)

| Fixture | Family | Call P/R/F1 | Aff-test rec | Blast rec/excl/size | Cross P/R/F1 |
|---|---|---|---|---|---|
| systems-rust | systems-rust | 100/50/66 | — | 100%/100%/1.0 | — |
| scripting-python | scripting-python | 100/100/100 | 100% | 100%/100%/2.0 | — |
| web-ts | web-ts | 100/100/100 | — | 100%/100%/1.0 | — |
| oo-java | oo-java | 100/100/100 | — | 100%/100%/1.0 | — |
| systems-go | systems-go | 100/100/100 | — | 100%/100%/1.0 | — |
| deep-python (multi-hop) | scripting-python | 100/100/100 | 100% | 100%/100%/3.0 | — |
| cross-lang-ts-rust | cross-lang | — | — | — | 100/100/100 |
| cross-lang-grpc | cross-lang | — | — | — | 100/100/100 |
| cross-lang-queue | cross-lang | 100/100/100 | — | — | 100/100/100 |
| cross-lang-pyo3 | cross-lang | 100/100/100 | — | — | 100/100/100 |
| cross-lang-ws | cross-lang | 100/100/100 | — | — | 100/100/100 |

Pooled: call edges precision 100% / recall 94% / F1 97% over 18 labeled edges; blast recall
100% with 0 distractors leaked; affected-test recall 100% with the labeled unrelated test
excluded; cross-language precision 100% / recall 100% over 6 labeled couplings with 6
distractors correctly unconnected.

`—` marks a metric a fixture does not label. The harness prints `n/a` for these rather than a
vacuous 100%, so an empty label set is never mistaken for a perfect score.

A regression test (`per_fixture_baselines_hold`) pins each fixture's measured call P/R, blast
recall, blast distractor-exclusion, and the cross-language / multi-hop test assertions, so an
extraction regression fails CI; when extraction *improves* (e.g. Rust gains cross-file call
resolution), the affected baseline is updated upward deliberately.

### Limitations

- The corpus is small and hand-labeled: it validates correctness on representative shapes, not
  coverage at internet scale. Scale is measured separately (below).
- The Rust fixture's 50% call recall is the intra-file resolution limit, surfaced rather than
  hidden; cross-file *reachability* is still preserved via `imports` edges (blast recall 100%).
- Per-fixture call precision is reported and gated only via the pinned baseline; on tiny
  fixtures one unlabeled-but-real edge would swing the ratio, so the guard pins the measured
  value rather than asserting a universal 100%.

## Prediction calibration

The forecast layer attaches a confidence to each predicted co-change. Calibration asks whether
that confidence is honest: do the things it calls "70% likely" happen ~70% of the time?

Run it:

```sh
synaptic eval calibrate --max-commits 200    # reliability table + Brier score
synaptic eval calibrate --json
```

### Method

For each of the most recent `--max-commits` commits (oldest-first overall), the harness:

1. uses EACH file the commit touched as a seed in turn (no single-filename bias);
2. asks the co-change predictor, trained ONLY on commits preceding this one, which other files
   should change with the seed (each suggestion carries a confidence);
3. records a sample `(confidence, hit)` where `hit` is whether that file actually changed in
   the commit.

It then bins the samples into `--bins` equal-width confidence buckets and reports, per bucket,
the mean predicted confidence vs. the observed hit rate (the **reliability table**), plus:

- **Brier score** = mean of `(confidence - outcome)^2` (0 perfect, 1 worst);
- **base rate** = overall observed hit rate;
- **Brier skill score** = `1 - brier / brier_baseline`, where the baseline always predicts the
  base rate (`brier_baseline = base_rate * (1 - base_rate)`). Positive means better than
  guessing; `<= 0` means no better than the base rate. This makes the raw Brier interpretable.
- **expected calibration error (ECE)** = count-weighted mean gap between each bin's mean
  confidence and its observed hit rate.

The scoring core (`samples_from_history`, `reliability`) is pure and unit-tested; only history
extraction touches git.

### Interpreting it

Calibration is a **per-repo** property: confidence is derived from that repo's own co-change
history, so the number reflects the repo's commit granularity. Measured on Synaptic's own
(squash-heavy, synthetic) history the Brier skill score is **negative** — the co-change
predictor is *worse than always guessing the base rate* — because squashed commits touch many
unrelated files together and inflate apparent co-change. That is not a flattering number, and it
is the point: the skill score and ECE refuse to dress up a predictor that is miscalibrated on
this history. Run it on a repo with normal commit granularity for a representative result; the
baseline makes the Brier comparable across repos.

## Scale

Extraction throughput across pinned external repositories spanning size tiers and language
families. Manifest: `crates/synaptic-eval/scale-corpus.toml` (repo URL + full SHA + family +
tier). Network + git required; opt-in (never run in CI).

Run it:

```sh
synaptic eval scale                 # clone each pinned repo, time cold + warm builds
synaptic eval scale --tier small    # restrict to a tier
synaptic eval scale --json
```

### Method

For each repo the harness clones at the pinned SHA into a cache dir (`--filter=blob:none` to
keep the transfer small), times a **cold** build and then a **warm** build (AST cache hot), and
records the pinned SHA, raw timing samples, files, LOC, graph nodes/edges, both summary
timings, and warm LOC/sec. The incremental sample re-extracts the manifest's named,
primary-language **unchanged** file; it measures incremental-path overhead, not patch latency.
The repository fails validation unless exactly one file is re-extracted without a read fallback
and the resulting topology is unchanged.
A repo that cannot be cloned or built is retained in the report and makes the command fail
unless `--allow-skips` is passed for an explicitly partial exploratory run.

### Results (pinned 2026-08-12; Windows / x86_64 / 16 logical CPUs; median of 5 reps)

Run from Synaptic 0.9.10 source `a679e4800e7facb07d685cd8ebd2709d7a99f966`
with a dirty working tree containing these benchmark changes. The dirty flag is part of the
report, so this is development evidence rather than a clean release baseline. All ten
repositories completed; none were skipped. Exact SHAs, selected incremental files, and raw
samples: [`eval/scale-results-2026-08-12.json`](eval/scale-results-2026-08-12.json).

| Repo | Family | Files | LOC | Nodes | Edges | Cold med/max (s) | Warm med/max (s) | Unchanged-file incr (s) | Warm LOC/s |
|---|---|--:|--:|--:|--:|--:|--:|--:|--:|
| memchr | systems-rust | 69 | 17,855 | 1,082 | 2,265 | 0.12/0.15 | 0.07/0.08 | 0.064 | 257,486 |
| click | scripting-python | 113 | 35,080 | 3,780 | 5,881 | 0.26/0.34 | 0.15/0.21 | 0.137 | 229,636 |
| p-map | web-ts | 10 | 1,501 | 108 | 120 | 0.05/0.06 | 0.03/0.03 | 0.034 | 43,796 |
| cobra | go | 55 | 19,514 | 923 | 2,451 | 0.13/0.14 | 0.06/0.07 | 0.055 | 305,344 |
| axum | systems-rust | 350 | 52,990 | 5,842 | 11,705 | 0.58/0.66 | 0.35/0.38 | 0.252 | 153,305 |
| gson | jvm-java | 287 | 58,882 | 7,593 | 20,165 | 1.03/1.05 | 0.49/0.54 | 0.462 | 119,189 |
| fmt | systems-cpp | 100 | 80,762 | 3,938 | 8,174 | 0.65/0.69 | 0.24/0.26 | 0.160 | 338,904 |
| Humanizer | dotnet-csharp | 2,563 | 476,967 | 44,548 | 54,985 | 7.07/8.43 | 2.69/2.83 | 1.889 | 177,035 |
| rack | scripting-ruby | 106 | 23,181 | 1,531 | 2,020 | 0.20/0.25 | 0.10/0.11 | 0.074 | 221,814 |
| Slim | web-php | 135 | 17,196 | 2,092 | 4,085 | 0.39/0.45 | 0.14/0.15 | 0.118 | 124,612 |
| **Total represented** | **9 families** | **3,788** | **783,928** | **71,437** | **111,851** | — | — | — | — |

Notes on reading these:

- Absolute times are machine-dependent. Median cold-to-warm speedup ranged from 1.4x to 2.8x
  in this run; that is evidence for these pinned inputs on this host, not a universal claim.
- The checkout and operating-system file cache were already warm. "Cold" means Synaptic's AST
  cache was deleted; it does not mean a cold disk, a fresh clone, or a newly started machine.
- Scale measures extraction completion and throughput, not graph correctness or superiority to
  another tool. The hand-labeled accuracy corpus above is a separate, much smaller evaluation.
- `Files` counts distinct source files that produced graph nodes (not every file on disk);
  `LOC` sums lines across those files.
- `Incr` re-extracts one unchanged file against the prior graph and still re-runs graph
  assembly. It measures incremental-path overhead, not the latency of applying a real edit.
- Below 20 repetitions, the tail column is labeled as the observed maximum; at 20 or more it
  is nearest-rank p95. Raw samples remain in `report.json` either way.
- The harness records skipped repos in the report and warns prominently; a published run with
  skips is partial by construction. Refresh the pinned SHAs deliberately.

## Extraction quality at scale

Scale (above) measures how *fast* extraction runs. A graph that anchored every declaration to
the wrong line would post identical timings. This measures whether the graph is **right**, on
54 pinned real-world repositories covering every language Synaptic ships, using properties
that need no hand labels.

It exists because the accuracy corpus, while exact, is 11 hand-written fixtures and 42 labeled
symbols — and because a 2026-08-13 audit found 1,475 of 31,732 anchors wrong across a set of
real repositories. That audit was ad hoc: nothing in the tree encoded its corpus or its checks,
so the same class of defect could return unnoticed. This makes it repeatable and gated.

Manifest: `crates/synaptic-eval/repo-corpus.toml` (shared with the scale suite via a `suites`
tag, so a repository is added in one place). Baselines: `crates/synaptic-eval/quality-baselines.toml`.
Network + git required; opt-in, never run in CI.

```sh
synaptic eval quality                      # measure, then gate against baselines
synaptic eval quality --language pascal    # one language
synaptic eval quality --repo axum          # one repository
synaptic eval quality --update-baselines   # ratchet bounds (refuses to loosen)
synaptic eval quality --pin                # re-resolve every URL's HEAD
```

### What is measured

**Anchor exactness.** For every node carrying a line, read the source at that line and confirm
the declaration is really there. Anchors resolve three ways: the name is *on the line*; the line
is the declaration's true start — its annotation/attribute block, its docstring opener, or the
first line of a signature wrapped across lines — and the name follows *within* it; or the anchor
does not reach the declaration at all. The middle case is counted correct but reported in its own
column, because an annotated declaration's syntax node genuinely starts at its first annotation,
and folding that in silently would overstate precision. A blank line always ends the walk: an
anchor landing on a blank line is the signature of the `^\s*` regex bug that cost Pascal 58% of
its anchors, and admitting blanks would hide the defect this metric exists to catch.

**Parse and recovery health.** Per language: the share of files whose grammar errored, the share
that produced nothing but their own file node (a silent hole, indistinguishable in the graph from
a file that genuinely declares nothing), and how much the bounded recovery pass rescued.

**Self-consistency.** Two absolute assertions with no baseline, because they have a principled
correct answer: extracting the same SHA twice must produce identical graphs, and a full rebuild
must match an incremental rebuild over touched files. Both hard-fail.

**Independent oracle.** universal-ctags — a completely different, hand-written parser — run over
the same checkout, compared as a *symmetric difference*. Missing ctags skips only this stage,
loudly; the other three still run everywhere.

### Results

60 repositories, 39 languages, 80,061 files, 938,001 nodes. Windows / x86_64 / 16 logical CPUs,
Synaptic 0.9.11 at source revision `8633719899a1` with a dirty working tree (these benchmark
changes), so this is development evidence rather than a clean release baseline. No repository was
skipped. Full per-repository and per-language tables: `synaptic-out/eval/quality/report.md`.

**Pooled anchor exactness: 735,198 / 735,493 = 99.96%.** Of those, 31,810 resolved through a
declaration's leading annotation block, 9,862 are named by their file rather than by any text
inside it, and 238 nodes carried no name to look for (excluded from the ratio rather than scored
either way).

**Self-consistency: 60 / 60 deterministic, 60 / 60 incrementally equivalent.**

Per-language anchor exactness, worst first:

| Language | Anchors ok/checked | Exact | Parse err | Zero-decl |
|---|--:|--:|--:|--:|
| yaml | 984/999 | 98.50% | 0.00% | 35.52% |
| groovy | 1109/1118 | 99.19% | 83.39% | 15.96% |
| php | 3799/3823 | 99.37% | 0.00% | 1.16% |
| cpp | 15421/15517 | 99.38% | 22.20% | 13.19% |
| sql | 1022/1026 | 99.61% | 75.02% | 84.35% |
| c | 20795/20863 | 99.67% | 60.94% | 64.01% |
| csharp | 175017/175092 | 99.96% | 1.70% | 9.67% |
| razor | 9287/9290 | 99.97% | 0.00% | 0.00% |
| python | 29482/29483 | 100.00% | 1.20% | 23.36% |
| the remaining 30 languages | — | 100.00% | — | — |

The residuals are real and are reported rather than tuned away:

- **yaml (98%)** — synthesized composite labels (`Service/1234`) that do not appear verbatim.

### What the benchmark caught: Razor / Blazor

Razor reached the suite through two incidental fixtures -- 20 files and 52 anchors -- and scored
73%. Adding three real component libraries (MudBlazor, ant-design-blazor, fluentui-blazor) took
Razor to **4,505 files and 9,290 anchors** and exposed that the score was the smaller problem:

| Defect | Effect | Evidence |
|---|---|---|
| No `@code` block meant no delegation | The file produced no node at all, not even a file node | **735 of 1,999 MudBlazor files (36.8%) invisible to every query** |
| `@code` holding an inline Razor template | Same, for valid Razor that is not valid C# (`@<div>…</div>` returning a `RenderFragment`) | MudChart, MudColorPicker, MudTimePicker absent |
| Component anchored at its `@code` block | "Go to definition" landed in the middle of the markup | **0 of 1,261 components at line 1** |
| Directives never read | `@inherits`, `@implements`, `@inject` live outside `@code`, so the base class, interfaces and injected services were all missing | 12.4% / 1.2% / 9.7% of a 4,023-file corpus |
| Two `@code` blocks in one file | The class node is emitted twice and only the first was re-anchored | four `App.razor` template files |

A Razor component is declared by its *file*, not by any text inside it, so it is now anchored at
line 1 and emitted whether or not the file has a `@code` block and whether or not that block is
anything the C# grammar can read. Directives became **1,383 edges** (499 `inherits`, 832 `uses`,
52 `implements`) across the three libraries -- a 100% capture of the directives present.

Measured effect: invisible files **36.8% -> 0%**, and Razor anchor exactness **73.08% -> 99.97%**
over 179x more anchors.

Two of these were bugs in the *fix* rather than the extractor, both caught by re-measuring rather
than by assuming: re-anchoring only the first of several duplicate class nodes, and rewriting the
`source_location` string while leaving the typed `span` -- which consumers read first, so the
components were still reported at their `@code` line. The tests now assert both fields.

### What the benchmark caught: SQL

SQL was the corpus's blind spot rather than a passing grade. It reached the suite only through
incidental seed scripts — **24 files and 7 checkable anchors across the whole corpus**, which is
not a measurement. Adding three real SQL repositories (`sqlfluff` for warehouse dialects,
`chinook-database` for canonical DDL, `dbt-utils` for Jinja-templated models) took SQL to
**2,722 files and 1,026 anchors**, and immediately surfaced five defects, each now fixed and
covered by a test:

| Defect | Effect | Evidence |
|---|---|---|
| Regex recovery hard-coded line 1 | Every recovered procedure, trigger and table pointed at the file header | A procedure declared on line 5 reported `L1` |
| dbt Jinja defeated the grammar | Every dbt model yielded zero declarations and no lineage | jaffle_shop: 5/5 files parse-error, 0 declarations |
| `CREATE` modifiers unrecognized | `MATERIALIZED VIEW` / `EXTERNAL TABLE` dropped the object entirely | 112 of 125 missing declarations on the dialect corpus |
| One `CREATE` per `;`-chunk | Files omitting semicolons lost every statement after the first | — |
| Comments scanned as code | A comment mentioning DDL invented a table | 121 comment lines across the dialect corpus |

Measured effect: true DDL miss rate **7.6% → 1.7%**, declarations 737 → 917 on the dialect
corpus, and jaffle_shop from 0 declarations to 5 models with 8 `reads_from` lineage edges.
SQL anchor exactness is now **99.61%** over 1,026 anchors.

A sixth defect was caught by the SQL fuzz corpus rather than by the benchmark: Jinja
neutralization replaced each *character* with one space, so a multi-byte character shortened the
file and shifted every line below it. The invariant (byte length and newline count preserved) is
now asserted directly.

Two limitations stay visible in the table above. SQL's `parse err` (75.0%) and `zero-decl` (84.4%)
rates are high because `tree-sitter-sequel` does not cover warehouse dialects — a bare `commit`
or a `select top 1` defeats it. That is reported rather than hidden, and it is measured as
costing only 1.7% of declarations, because the regex recovery pass catches CREATE objects
regardless of parse state. Replacing the grammar would move a health signal, not the graph.

High `parse err` and `zero-decl` rates are honest coverage signals, not anchor defects: Fortran
(88.87%), Groovy (83.39%), Verilog (68.90%) and C (60.94%) defeat their grammars often, which is
why the recovery pass exists — it contributed 1,361 Groovy, 548 Verilog and 49 PowerShell
declarations in this run, every one of them anchor-exact.

### The oracle, read correctly

Across 27 languages ctags could parse: **321,944 declarations found by both**, 570,314 found only
by ctags, 55,727 found only by Synaptic.

The large ctags-only column is a **granularity difference, not a recall deficit**, which is
exactly why this is published as a symmetric difference and never as a recall percentage. ctags
emits a tag for every JSON key (278,983 of the ctags-only total is JSON alone), every struct
member and every macro; Synaptic models the declarations a dependency graph needs and adds
structure ctags has no concept of — cross-file edges, framework routes, and 32,059 Markdown
headings ctags never emits. Neither tool is ground truth. What is actionable is the asymmetry
per language, which is why the report breaks it out that way.

### Gating

Each repository carries pinned bounds: `anchor_exactness_min`, `parse_error_rate_max`,
`zero_decl_file_rate_max`, `ctags_missed_rate_max`. A run that breaches one exits non-zero naming
the repository, metric and delta. `--update-baselines` tightens freely but **refuses to loosen a
bound** without `--allow-regression`, so a regression has to be an explicit decision rather than a
side effect of re-running the benchmark. Bounds round outward to four places so a rerun of the
same pin cannot fail on float noise.

A test (`every_extractor_is_benchmarked`) fails when a shipped extractor appears in no manifest
entry, so a new language cannot ship without a real repository behind it. The corpus this replaced
reached 9 of 39 languages; the remaining 30 extractors had never been checked against code anyone
wrote, which is how a case-sensitive extension match that routed `.F90` to no extractor at all
survived to be found by hand.

### Limitations

- Anchor exactness asks whether the recorded line contains the declaration. It does not check
  that the *right* declaration was found, that edges are correct, or that nothing was missed —
  the hand-labeled corpus and the oracle diff cover those, from different directions.
- ctags does not know every language Synaptic does (Apex, QL, Razor); those report the other
  three measurements and no oracle number, rather than a fabricated zero.
- The corpus is pinned. A moved SHA changes the numbers; refresh with `--pin` deliberately.
- Absolute file and node counts are machine-independent, but the run is opt-in and single-host.
