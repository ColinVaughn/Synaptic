# CodeGraph benchmarks

CodeGraph's claims are backed by reproducible benchmarks rather than assertion. There are
three families:

1. **Token economy** — how much smaller a graph query is than reading source (see the README).
2. **Accuracy** — extraction correctness against a hand-labeled corpus (this document).
3. **Scale** — extraction throughput across repository sizes and language families.

All accuracy numbers are exact set-comparison against human-verified labels; nothing here is
estimated or self-reported by the tool.

## Accuracy corpus

Location: `crates/codegraph-eval/corpus/`. Each fixture is a small, hand-written, parseable
source fixture (not a full buildable project) plus a `ground_truth.toml` that encodes only what
a human verified by reading the code. A top-level `manifest.toml` lists the fixtures and groups
them by language family. A preflight resolves every labeled symbol before any metric is
computed and fails the run if any label does not resolve, so a dropped node cannot silently
shrink a denominator (or let a malformed fixture become a misleading oracle).

Run it:

```sh
codegraph eval corpus            # markdown table to stdout + report.json/md
codegraph eval corpus --json     # machine-readable
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

### Current results (7 fixtures, 6 language families, 26 labeled symbols)

| Fixture | Family | Call P/R/F1 | Aff-test rec | Blast rec/excl/size | Cross P/R/F1 |
|---|---|---|---|---|---|
| systems-rust | systems-rust | 100/50/66 | — | 100%/100%/1.0 | — |
| scripting-python | scripting-python | 100/100/100 | 100% | 100%/100%/2.0 | — |
| web-ts | web-ts | 100/100/100 | — | 100%/100%/1.0 | — |
| oo-java | oo-java | 100/100/100 | — | 100%/100%/1.0 | — |
| systems-go | systems-go | 100/100/100 | — | 100%/100%/1.0 | — |
| deep-python (multi-hop) | scripting-python | 100/100/100 | 100% | 100%/100%/3.0 | — |
| cross-lang-ts-rust | cross-lang | — | — | — | 100/100/100 |

Pooled: call edges precision 100% / recall 93% / F1 96% over 15 labeled edges; blast recall
100% with 0 distractors leaked; affected-test recall 100% with the labeled unrelated test
excluded; cross-language precision 100% / recall 100% with 2 distractors correctly unconnected.

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
codegraph eval calibrate --max-commits 200    # reliability table + Brier score
codegraph eval calibrate --json
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
history, so the number reflects the repo's commit granularity. Measured on CodeGraph's own
(squash-heavy, synthetic) history the Brier skill score is **negative** — the co-change
predictor is *worse than always guessing the base rate* — because squashed commits touch many
unrelated files together and inflate apparent co-change. That is not a flattering number, and it
is the point: the skill score and ECE refuse to dress up a predictor that is miscalibrated on
this history. Run it on a repo with normal commit granularity for a representative result; the
baseline makes the Brier comparable across repos.

## Scale

Extraction throughput across pinned external repositories spanning size tiers and language
families. Manifest: `crates/codegraph-eval/scale-corpus.toml` (repo URL + full SHA + family +
tier). Network + git required; opt-in (never run in CI).

Run it:

```sh
codegraph eval scale                 # clone each pinned repo, time cold + warm builds
codegraph eval scale --tier small    # restrict to a tier
codegraph eval scale --json
```

### Method

For each repo the harness clones at the pinned SHA into a cache dir (`--filter=blob:none` to
keep the transfer small), times a **cold** build and then a **warm** build (AST cache hot), and
records files, graph nodes/edges, both timings, and warm files/sec. A repo that cannot be
cloned or built is logged to stderr and skipped, never fatal.

### Results (pinned 2026-06-19; Windows / x86_64 / 16 logical CPUs; median of 3 reps)

| Repo | Family | Tier | Files | LOC | Nodes | Edges | Cold (s) | Warm (s) | Incr (s) | Files/s |
|---|---|---|--:|--:|--:|--:|--:|--:|--:|--:|
| memchr | systems-rust | small | 75 | 70,044 | 3,849 | 13,592 | 12.5 | 7.5 | 4.3 | 10 |
| click | scripting-python | medium | 112 | 35,063 | 2,189 | 3,475 | 2.4 | 1.7 | 0.8 | 66 |
| p-map | web-ts | small | 10 | 1,501 | 85 | 83 | 0.07 | 0.04 | 0.04 | 269 |
| cobra | go | medium | 55 | 19,514 | 846 | 2,362 | 1.1 | 0.7 | 0.4 | 82 |
| axum | systems-rust | large | 348 | 52,969 | 3,656 | 9,510 | 4.7 | 3.6 | 3.5 | 97 |

Notes on reading these:

- Absolute times are machine-dependent; the reproducible signals are the **cold→warm ratio**
  (~1.4-2x; the AST cache removes re-parsing) and that throughput tracks repo content rather
  than collapsing on the large tier.
- `Files` counts distinct source files that produced graph nodes (not every file on disk);
  `LOC` sums lines across those files.
- `Incr` re-extracts a single file against the prior graph but still re-runs graph assembly, so
  it is the steady-state edit cost, not a parse-only number.
- The harness records skipped repos in the report and warns prominently; a published run with
  skips is partial by construction. Refresh the pinned SHAs deliberately.
