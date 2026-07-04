//! Synaptic file detection: discovery, classification, ignore handling, manifest.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

pub mod classify;
pub mod file_type;
pub mod manifest;
pub mod noise;
pub mod sensitive;
pub mod submodule;

pub use classify::classify_file;
pub use file_type::{FileType, ALL_FILE_TYPES};
pub use manifest::{hash_file, relative_key, FileEntry, Manifest, ManifestDiff};
pub use sensitive::is_sensitive;
pub use submodule::submodule_paths;

const CORPUS_WARN_THRESHOLD: usize = 50_000;
const CORPUS_UPPER_THRESHOLD: usize = 500_000;
const FILE_COUNT_UPPER: usize = 500;

/// Result of scanning a corpus root.
#[derive(Debug, Clone, Default)]
pub struct DetectResult {
    /// Classified files, per type, sorted by path.
    pub files: BTreeMap<FileType, Vec<PathBuf>>,
    pub total_files: usize,
    pub total_words: usize,
    pub needs_graph: bool,
    pub warning: Option<String>,
    pub skipped_sensitive: Vec<PathBuf>,
    pub scan_root: PathBuf,
    /// `tsconfig.json` / `jsconfig.json` files found in the same walk. These
    /// classify to `None` (so they never enter `files`), but the JS/TS import
    /// resolver needs them to expand `@/...` path aliases. Sorted by path.
    pub ts_config_files: Vec<PathBuf>,
}

impl DetectResult {
    /// Files classified as `ft` (empty slice if none).
    pub fn of(&self, ft: FileType) -> &[PathBuf] {
        self.files.get(&ft).map(Vec::as_slice).unwrap_or(&[])
    }
}

fn count_words(path: &Path) -> usize {
    // Lossy decode so non-UTF8 files still contribute words. Hard read
    // failure counts 0.
    std::fs::read(path)
        .map(|b| String::from_utf8_lossy(&b).split_whitespace().count())
        .unwrap_or(0)
}

/// Discover and classify files under `root`. Honors `.synapticignore` and
/// `.gitignore` (both apply, per-directory + parents; `.synapticignore` wins on
/// conflicts via the ignore crate's higher custom-ignore precedence tier), prunes
/// noise dirs, and skips lock + sensitive files.
pub fn detect(root: &Path) -> DetectResult {
    detect_impl(root, true)
}

/// Like [`detect`] but without the corpus word count, which reads every
/// code/document file and exists only to feed `extract`'s size warning. The
/// rebuild and staleness-detection paths run a detect per update round / serve
/// catch-up check, where those reads are O(repo bytes) of waste; they use this.
/// `total_words`/`needs_graph`/`warning` are not meaningful in the result.
pub fn detect_inputs(root: &Path) -> DetectResult {
    detect_impl(root, false)
}

fn detect_impl(root: &Path, count: bool) -> DetectResult {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    // Both `.synapticignore` and `.gitignore` apply, layered per-directory and up
    // to the VCS root. `add_custom_ignore_filename` gives `.synapticignore` higher
    // precedence than `.gitignore` for conflicting rules (e.g. a `!` re-include).
    // (A previous root-level toggle disabled gitignore globally when a root
    // `.synapticignore` existed, which silently dropped every subdir/parent
    // `.gitignore` and leaked ignored files into the corpus. Audit fix.)
    let root_guard = root.clone();
    let walker = WalkBuilder::new(&root)
        .hidden(false) // walk dotfiles; noise/sensitive rules handle exclusions
        .parents(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .follow_links(true)
        .add_custom_ignore_filename(".synapticignore")
        .filter_entry(move |entry| {
            // Prune symlinks whose real path escapes the scan root (escape/cycle
            // guard): in-tree links are followed, out-of-tree ones are not.
            if entry.path_is_symlink() {
                if let Ok(real) = entry.path().canonicalize() {
                    if !real.starts_with(&root_guard) {
                        return false;
                    }
                }
            }
            // Prune noise directories so we never descend into them.
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                let name = entry.file_name().to_string_lossy();
                let parent = entry.path().parent().unwrap_or_else(|| Path::new(""));
                return !noise::is_noise_dir(&name, parent);
            }
            true
        })
        .build();

    let mut files: BTreeMap<FileType, Vec<PathBuf>> = BTreeMap::new();
    let mut skipped_sensitive: Vec<PathBuf> = Vec::new();
    let mut ts_config_files: Vec<PathBuf> = Vec::new();
    let mut total_words = 0usize;

    for entry in walker.filter_map(Result::ok) {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy();
        if noise::is_skip_file(&name) {
            continue;
        }
        // tsconfig/jsconfig classify to `None` but the JS/TS alias resolver needs
        // them. Capture before the classify step (which would drop them).
        if name == "tsconfig.json" || name == "jsconfig.json" {
            ts_config_files.push(path.to_path_buf());
        }
        if is_sensitive(path) {
            skipped_sensitive.push(path.to_path_buf());
            continue;
        }
        if let Some(ft) = classify_file(path) {
            if count && matches!(ft, FileType::Code | FileType::Document | FileType::Paper) {
                total_words += count_words(path);
            }
            files.entry(ft).or_default().push(path.to_path_buf());
        }
    }

    for list in files.values_mut() {
        list.sort();
    }
    skipped_sensitive.sort();
    ts_config_files.sort();

    let total_files: usize = files.values().map(Vec::len).sum();
    let needs_graph = total_words >= CORPUS_WARN_THRESHOLD;
    let warning = if !count {
        None
    } else if !needs_graph {
        Some(format!(
            "Corpus is ~{total_words} words - fits in a single context window. You may not need a graph."
        ))
    } else if total_words >= CORPUS_UPPER_THRESHOLD || total_files >= FILE_COUNT_UPPER {
        Some(format!(
            "Large corpus: {total_files} files · ~{total_words} words. Semantic extraction will be expensive. Consider running on a subfolder."
        ))
    } else {
        None
    };

    DetectResult {
        files,
        total_files,
        total_words,
        needs_graph,
        warning,
        skipped_sensitive,
        scan_root: root,
        ts_config_files,
    }
}

/// Noise- and gitignore-aware list of directories under `root` (including `root`),
/// sorted, pruned by the same rules as [`detect`]. `max_depth` is relative to
/// `root` (None = unlimited). Used by workspace project-root discovery.
pub fn walk_dirs(root: &Path, max_depth: Option<usize>) -> Vec<PathBuf> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let root_guard = root.clone();
    let mut builder = WalkBuilder::new(&root);
    builder
        .hidden(false)
        .parents(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .follow_links(true)
        .add_custom_ignore_filename(".synapticignore")
        .filter_entry(move |entry| {
            if entry.path_is_symlink() {
                if let Ok(real) = entry.path().canonicalize() {
                    if !real.starts_with(&root_guard) {
                        return false;
                    }
                }
            }
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                let name = entry.file_name().to_string_lossy();
                let parent = entry.path().parent().unwrap_or_else(|| Path::new(""));
                return !noise::is_noise_dir(&name, parent);
            }
            true
        });
    if let Some(d) = max_depth {
        builder.max_depth(Some(d));
    }
    let mut dirs: Vec<PathBuf> = builder
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_dir()))
        .map(|e| e.path().to_path_buf())
        .collect();
    dirs.sort();
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn follows_in_tree_symlink_dir() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        write(r, "real/mod.rs", "pub fn f() {}\n");
        std::os::unix::fs::symlink(r.join("real"), r.join("linked")).unwrap();
        let det = detect(r);
        let code: Vec<String> = det
            .of(FileType::Code)
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        // The file is reachable via the real path (and via the in-tree symlink).
        assert!(code.iter().any(|c| c.ends_with("real/mod.rs")), "{code:?}");
    }

    #[test]
    fn walk_dirs_lists_real_dirs_and_prunes_noise() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        write(r, "a/Cargo.toml", "[package]\nname=\"a\"\n");
        write(r, "b/c/pkg.json", "{}");
        write(r, "node_modules/x/index.js", "x");
        write(r, "target/debug/foo", "x");
        let dirs = walk_dirs(r, Some(6));
        let names: Vec<String> = dirs
            .iter()
            .map(|p| {
                p.strip_prefix(r.canonicalize().unwrap())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert!(names.iter().any(|n| n == "a"), "{names:?}");
        assert!(names.iter().any(|n| n == "b/c"), "{names:?}");
        assert!(
            !names.iter().any(|n| n.starts_with("node_modules")),
            "noise pruned: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("target")),
            "noise pruned: {names:?}"
        );
    }

    fn code_names(r: &DetectResult) -> Vec<String> {
        r.of(FileType::Code)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn detect_inputs_classifies_identically_without_corpus_reads() {
        // detect() reads every code/doc file to count corpus words -- an
        // `extract` UX hint only. The rebuild/staleness paths run a detect on
        // every update round and every serve catch-up check, where those reads
        // are O(repo bytes) of pure waste. The stats-free variant must walk and
        // classify identically (same ignore rules, same tsconfig capture) while
        // skipping the reads.
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/main.py", "def main():\n    pass\n");
        write(r, "README.md", "# hi\n");
        write(r, "tsconfig.json", "{}\n");
        write(r, "node_modules/x/index.js", "x");
        write(r, ".gitignore", "generated/\n");
        write(r, "generated/gen.py", "g = 1\n");

        let full = detect(r);
        let inputs = detect_inputs(r);
        assert_eq!(full.files, inputs.files, "identical classification");
        assert_eq!(full.ts_config_files, inputs.ts_config_files);
        assert_eq!(full.scan_root, inputs.scan_root);
        assert_eq!(inputs.total_words, 0, "no corpus reads");
        assert!(inputs.warning.is_none(), "no corpus warning");
        assert!(
            !inputs
                .of(FileType::Code)
                .iter()
                .any(|p| p.to_string_lossy().contains("generated")),
            "gitignore still honored"
        );
    }

    #[test]
    fn discovers_and_classifies() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/main.py", "def main():\n    pass\n");
        write(dir.path(), "README.md", "# hi\n");
        let r = detect(dir.path());
        assert!(code_names(&r).contains(&"main.py".to_string()));
        assert_eq!(r.of(FileType::Document).len(), 1);
        assert_eq!(r.total_files, 2);
    }

    #[test]
    fn captures_tsconfig_and_jsconfig() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "tsconfig.json", "{}\n");
        write(dir.path(), "packages/app/tsconfig.json", "{}\n");
        write(dir.path(), "jsconfig.json", "{}\n");
        write(dir.path(), "src/app.ts", "export const x = 1;\n");
        // A dependency tsconfig under a pruned noise dir must NOT be captured.
        write(dir.path(), "node_modules/dep/tsconfig.json", "{}\n");
        let r = detect(dir.path());
        let names: Vec<String> = r
            .ts_config_files
            .iter()
            .map(|p| {
                p.strip_prefix(dir.path().canonicalize().unwrap())
                    .unwrap_or(p)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert!(names.contains(&"tsconfig.json".to_string()), "{names:?}");
        assert!(names.contains(&"jsconfig.json".to_string()), "{names:?}");
        assert!(
            names.contains(&"packages/app/tsconfig.json".to_string()),
            "{names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("node_modules")),
            "node_modules tsconfig must be pruned: {names:?}"
        );
    }

    #[test]
    fn prunes_noise_dirs() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/app.py", "x = 1\n");
        write(
            dir.path(),
            "node_modules/dep/index.js",
            "module.exports = {}\n",
        );
        write(dir.path(), "target/build.rs", "fn main() {}\n");
        let r = detect(dir.path());
        let names = code_names(&r);
        assert!(names.contains(&"app.py".to_string()));
        assert!(!names.iter().any(|n| n == "index.js"));
        assert!(!names.iter().any(|n| n == "build.rs"));
    }

    #[test]
    fn skips_lock_and_sensitive_files() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Cargo.lock", "# lock\n");
        write(dir.path(), ".env", "SECRET=1\n");
        write(dir.path(), "main.py", "x = 1\n");
        let r = detect(dir.path());
        assert_eq!(r.total_files, 1); // only main.py
        assert!(r.skipped_sensitive.iter().any(|p| p.ends_with(".env")));
    }

    #[test]
    fn synapticignore_excludes_with_negation() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".synapticignore", "*.py\n!keep.py\n");
        write(dir.path(), "drop.py", "x = 1\n");
        write(dir.path(), "keep.py", "x = 2\n");
        let r = detect(dir.path());
        let names = code_names(&r);
        assert!(names.contains(&"keep.py".to_string()));
        assert!(!names.contains(&"drop.py".to_string()));
    }

    #[test]
    fn negation_cannot_rescue_under_excluded_dir() {
        let dir = tempfile::tempdir().unwrap();
        // android/ excluded; !src/ cannot re-include a file under android/.
        write(dir.path(), ".synapticignore", "android/\n!src/\n");
        write(dir.path(), "android/app/src/Main.kt", "fun main() {}\n");
        write(dir.path(), "lib/src/Other.kt", "fun other() {}\n");
        let r = detect(dir.path());
        let names = code_names(&r);
        assert!(!names.contains(&"Main.kt".to_string()));
        assert!(names.contains(&"Other.kt".to_string()));
    }

    #[test]
    fn gitignore_and_synapticignore_layer_with_synapticignore_precedence() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        // .gitignore excludes vendor/ AND keep.py; .synapticignore excludes other.py
        // and RE-INCLUDES keep.py (precedence over .gitignore).
        write(dir.path(), ".gitignore", "vendor/\nkeep.py\n");
        write(dir.path(), ".synapticignore", "other.py\n!keep.py\n");
        write(dir.path(), "vendor/lib.py", "x = 1\n");
        write(dir.path(), "main.py", "x = 1\n");
        write(dir.path(), "other.py", "x = 1\n");
        write(dir.path(), "keep.py", "x = 1\n");
        let names = code_names(&detect(dir.path()));
        assert!(names.contains(&"main.py".to_string()));
        assert!(
            !names.contains(&"lib.py".to_string()),
            "gitignore still applies (layered)"
        );
        assert!(
            !names.contains(&"other.py".to_string()),
            "synapticignore applies"
        );
        assert!(
            names.contains(&"keep.py".to_string()),
            "synapticignore !keep.py wins over .gitignore"
        );
    }

    #[test]
    fn subdir_gitignore_still_applies_under_root_synapticignore() {
        // Audit regression: a root .synapticignore must NOT disable a subdir's
        // .gitignore (the old toggle did, leaking ignored files in).
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        write(dir.path(), ".synapticignore", "rootignored.py\n");
        write(dir.path(), "rootignored.py", "x = 1\n");
        write(dir.path(), "sub/.gitignore", "build_artifact.py\n");
        write(dir.path(), "sub/build_artifact.py", "x = 1\n");
        write(dir.path(), "sub/keep.py", "x = 1\n");
        let names = code_names(&detect(dir.path()));
        assert!(names.contains(&"keep.py".to_string()));
        assert!(
            !names.contains(&"rootignored.py".to_string()),
            "root synapticignore applies"
        );
        assert!(
            !names.contains(&"build_artifact.py".to_string()),
            "subdir .gitignore must still apply"
        );
    }
}
