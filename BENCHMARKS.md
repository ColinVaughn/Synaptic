# CodeGraph benchmarks

CodeGraph's claims are backed by reproducible benchmarks rather than assertion. There are
three families:

1. **Token economy** — how much smaller a graph query is than reading source (see the README).
2. **Accuracy** — extraction correctness against a hand-labeled corpus (this document).
3. **Scale** — extraction throughput across repository sizes and language families.

All accuracy numbers are exact set-comparison against human-verified labels; nothing here is
estimated or self-reported by the tool.

## Accuracy corpus

Location: `crates/codegraph-eval/corpus/`. Each fixture is a small, real, compiling mini-repo
plus a `ground_truth.toml` that encodes only what a human verified by reading the code. A
top-level `manifest.toml` lists the fixtures and groups them by language family.

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
  The oracle includes cross-file calls the extractor is *not* designed to resolve, so recall
  reflects the real call graph, not a self-fulfilling subset.
- **Affected-test recall** — of the tests labeled as covering a symbol, how many CodeGraph's
  reverse-impact surfaces from that symbol.
- **Blast-radius false-negative rate** — for each labeled seed change, the percent of truly
  affected nodes the reverse-impact analysis misses (lower is better).
- **Cross-language accuracy** — whether each labeled client→server/native coupling is
  connected end to end (forward reachability over the cross-language relations: a client
  `calls_service` into a path-keyed route node that is `handled_by` the server handler).

Reverse-impact uses the same relation vocabulary (`DEFAULT_AFFECTED_RELATIONS`) a consumer of
the affected/predict tools sees, so the benchmark measures real reachability.

### Current results (5 fixtures, 5 language families)

| Fixture | Family | Call P/R/F1 | Aff-test recall | Blast FN | Cross P/R/F1 |
|---|---|---|---|---|---|
| systems-rust | systems-rust | 100/50/66 | — | 0% | — |
| scripting-python | scripting-python | 100/100/100 | 100% | 0% | — |
| web-ts | web-ts | 100/100/100 | — | 0% | — |
| oo-java | oo-java | 100/100/100 | — | 0% | — |
| cross-lang-ts-rust | cross-lang | — | — | — | 100/100/100 |

Pooled call edges: precision 100% / recall 88% / F1 93% over 9 labeled edges.

`—` marks a metric a fixture does not label (e.g. a single-language fixture has no
cross-language couplings). The harness prints `n/a` for these rather than a vacuous 100%, so
an empty label set is never mistaken for a perfect score.

A regression test (`per_fixture_baselines_hold`) pins each fixture's measured values so an
extraction regression fails CI; when extraction *improves* (e.g. Rust gains cross-file call
resolution), the affected baseline is updated upward deliberately.

### Limitations

- The corpus is small and hand-labeled: it validates correctness on representative shapes, not
  coverage at internet scale. Scale is measured separately (below).
- Per-fixture precision is reported, not gated, on tiny fixtures where one unlabeled-but-real
  edge would swing the ratio; the regression guard pins the measured value instead.
