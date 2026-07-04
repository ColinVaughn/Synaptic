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

/// True for files whose change warrants an AST rebuild: code (any language
/// Synaptic classifies as Code) plus extractable markdown. Shared by `watch`
/// and `serve --watch` so both react to exactly the graph's input set.
pub fn is_rebuildable(path: &Path) -> bool {
    synaptic_detect::classify_file(path) == Some(synaptic_detect::FileType::Code)
        || crate::freshen::is_extractable_markdown(path)
}

/// True if `path` lies inside an ignored subtree (the watcher must skip it).
/// Delegates to detect's noise rules so the watcher skips exactly what the
/// walker skips -- critically including `synaptic-out` (writing `graph.json`
/// would otherwise fire a change event and loop forever). Also including
/// `.git`: post-commit bookkeeping must not look like source changes. Pass
/// repo-RELATIVE paths: a noise name in an ancestor of the repo root (e.g. a
/// checkout under `/build/app`) must not ignore the whole tree.
pub fn should_ignore_path(path: &Path) -> bool {
    let mut parent = PathBuf::new();
    for c in path.components() {
        if let Component::Normal(os) = c {
            let name = os.to_string_lossy().to_ascii_lowercase();
            if synaptic_detect::noise::is_noise_dir(&name, &parent) {
                return true;
            }
        }
        parent.push(c);
    }
    false
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
            // The watcher must skip exactly what detect's walker skips, so a
            // save in any noise dir can never trigger a pointless rebuild.
            "dist/bundle.js",
            ".next/server/page.js",
            "coverage/lcov.info",
            "app/.nuxt/x.ts",
            "build/gen.py",
        ] {
            assert!(should_ignore_path(Path::new(p)), "should ignore {p}");
        }
    }

    #[test]
    fn rebuildable_is_code_or_extractable_markdown() {
        for p in ["src/main.rs", "a/b.py", "docs/notes.md", "x.mdx", "q.qmd"] {
            assert!(is_rebuildable(Path::new(p)), "should rebuild for {p}");
        }
        for p in ["notes.txt", "data.bin", "img.png"] {
            assert!(!is_rebuildable(Path::new(p)), "should not rebuild for {p}");
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
