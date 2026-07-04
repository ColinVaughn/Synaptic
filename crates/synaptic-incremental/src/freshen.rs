//! Build provenance + on-disk change detection for the serve catch-up path.
//!
//! A build (full `extract`, incremental `update`/`watch`, or a server catch-up)
//! persists a [`Manifest`] under `synaptic-out/` recording the mtime + content
//! hash of every file that fed the graph. The serve process later diffs the
//! current on-disk state against that manifest to learn -- cheaply, with the
//! mtime fastpath -- exactly which files an agent added/changed/removed since the
//! graph was built, so it can run a minimal incremental rebuild before answering
//! a query. The detector deliberately enumerates the *same* input set as
//! [`rebuild`](crate::rebuild) (code + extractable markdown) so it never reports a
//! file the rebuild would ignore (which would otherwise churn forever).

use std::path::{Path, PathBuf};

use synaptic_detect::{detect_inputs, DetectResult, FileType, Manifest, ManifestDiff};

/// The build manifest's name under the output dir. Hidden + inside
/// `synaptic-out/`, so the watcher's ignore rules already skip it (no
/// self-trigger), alongside `.rebuild.lock` / `.pending_changes`.
const MANIFEST_FILE: &str = ".manifest.json";

/// Path of the persisted build manifest under `out_dir`.
pub fn manifest_path(out_dir: &Path) -> PathBuf {
    out_dir.join(MANIFEST_FILE)
}

/// True for markdown files that get structural heading extraction, matching
/// [`rebuild`](crate::rebuild)'s markdown selection. Shared so the change
/// detector and the rebuild agree on exactly which markdown feeds the graph.
pub fn is_extractable_markdown(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("md") | Some("mdx") | Some("qmd")
    )
}

/// The files that feed the graph from a detect result: code + extractable
/// markdown, matching `rebuild`'s `extract_set`.
fn inputs_of(det: &DetectResult) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = det.of(FileType::Code).to_vec();
    files.extend(
        det.of(FileType::Document)
            .iter()
            .filter(|p| is_extractable_markdown(p))
            .cloned(),
    );
    files
}

/// The files that feed the graph for `root` (code + extractable markdown).
pub fn graph_input_files(root: &Path) -> Vec<PathBuf> {
    inputs_of(&detect_inputs(root))
}

/// Write a build manifest under `out_dir` reflecting the current on-disk state of
/// the graph-input files. Called after every successful build so the serve
/// catch-up has a baseline to diff against.
pub fn persist_manifest(out_dir: &Path, root: &Path) -> std::io::Result<()> {
    persist_manifest_with(out_dir, &detect_inputs(root))
}

/// Like [`persist_manifest`] but reuses an existing detect result instead of
/// walking the tree again -- for callers (the serve catch-up, `extract`) that
/// already scanned. Builds against the prior manifest with the mtime fastpath,
/// so it only re-hashes files whose mtime moved.
pub fn persist_manifest_with(out_dir: &Path, det: &DetectResult) -> std::io::Result<()> {
    snapshot_manifest(out_dir, det).save(&manifest_path(out_dir))
}

/// Build (without saving) the manifest for the current on-disk state of the
/// graph-input files. A rebuild takes this snapshot BEFORE extracting and saves
/// it only after success: the manifest then never records file state the graph
/// didn't ingest, so an edit landing mid-rebuild stays detectable.
pub fn snapshot_manifest(out_dir: &Path, det: &DetectResult) -> Manifest {
    let prior = Manifest::load(&manifest_path(out_dir));
    let files = inputs_of(det);
    Manifest::build_incremental(files.iter().map(PathBuf::as_path), &det.scan_root, &prior)
}

/// What [`detect_changes`] found on disk since the last build.
#[derive(Debug)]
pub struct ChangeReport {
    /// Added/changed/removed/unchanged file keys (repo-relative, POSIX).
    pub diff: ManifestDiff,
    /// The freshly built manifest, so the caller can persist it without a second
    /// filesystem walk.
    pub current: Manifest,
    /// True when no prior manifest existed and this run only established the
    /// baseline (so the diff is empty by construction, not because nothing
    /// changed).
    pub bootstrapped: bool,
    /// The detect result that produced this report, so the caller can feed it
    /// straight to `rebuild_with_detect` instead of walking the tree again.
    pub det: DetectResult,
}

impl ChangeReport {
    /// No add/change/remove relative to the prior manifest.
    pub fn is_empty(&self) -> bool {
        self.diff.added.is_empty() && self.diff.changed.is_empty() && self.diff.removed.is_empty()
    }

    /// The added + changed + removed paths as a rebuild change set (relative to
    /// the repo root; removed paths no longer exist and are evicted by rebuild).
    pub fn changed_paths(&self) -> Vec<PathBuf> {
        self.diff
            .added
            .iter()
            .chain(&self.diff.changed)
            .chain(&self.diff.removed)
            .map(PathBuf::from)
            .collect()
    }
}

/// Diff the current on-disk graph inputs against the manifest persisted under
/// `out_dir`. Uses the mtime fastpath, so unchanged files are stat-only.
///
/// Bootstrap: when no prior manifest exists (e.g. a graph built by an older
/// binary), this builds and saves the baseline manifest and reports *no changes*
/// -- the loaded graph is assumed to match disk, avoiding a spurious full
/// rebuild on the first query.
pub fn detect_changes(out_dir: &Path, root: &Path) -> ChangeReport {
    let mpath = manifest_path(out_dir);
    let prior_exists = mpath.exists();
    let prior = Manifest::load(&mpath);
    let det = detect_inputs(root);
    let files = inputs_of(&det);
    let root = det.scan_root.as_path();
    let current = if prior_exists {
        Manifest::build_incremental(files.iter().map(PathBuf::as_path), root, &prior)
    } else {
        Manifest::build(files.iter().map(PathBuf::as_path), root)
    };

    if !prior_exists {
        let _ = current.save(&mpath);
        return ChangeReport {
            diff: ManifestDiff::default(),
            current,
            bootstrapped: true,
            det,
        };
    }

    let diff = prior.diff(&current);
    ChangeReport {
        diff,
        current,
        bootstrapped: false,
        det,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    #[test]
    fn graph_inputs_include_code_and_markdown_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/a.py", "x = 1\n");
        write(root, "README.md", "# hi\n");
        write(root, "data.bin", "\x00\x01");
        write(root, "notes.txt", "plain\n");

        let keys: std::collections::BTreeSet<String> = graph_input_files(root)
            .iter()
            .map(|p| {
                p.strip_prefix(root.canonicalize().unwrap())
                    .unwrap_or(p)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert!(keys.contains("src/a.py"), "code included: {keys:?}");
        assert!(keys.contains("README.md"), "markdown included: {keys:?}");
        assert!(!keys.contains("data.bin"), "binary excluded: {keys:?}");
        assert!(!keys.contains("notes.txt"), "plain text excluded: {keys:?}");
    }

    #[test]
    fn detect_changes_bootstraps_when_no_prior_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let out = root.join("synaptic-out");
        write(root, "a.py", "x = 1\n");

        let report = detect_changes(&out, root);
        assert!(report.bootstrapped, "no prior manifest => bootstrap");
        assert!(report.is_empty(), "bootstrap reports no changes");
        assert!(
            manifest_path(&out).exists(),
            "bootstrap writes the baseline manifest"
        );
    }

    #[test]
    fn detect_changes_reports_added_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let out = root.join("synaptic-out");
        write(root, "a.py", "x = 1\n");
        persist_manifest(&out, root).unwrap();

        // Agent writes a brand-new file after the build.
        write(root, "b.py", "y = 2\n");
        let report = detect_changes(&out, root);
        assert!(!report.is_empty(), "new file detected");
        assert_eq!(report.diff.added, vec!["b.py".to_string()]);
        assert_eq!(report.changed_paths(), vec![PathBuf::from("b.py")]);
    }

    #[test]
    fn detect_changes_reports_changed_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let out = root.join("synaptic-out");
        write(root, "a.py", "x = 1\n");
        write(root, "b.py", "y = 2\n");
        persist_manifest(&out, root).unwrap();

        // a.py edited, b.py deleted.
        write(root, "a.py", "x = 99\n");
        fs::remove_file(root.join("b.py")).unwrap();
        let report = detect_changes(&out, root);
        assert_eq!(report.diff.changed, vec!["a.py".to_string()]);
        assert_eq!(report.diff.removed, vec!["b.py".to_string()]);
    }

    #[test]
    fn persisting_current_manifest_stops_redetection() {
        // After a change is detected and the report's fresh manifest is persisted
        // (what the serve catch-up does), the next detect reports nothing -- so a
        // content change never makes the server rebuild the same file forever.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let out = root.join("synaptic-out");
        write(root, "a.py", "x = 1\n");
        persist_manifest(&out, root).unwrap();

        write(root, "b.py", "y = 2\n");
        let report = detect_changes(&out, root);
        assert!(!report.is_empty(), "new file detected");
        report.current.save(&manifest_path(&out)).unwrap();

        let again = detect_changes(&out, root);
        assert!(
            again.is_empty(),
            "re-detection after persisting the manifest must be empty: {:?}",
            again.diff
        );
    }

    #[test]
    fn snapshot_taken_before_an_edit_keeps_the_edit_detectable() {
        // Regression: `update` used to rebuild the manifest by re-walking the
        // disk AFTER the rebuild, so a file edited mid-rebuild was stamped as
        // seen without ever being extracted -- invisible to detect_changes until
        // its next edit. The fix snapshots the manifest BEFORE extraction and
        // saves that snapshot after: an edit landing in between stays visible.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let out = root.join("synaptic-out");
        write(root, "a.py", "x = 1\n");
        persist_manifest(&out, root).unwrap();

        // Rebuild starts: snapshot the pre-extraction state.
        let det = detect_inputs(root);
        let snapshot = snapshot_manifest(&out, &det);
        // Mid-rebuild edit.
        write(root, "a.py", "x = 2\n");
        // Rebuild finishes: persist the snapshot, not a fresh walk.
        snapshot.save(&manifest_path(&out)).unwrap();

        let report = detect_changes(&out, root);
        assert_eq!(
            report.diff.changed,
            vec!["a.py".to_string()],
            "an edit during the rebuild must stay detectable"
        );
    }

    #[test]
    fn detect_changes_empty_when_nothing_moved() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let out = root.join("synaptic-out");
        write(root, "a.py", "x = 1\n");
        persist_manifest(&out, root).unwrap();

        let report = detect_changes(&out, root);
        assert!(!report.bootstrapped, "prior manifest existed");
        assert!(report.is_empty(), "no edits => no changes");
    }
}
