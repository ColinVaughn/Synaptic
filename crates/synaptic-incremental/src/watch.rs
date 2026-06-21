//! Filesystem-watch support: the pure debounce/filter cores behind `synaptic
//! watch`. The OS event loop (`notify`) lives in the CLI; this module holds the
//! testable logic — which paths to ignore (so the watcher never rebuilds in
//! response to its own output) and how changed paths are batched and deduped
//! between debounced rebuilds.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// Debounce window (ms) for batching a burst of saves into one rebuild — a
/// ~3-second settle.
pub const DEBOUNCE_MS: u64 = 3000;

/// Directory names whose subtrees must never trigger a rebuild. Critically this
/// includes the output dir: writing `graph.json` there would otherwise fire a
/// change event and loop forever. The rest are VCS / build / dependency caches
/// that only generate noise.
const IGNORED_DIRS: &[&str] = &[
    "synaptic-out",
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
];

/// True if `path` lies inside an ignored subtree (the watcher must skip it).
pub fn should_ignore_path(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(c, Component::Normal(os) if IGNORED_DIRS.iter().any(|d| os.eq_ignore_ascii_case(d)))
    })
}

/// Accumulates changed paths between debounced rebuilds. Deduplicates, drops
/// ignored subtrees on insert, and drains in sorted order for deterministic
/// rebuild input.
#[derive(Debug, Default)]
pub struct ChangeBatch {
    pending: BTreeSet<PathBuf>,
}

impl ChangeBatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a changed path. Returns `true` if it was newly recorded; ignored
    /// subtrees and duplicates return `false`.
    pub fn record(&mut self, path: PathBuf) -> bool {
        if should_ignore_path(&path) {
            return false;
        }
        self.pending.insert(path)
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Drain the accumulated paths (sorted, deduped), leaving the batch empty.
    pub fn take(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.pending).into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_output_vcs_and_build_subtrees() {
        for p in [
            "synaptic-out/graph.json",
            "proj/synaptic-out/cache/manifest.json",
            ".git/index",
            "a/.git/HEAD",
            "target/debug/foo",
            "node_modules/x/y.js",
            "__pycache__/m.pyc",
        ] {
            assert!(should_ignore_path(Path::new(p)), "should ignore {p}");
        }
    }

    #[test]
    fn does_not_ignore_real_sources() {
        for p in ["src/main.rs", "a/b/foo.py", "lib.ts", "docs/notes.md"] {
            assert!(!should_ignore_path(Path::new(p)), "should watch {p}");
        }
    }

    #[test]
    fn batch_dedups_and_skips_ignored() {
        let mut b = ChangeBatch::new();
        assert!(b.record(PathBuf::from("src/a.rs")));
        assert!(
            !b.record(PathBuf::from("src/a.rs")),
            "duplicate not re-added"
        );
        assert!(b.record(PathBuf::from("src/b.rs")));
        assert!(
            !b.record(PathBuf::from("synaptic-out/graph.json")),
            "ignored path not recorded (no self-trigger)"
        );
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn take_drains_sorted_and_empties() {
        let mut b = ChangeBatch::new();
        b.record(PathBuf::from("z.rs"));
        b.record(PathBuf::from("a.rs"));
        let drained = b.take();
        assert_eq!(drained, vec![PathBuf::from("a.rs"), PathBuf::from("z.rs")]);
        assert!(b.is_empty(), "batch emptied after take");
    }
}
