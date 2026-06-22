//! Sibling git-repo discovery — multi-repo on-disk auto-discovery
//! (spec 2026-06-14). A bounded, noise-pruned `read_dir` walk that stops at each
//! `.git` boundary: it records a repo root and never descends into it, so cost is
//! ≈ `find -maxdepth N -name .git -prune`. The expensive per-member build is the
//! existing pipeline, unchanged.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use synaptic_detect::noise::is_noise_dir;

use crate::coordinate::{package_coordinate, Coordinate};
use crate::discover::has_recognized_manifest;
use crate::sanitize_tag;

/// Bounds for a sibling-repo scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanOptions {
    /// Directory levels below the scan root to examine (default 3).
    pub depth: usize,
    /// Max repos to include; the rest become `SkipReason::OverCap` (default 50).
    pub max: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions { depth: 3, max: 50 }
    }
}

/// A discovered git repository eligible to become a federated member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoCandidate {
    pub name: String,
    pub path: PathBuf,
    pub coordinate: Option<Coordinate>,
}

/// Why a git repo found during the scan was not included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    NoManifest,
    OverCap,
}

/// Outcome of a sibling-repo scan.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanResult {
    pub repos: Vec<RepoCandidate>,
    pub skipped: Vec<(PathBuf, SkipReason)>,
}

/// A directory is a git repo iff it contains a `.git` entry (dir, or a file for
/// worktrees/submodules).
fn is_git_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Recurse into `dir` collecting repo roots, stopping at each `.git` boundary.
/// `depth_left` is the number of directory levels still allowed below `dir`.
fn collect(dir: &Path, depth_left: usize, out: &mut Vec<PathBuf>) {
    if depth_left == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.filter_map(Result::ok) {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        // Real directories only: never follow symlinks (no filesystem escape).
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if is_noise_dir(&entry.file_name().to_string_lossy(), dir) {
            continue;
        }
        if is_git_repo(&path) {
            out.push(path); // repo boundary: record, do NOT descend
        } else {
            collect(&path, depth_left - 1, out);
        }
    }
}

/// Discover sibling git repositories under `scan_root` (see module docs). Repos
/// with a recognized manifest become `RepoCandidate`s (sorted by path, unique
/// sanitized names); git repos without one → `skipped(NoManifest)`; repos beyond
/// `opts.max` → `skipped(OverCap)`. `exclude_self` (compared canonically) drops the
/// current repo.
pub fn discover_sibling_repos(
    scan_root: &Path,
    opts: &ScanOptions,
    exclude_self: Option<&Path>,
) -> ScanResult {
    let scan_root = scan_root
        .canonicalize()
        .unwrap_or_else(|_| scan_root.to_path_buf());
    let exclude = exclude_self.and_then(|p| p.canonicalize().ok());

    let mut roots = Vec::new();
    collect(&scan_root, opts.depth, &mut roots);

    // Canonicalize, drop self, dedup, sort.
    let mut seen = HashSet::new();
    let mut kept: Vec<PathBuf> = Vec::new();
    for r in roots {
        let key = r.canonicalize().unwrap_or(r);
        if exclude.as_ref() == Some(&key) {
            continue;
        }
        if seen.insert(key.clone()) {
            kept.push(key);
        }
    }
    kept.sort();

    let mut result = ScanResult::default();
    let mut used: HashSet<String> = HashSet::new();
    for path in kept {
        if !has_recognized_manifest(&path) {
            result.skipped.push((path, SkipReason::NoManifest));
            continue;
        }
        if result.repos.len() >= opts.max {
            result.skipped.push((path, SkipReason::OverCap));
            continue;
        }
        let base = sanitize_tag(
            &path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "repo".into()),
        );
        let mut name = base.clone();
        let mut n = 2;
        while used.contains(&name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        used.insert(name.clone());
        let coordinate = package_coordinate(&path);
        result.repos.push(RepoCandidate {
            name,
            path,
            coordinate,
        });
    }
    result
}

/// Express `target` relative to `root` using `..` segments (posix separators), so a
/// discovered sibling repo is stored portably in the manifest. Falls back to an
/// absolute (lossily-stringified) path when the two share no common prefix (e.g. a
/// different Windows drive).
pub fn relative_path(root: &Path, target: &Path) -> String {
    let r: Vec<_> = root.components().collect();
    let t: Vec<_> = target.components().collect();
    let common = r.iter().zip(&t).take_while(|(a, b)| a == b).count();
    if common == 0 {
        return target.to_string_lossy().replace('\\', "/");
    }
    let ups = r.len() - common;
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), ups).collect();
    for c in &t[common..] {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// A layout with several sibling repos at varying depths.
    fn make_layout(parent: &Path) {
        touch(parent, "repoA/.git/HEAD", "ref: refs/heads/main\n");
        touch(parent, "repoA/Cargo.toml", "[package]\nname=\"a\"\n");
        // nested repo inside repoA: must NOT be discovered (boundary prune).
        touch(parent, "repoA/sub/.git/HEAD", "x\n");
        touch(parent, "repoA/sub/Cargo.toml", "[package]\nname=\"sub\"\n");
        touch(parent, "repoB/.git/HEAD", "x\n");
        touch(parent, "repoB/package.json", "{\"name\":\"b\"}");
        touch(parent, "group/repoC/.git/HEAD", "x\n");
        touch(parent, "group/repoC/go.mod", "module c\n");
        touch(parent, "plain/readme.md", "no git here\n");
        touch(parent, "repoNoManifest/.git/HEAD", "x\n");
    }

    #[test]
    fn finds_sibling_repos_with_manifests_and_skips_others() {
        let d = tempfile::tempdir().unwrap();
        make_layout(d.path());
        let res = discover_sibling_repos(d.path(), &ScanOptions::default(), None);
        let names: Vec<&str> = res.repos.iter().map(|r| r.name.as_str()).collect();
        assert!(
            names.contains(&"repoA") && names.contains(&"repoB") && names.contains(&"repoC"),
            "{names:?}"
        );
        // nested repoA/sub never discovered (we stop at repoA's .git).
        assert!(!names.contains(&"sub"), "boundary prune failed: {names:?}");
        // repoNoManifest is a git repo without a manifest -> skipped.
        assert!(
            res.skipped
                .iter()
                .any(|(p, r)| p.ends_with("repoNoManifest") && *r == SkipReason::NoManifest),
            "{:?}",
            res.skipped
        );
    }

    #[test]
    fn dotnet_solution_repo_is_discovered_with_coordinate() {
        // A git repo whose root holds only a `.sln` (projects in subdirs) must be
        // federated, not skipped as NoManifest.
        use crate::coordinate::Ecosystem;
        let d = tempfile::tempdir().unwrap();
        touch(d.path(), "desktop-app/.git/HEAD", "ref: refs/heads/main\n");
        touch(
            d.path(),
            "desktop-app/Acme.sln",
            "Project(\"{G}\") = \"Acme.App\", \"App\\Acme.App.csproj\", \"{P}\"\n",
        );
        touch(
            d.path(),
            "desktop-app/App/Acme.App.csproj",
            r#"<Project><PropertyGroup><AssemblyName>Acme.App</AssemblyName></PropertyGroup></Project>"#,
        );
        let res = discover_sibling_repos(d.path(), &ScanOptions::default(), None);
        let repo = res
            .repos
            .iter()
            .find(|r| r.name == "desktop-app")
            .unwrap_or_else(|| panic!("not discovered; skipped={:?}", res.skipped));
        let coord = repo.coordinate.as_ref().expect("has coordinate");
        assert_eq!(coord.ecosystem, Ecosystem::DotNet);
        assert_eq!(coord.name, "Acme.App");
    }

    #[test]
    fn depth_one_finds_only_direct_children() {
        let d = tempfile::tempdir().unwrap();
        make_layout(d.path());
        let res = discover_sibling_repos(d.path(), &ScanOptions { depth: 1, max: 50 }, None);
        let names: Vec<&str> = res.repos.iter().map(|r| r.name.as_str()).collect();
        assert!(
            names.contains(&"repoA") && names.contains(&"repoB"),
            "{names:?}"
        );
        assert!(
            !names.contains(&"repoC"),
            "group/repoC is depth 2: {names:?}"
        );
    }

    #[test]
    fn exclude_self_drops_the_current_repo() {
        let d = tempfile::tempdir().unwrap();
        make_layout(d.path());
        let me = d.path().join("repoA");
        let res = discover_sibling_repos(d.path(), &ScanOptions::default(), Some(&me));
        assert!(
            res.repos.iter().all(|r| r.name != "repoA"),
            "self not excluded"
        );
    }

    #[test]
    fn cap_overflows_into_skipped() {
        let d = tempfile::tempdir().unwrap();
        make_layout(d.path());
        let res = discover_sibling_repos(d.path(), &ScanOptions { depth: 3, max: 1 }, None);
        assert_eq!(res.repos.len(), 1);
        assert!(res.skipped.iter().any(|(_, r)| *r == SkipReason::OverCap));
    }

    #[test]
    fn relative_path_emits_dotdot_for_siblings() {
        assert_eq!(
            relative_path(Path::new("/x/parent/ws"), Path::new("/x/parent/repoA")),
            "../repoA"
        );
        assert_eq!(
            relative_path(
                Path::new("/x/parent/ws"),
                Path::new("/x/parent/group/repoC")
            ),
            "../group/repoC"
        );
    }

    #[test]
    fn noise_dirs_are_pruned() {
        let d = tempfile::tempdir().unwrap();
        touch(d.path(), "node_modules/pkg/.git/HEAD", "x\n");
        touch(
            d.path(),
            "node_modules/pkg/package.json",
            "{\"name\":\"x\"}",
        );
        let res = discover_sibling_repos(d.path(), &ScanOptions::default(), None);
        assert!(
            res.repos.is_empty(),
            "node_modules not pruned: {:?}",
            res.repos
        );
    }
}
