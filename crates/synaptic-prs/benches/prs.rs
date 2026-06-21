//! Criterion benchmarks for `synaptic-prs`.
//!
//! Headline measurement is **H5**: a triage run computes blast radius for up to
//! ~50 PRs, and the pre-H5 code rebuilt the whole `source_file → impact` index
//! (a full graph-node scan + per-file `String` allocs) for *every* PR. The two
//! groups below process the same batch of PR file-sets both ways — rebuilding
//! the index per PR vs building it once and reusing it — so the delta is the
//! redundant index-build work that hoisting removed.
//!
//! Run: `cargo bench -p synaptic-prs`

use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use synaptic_prs::{compute_pr_impact, path_match, ImpactIndex};

const NODE_SCALES: [usize; 2] = [1_000, 10_000];
/// PRs per triage batch (the CLI/server `--limit` default).
const PRS: usize = 50;
/// Changed files per PR.
const FILES_PER_PR: usize = 20;

/// `n` graph nodes as `(source_file, community)`, spread over many files.
fn synthetic_nodes(n: usize) -> Vec<(String, Option<u32>)> {
    (0..n)
        .map(|i| {
            (
                format!("src/mod_{}/file_{}.rs", i % 32, i % 512),
                Some((i % 16) as u32),
            )
        })
        .collect()
}

/// `PRS` PR diffs, each `FILES_PER_PR` changed files (some hit the graph, some not).
fn synthetic_prs() -> Vec<Vec<String>> {
    (0..PRS)
        .map(|p| {
            (0..FILES_PER_PR)
                .map(|f| format!("src/mod_{}/file_{}.rs", (p + f) % 32, (p * 7 + f) % 512))
                .collect()
        })
        .collect()
}

fn bench_impact_index_reuse(c: &mut Criterion) {
    let mut group = c.benchmark_group("prs/impact_index");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    let prs = synthetic_prs();
    for &n in &NODE_SCALES {
        let nodes = synthetic_nodes(n);

        // OLD (pre-H5): rebuild the index for every PR.
        group.bench_with_input(BenchmarkId::new("per_pr_rebuild", n), &n, |b, _| {
            b.iter(|| {
                let mut acc = 0usize;
                for files in &prs {
                    let (_c, nodes_affected) =
                        compute_pr_impact(nodes.iter().map(|(s, c)| (s.as_str(), *c)), files);
                    acc += nodes_affected;
                }
                black_box(acc)
            });
        });

        // NEW (H5): build the index once, reuse it for every PR.
        group.bench_with_input(BenchmarkId::new("reused_index", n), &n, |b, _| {
            b.iter(|| {
                let index = ImpactIndex::build(nodes.iter().map(|(s, c)| (s.as_str(), *c)));
                let mut acc = 0usize;
                for files in &prs {
                    let (_c, nodes_affected) = index.impact_for_files(files);
                    acc += nodes_affected;
                }
                black_box(acc)
            });
        });
    }
    group.finish();
}

/// M6: `path_match` is on the inner `files × source_files` loop. Confirm the
/// allocation-free boundary check isn't slower than the old `format!` version
/// (it does the same work without the two per-call heap allocations).
fn bench_path_match(c: &mut Criterion) {
    let mut group = c.benchmark_group("prs/path_match");
    group.sample_size(50);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(2));

    let pairs = [
        ("src/auth/api.py", "api.py"),
        ("api.py", "pkg/api.py"),
        ("config.py", "g.py"),
        ("src/auth/api.py", "src/auth/api.py"),
    ];
    group.bench_function("mixed_pairs", |b| {
        b.iter(|| {
            let mut hits = 0usize;
            for _ in 0..1000 {
                for (a, c) in &pairs {
                    if path_match(black_box(a), black_box(c)) {
                        hits += 1;
                    }
                }
            }
            black_box(hits)
        });
    });
    group.finish();
}

criterion_group!(benches, bench_impact_index_reuse, bench_path_match);
criterion_main!(benches);
