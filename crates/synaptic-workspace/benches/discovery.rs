//! Criterion benchmark for `discover_members` — the monorepo member-discovery
//! walk. Exists on both `main` and the federation branch, so it is the A/B target
//! for "did discovery degrade from main" (the branch adds per-member manifest
//! validation, Cargo `exclude`, and nested-workspace recursion that calls a full
//! detector pass per member).
//!
//! Run: `cargo bench -p synaptic-workspace --bench discovery`

use std::path::Path;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use synaptic_workspace::discover::discover_members;

fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// A Cargo monorepo with `n` crate members under `crates/`.
fn make_monorepo(root: &Path, n: usize) {
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\n",
    );
    for i in 0..n {
        write(
            root,
            &format!("crates/c{i}/Cargo.toml"),
            &format!("[package]\nname = \"c{i}\"\n"),
        );
        write(root, &format!("crates/c{i}/src/lib.rs"), "pub fn f() {}\n");
    }
}

fn bench_discover_members(c: &mut Criterion) {
    let mut group = c.benchmark_group("discovery/discover_members");
    group.sample_size(20);
    for &n in &[50usize, 200, 1000] {
        let dir = tempfile::tempdir().unwrap();
        make_monorepo(dir.path(), n);
        let root = dir.path().to_path_buf();
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| black_box(discover_members(black_box(&root))));
        });
        // `dir` (TempDir) is dropped at loop end, after the bench for this `n`.
    }
    group.finish();
}

criterion_group!(benches, bench_discover_members);
criterion_main!(benches);
