//! Criterion benchmark for `discover_sibling_repos` — the new multi-repo on-disk
//! scan (`.git`-boundary-pruned, depth-bounded). Branch-only (no `main`
//! equivalent), so this measures absolute scaling: it must stay linear in the
//! number of sibling repos and must NOT descend into a repo (the boundary prune).
//!
//! Run: `cargo bench -p synaptic-workspace --bench sibling_scan`

use std::path::Path;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use synaptic_workspace::scan::{discover_sibling_repos, ScanOptions};

fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// A parent dir holding `n` sibling git repos, each a tiny crate with `.git/` and
/// a deep internal tree (to prove the scan does NOT descend past the `.git`).
fn make_siblings(parent: &Path, n: usize) {
    for i in 0..n {
        let repo = format!("repo{i}");
        write(
            parent,
            &format!("{repo}/.git/HEAD"),
            "ref: refs/heads/main\n",
        );
        write(
            parent,
            &format!("{repo}/Cargo.toml"),
            &format!("[package]\nname = \"r{i}\"\n"),
        );
        // Internal depth the boundary-prune must skip (never recursed into).
        write(
            parent,
            &format!("{repo}/src/a/b/c/deep.rs"),
            "pub fn f() {}\n",
        );
    }
}

fn bench_sibling_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("discovery/sibling_scan");
    group.sample_size(20);
    for &n in &[50usize, 200] {
        let dir = tempfile::tempdir().unwrap();
        make_siblings(dir.path(), n);
        let root = dir.path().to_path_buf();
        let opts = ScanOptions::default();
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| black_box(discover_sibling_repos(black_box(&root), &opts, None)));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_sibling_scan);
criterion_main!(benches);
