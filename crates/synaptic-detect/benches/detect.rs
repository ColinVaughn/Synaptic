//! Criterion benchmarks for `synaptic-detect` — the file-discovery walk that
//! runs on every `extract`.
//!
//!   * `detect/full` — `detect()` over the workspace `crates/` dir (real corpus).
//!   * `detect/follow_links` — A/B of the `WalkBuilder` with `follow_links` on
//!     vs off over the same dir, isolating the cost of the symlink-follow change.
//!     The classification step is identical between the two, so the delta is the
//!     flag's overhead alone.
//!
//! Run: `cargo bench -p synaptic-detect`

use std::path::{Path, PathBuf};

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use ignore::WalkBuilder;
use synaptic_detect::{detect, noise::is_noise_dir};

/// The workspace `crates/` dir (this crate is `crates/synaptic-detect`).
fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .to_path_buf()
}

/// Count files under `root` using detect's walker config, with `follow_links`
/// toggled — isolates the flag's overhead from the (identical) classify step.
fn walk_count(root: &Path, follow: bool) -> usize {
    let guard = root.to_path_buf();
    WalkBuilder::new(root)
        .hidden(false)
        .parents(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .follow_links(follow)
        .add_custom_ignore_filename(".synapticignore")
        .filter_entry(move |entry| {
            if entry.path_is_symlink() {
                if let Ok(real) = entry.path().canonicalize() {
                    if !real.starts_with(&guard) {
                        return false;
                    }
                }
            }
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                let name = entry.file_name().to_string_lossy();
                let parent = entry.path().parent().unwrap_or_else(|| Path::new(""));
                return !is_noise_dir(&name, parent);
            }
            true
        })
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .count()
}

fn bench_detect(c: &mut Criterion) {
    let dir = crates_dir();

    let mut group = c.benchmark_group("detect");
    group.sample_size(20);

    group.bench_function("full", |b| b.iter(|| black_box(detect(black_box(&dir)))));
    group.bench_function("follow_links/on", |b| {
        b.iter(|| black_box(walk_count(black_box(&dir), true)))
    });
    group.bench_function("follow_links/off", |b| {
        b.iter(|| black_box(walk_count(black_box(&dir), false)))
    });

    group.finish();
}

criterion_group!(benches, bench_detect);
criterion_main!(benches);
