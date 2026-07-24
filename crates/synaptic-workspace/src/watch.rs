//! Workspace-scale filesystem watching: the pure targeting/filter core behind
//! `synaptic workspace build --watch`. The OS event loop (`notify`) lives in the
//! CLI, exactly as it does for the single-repo `synaptic watch`; this module
//! holds the testable logic — which roots a federated workspace must watch (its
//! own tree plus any member checked out *outside* it), which events matter, and
//! which member a changed path belongs to.
//!
//! The ignore and rebuildable rules are **not** redefined here: they delegate to
//! [`synaptic_incremental::should_ignore_path`] and
//! [`synaptic_incremental::is_rebuildable`], so a workspace watcher reacts to
//! exactly the input set the single-repo watcher does (and skips `synaptic-out`,
//! so writing the federated `graph.json` can never self-trigger).

use std::path::{Path, PathBuf};

use synaptic_incremental::{is_rebuildable, should_ignore_path};

use crate::discover::Member;
use crate::manifest::{RepoMember, MANIFEST_NAME};
use crate::workspace_build::resolve_members;
use crate::Result;

/// A filesystem event the workspace watcher acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// `synaptic-workspace.toml` changed: members may have been added or
    /// removed, so the watch targets must be re-resolved before rebuilding.
    Manifest,
    /// A member source file changed. Carries the path relative to the watch root
    /// it arrived under (absolute if it could not be stripped).
    Source(PathBuf),
}

/// Everything the watcher needs to resolve from a workspace, recomputed
/// whenever the manifest changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchTargets {
    /// Minimal set of directories to register a recursive watcher on.
    pub roots: Vec<PathBuf>,
    /// `(tag, on-disk root)` for every member with local source, longest-path
    /// first, so [`member_for_path`] can attribute a change to the innermost
    /// member.
    pub members: Vec<(String, PathBuf)>,
}

/// Absolute on-disk source roots of the declared `[[repos]]` members.
///
/// `git =` members live under `synaptic-out/workspace-repos/` (an ignored
/// subtree refreshed by `synaptic workspace sync`) and `subgraph =` members have
/// no source on disk, so neither is watchable — only `path =` members are.
fn repo_source_roots(root: &Path, repos: &[RepoMember]) -> Vec<(String, PathBuf)> {
    repos
        .iter()
        .filter(|r| r.subgraph.is_none())
        .filter_map(|r| {
            let p = r.path.as_ref()?;
            let abs = root.join(p);
            let abs = abs.canonicalize().unwrap_or(abs);
            Some((crate::sanitize_tag(&r.name), abs))
        })
        .collect()
}

/// True when a candidate root can actually be watched. A member declared with a
/// `path` that no longer exists (a checkout not cloned yet, a stale manifest
/// entry) must not take the whole watcher down: it is dropped here and surfaces
/// as a per-member build error instead.
fn is_watchable(p: &Path) -> bool {
    p.is_dir()
}

/// Collapse a candidate root list to the minimal set: a root already contained
/// in another watched root is dropped, because `notify` watches recursively and
/// overlapping registrations just duplicate every event.
fn minimal_roots(mut candidates: Vec<PathBuf>) -> Vec<PathBuf> {
    candidates.sort();
    candidates.dedup();
    // Shortest first: an ancestor is always a prefix of its descendants, so one
    // forward pass keeping only paths not already covered leaves the minimal set.
    candidates.sort_by_key(|p| p.components().count());
    let mut kept: Vec<PathBuf> = Vec::new();
    for c in candidates {
        if kept.iter().any(|k| c.starts_with(k)) {
            continue;
        }
        kept.push(c);
    }
    kept.sort();
    kept
}

/// The directories a federated workspace must watch: its own root (covering
/// every in-tree member) plus each member checked out *outside* it — the
/// multi-repo case, where `[[repos]] path = "../identity"` points at a sibling
/// checkout that the workspace root's recursive watcher would never see.
///
/// Roots that do not exist on disk are dropped rather than returned, so one
/// stale manifest entry cannot make the whole watcher unstartable.
pub fn watch_roots(root: &Path, members: &[Member], repos: &[RepoMember]) -> Vec<PathBuf> {
    let mut candidates = vec![root.to_path_buf()];
    candidates.extend(members.iter().map(|m| m.path.clone()));
    candidates.extend(repo_source_roots(root, repos).into_iter().map(|(_, p)| p));
    candidates.retain(|p| is_watchable(p));
    minimal_roots(candidates)
}

/// Resolve the watch targets for a workspace root: the roots to register
/// watchers on plus the member map used to attribute each change.
pub fn resolve_watch_targets(root: &Path) -> Result<WatchTargets> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let (members, repos) = resolve_members(&root)?;
    let roots = watch_roots(&root, &members, &repos);
    let mut member_roots: Vec<(String, PathBuf)> = members
        .iter()
        .map(|m| (m.tag.clone(), m.path.clone()))
        .collect();
    member_roots.extend(repo_source_roots(&root, &repos));
    // Longest path first so a member nested inside another wins attribution.
    member_roots.sort_by(|a, b| {
        b.1.components()
            .count()
            .cmp(&a.1.components().count())
            .then_with(|| a.0.cmp(&b.0))
    });
    Ok(WatchTargets {
        roots,
        members: member_roots,
    })
}

/// True when `path` is a workspace manifest (adding or removing a member).
pub fn is_manifest_path(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == MANIFEST_NAME)
}

/// Classify one raw filesystem event path against the watch root it arrived
/// under. Returns `None` for anything that cannot change the federated graph:
/// ignored subtrees (`synaptic-out`, `.git`, `target`, `node_modules`, ...) and
/// non-extractable file types.
///
/// Filtering runs on the path made **relative to its watch root**, matching the
/// single-repo watcher: a noise directory name in an ancestor of the root (a
/// checkout under `/build/app`) must not ignore the whole tree.
pub fn classify(abs: &Path, watch_root: &Path) -> Option<WatchEvent> {
    let rel = abs
        .strip_prefix(watch_root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| abs.to_path_buf());
    if should_ignore_path(&rel) {
        return None;
    }
    // A manifest edit changes the member set itself, so it matters even though
    // TOML is not an extractable source file.
    if is_manifest_path(&rel) {
        return Some(WatchEvent::Manifest);
    }
    if !is_rebuildable(&rel) {
        return None;
    }
    Some(WatchEvent::Source(rel))
}

/// The member tag owning an absolute changed path, for reporting which
/// repository triggered a rebuild. `members` is expected longest-path first (as
/// [`resolve_watch_targets`] returns it), so the innermost member wins.
pub fn member_for_path<'a>(abs: &Path, members: &'a [(String, PathBuf)]) -> Option<&'a str> {
    members
        .iter()
        .find(|(_, root)| abs.starts_with(root))
        .map(|(tag, _)| tag.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(tag: &str, path: &Path) -> Member {
        Member {
            tag: tag.into(),
            path: path.to_path_buf(),
            coordinate: None,
        }
    }

    fn path_repo(name: &str, path: &str) -> RepoMember {
        RepoMember {
            name: name.into(),
            git: None,
            rev: None,
            subgraph: None,
            path: Some(path.into()),
        }
    }

    #[test]
    fn in_tree_members_are_covered_by_the_workspace_root_alone() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join("pkgs/app")).unwrap();
        std::fs::create_dir_all(root.join("pkgs/lib")).unwrap();
        let members = vec![
            member("app", &root.join("pkgs/app")),
            member("lib", &root.join("pkgs/lib")),
        ];
        // Recursive watchers already cover descendants; registering each member
        // too would just duplicate every event.
        assert_eq!(
            watch_roots(root, &members, &[]),
            vec![root.to_path_buf()],
            "one recursive watcher covers every in-tree member"
        );
    }

    #[test]
    fn a_missing_member_path_is_dropped_instead_of_breaking_the_watcher() {
        // A stale `[[repos]] path = ...` entry (checkout not present) must not
        // make the watcher unstartable; it surfaces as a per-member build error.
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let repos = vec![path_repo("gone", "../does-not-exist")];
        assert_eq!(
            watch_roots(root, &[], &repos),
            vec![root.to_path_buf()],
            "only the existing workspace root is watched"
        );
    }

    #[test]
    fn out_of_tree_repo_members_get_their_own_root() {
        // The multi-repo case: a sibling checkout outside the workspace tree is
        // invisible to the workspace root's recursive watcher.
        let d = tempfile::tempdir().unwrap();
        let ws = d.path().join("ws");
        let sibling = d.path().join("identity");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let repos = vec![path_repo("identity", "../identity")];
        let roots = watch_roots(&ws, &[], &repos);
        let canon_ws = ws.canonicalize().unwrap();
        let canon_sib = sibling.canonicalize().unwrap();
        assert!(
            roots.contains(&canon_ws) || roots.contains(&ws),
            "{roots:?}"
        );
        assert!(
            roots.contains(&canon_sib),
            "sibling checkout watched: {roots:?}"
        );
    }

    #[test]
    fn git_and_subgraph_repos_are_not_watched() {
        // A `git =` member lives under synaptic-out/workspace-repos (ignored,
        // refreshed by `workspace sync`); a `subgraph =` member has no source.
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let repos = vec![
            RepoMember {
                name: "remote".into(),
                git: Some("https://example.com/a/b".into()),
                rev: None,
                subgraph: None,
                path: None,
            },
            RepoMember {
                name: "published".into(),
                git: None,
                rev: None,
                subgraph: Some("https://example.com/graph.json".into()),
                path: None,
            },
        ];
        assert_eq!(watch_roots(root, &[], &repos), vec![root.to_path_buf()]);
    }

    #[test]
    fn nested_candidate_roots_collapse_to_the_outermost() {
        let kept = minimal_roots(vec![
            PathBuf::from("/a/b/c"),
            PathBuf::from("/a"),
            PathBuf::from("/a/b"),
            PathBuf::from("/z"),
        ]);
        assert_eq!(kept, vec![PathBuf::from("/a"), PathBuf::from("/z")]);
    }

    #[test]
    fn ignores_output_vcs_and_build_subtrees() {
        let root = Path::new("/ws");
        for p in [
            // Writing the federated graph must never self-trigger.
            "/ws/synaptic-out/graph.json",
            // Nor may a member's own per-member cache.
            "/ws/pkgs/app/synaptic-out/cache/x.bin",
            "/ws/.git/index",
            "/ws/target/debug/foo",
            "/ws/node_modules/x/y.js",
            "/ws/pkgs/app/dist/bundle.js",
        ] {
            assert_eq!(classify(Path::new(p), root), None, "should ignore {p}");
        }
    }

    #[test]
    fn ignores_non_extractable_files() {
        let root = Path::new("/ws");
        for p in ["/ws/pkgs/app/notes.txt", "/ws/logo.png", "/ws/data.bin"] {
            assert_eq!(classify(Path::new(p), root), None, "should skip {p}");
        }
    }

    #[test]
    fn classifies_member_sources_relative_to_their_watch_root() {
        let root = Path::new("/ws");
        assert_eq!(
            classify(Path::new("/ws/pkgs/app/src/lib.rs"), root),
            Some(WatchEvent::Source(PathBuf::from("pkgs/app/src/lib.rs"))),
        );
        // Markdown is extractable (heading structure), same as `synaptic watch`.
        assert_eq!(
            classify(Path::new("/ws/pkgs/app/README.md"), root),
            Some(WatchEvent::Source(PathBuf::from("pkgs/app/README.md"))),
        );
    }

    #[test]
    fn a_noise_name_above_the_watch_root_does_not_ignore_the_tree() {
        // A checkout that happens to live under a directory named `build` is
        // still fully watched: filtering is relative to the watch root.
        let root = Path::new("/build/app");
        assert_eq!(
            classify(Path::new("/build/app/src/main.rs"), root),
            Some(WatchEvent::Source(PathBuf::from("src/main.rs"))),
        );
    }

    #[test]
    fn manifest_edits_are_classified_even_though_toml_is_not_extractable() {
        let root = Path::new("/ws");
        assert_eq!(
            classify(Path::new("/ws/synaptic-workspace.toml"), root),
            Some(WatchEvent::Manifest),
        );
    }

    #[test]
    fn changed_paths_attribute_to_the_innermost_member() {
        // Longest-path-first ordering, as resolve_watch_targets produces.
        let members = vec![
            ("inner".to_string(), PathBuf::from("/ws/pkgs/app/inner")),
            ("app".to_string(), PathBuf::from("/ws/pkgs/app")),
        ];
        assert_eq!(
            member_for_path(Path::new("/ws/pkgs/app/inner/src/a.rs"), &members),
            Some("inner")
        );
        assert_eq!(
            member_for_path(Path::new("/ws/pkgs/app/src/a.rs"), &members),
            Some("app")
        );
        assert_eq!(
            member_for_path(Path::new("/elsewhere/a.rs"), &members),
            None
        );
    }

    #[test]
    fn resolve_targets_maps_members_longest_first() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join("pkgs/app/src")).unwrap();
        std::fs::create_dir_all(r.join("pkgs/lib/src")).unwrap();
        std::fs::write(
            r.join("synaptic-workspace.toml"),
            "[workspace]\nname = \"demo\"\nmembers = [\"pkgs/*\"]\n",
        )
        .unwrap();
        std::fs::write(r.join("pkgs/app/Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
        std::fs::write(r.join("pkgs/app/src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(r.join("pkgs/lib/Cargo.toml"), "[package]\nname = \"lib\"\n").unwrap();
        std::fs::write(r.join("pkgs/lib/src/lib.rs"), "pub fn b() {}\n").unwrap();

        let targets = resolve_watch_targets(r).unwrap();
        let canon = r.canonicalize().unwrap();
        assert_eq!(targets.roots, vec![canon.clone()], "one in-tree root");
        let tags: Vec<&str> = targets.members.iter().map(|(t, _)| t.as_str()).collect();
        assert!(tags.contains(&"app") && tags.contains(&"lib"), "{tags:?}");
        assert_eq!(
            member_for_path(&canon.join("pkgs/app/src/lib.rs"), &targets.members),
            Some("app")
        );
    }
}
