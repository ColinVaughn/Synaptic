//! Build the Synaptic at a revision (via a throwaway worktree) or working tree.

use std::path::Path;

use synaptic_core::GraphData;
use synaptic_graph::KnowledgeGraph;
use synaptic_incremental::{rebuild, ChangeSet, RebuildOptions};

use crate::{git, snapshot, HistoryError};

/// Build (or load from the snapshot store) the graph at commit `sha`.
pub fn build_at_rev(
    repo_root: &Path,
    sha: &str,
    directed: bool,
    use_cache: bool,
) -> Result<KnowledgeGraph, HistoryError> {
    if use_cache {
        if let Some(gd) = snapshot::load(repo_root, sha, directed) {
            return Ok(KnowledgeGraph::from_graph_data(gd));
        }
    }
    git::worktree_prune(repo_root);
    // Per-process worktree path: two concurrent diffs touching the same commit
    // never collide, and a leftover dir from a crashed run can't wedge this one.
    let wt = snapshot::history_dir(repo_root)
        .join("wt")
        .join(format!("{sha}-{}", std::process::id()));
    if wt.exists() {
        let _ = git::worktree_remove(repo_root, &wt);
        let _ = std::fs::remove_dir_all(&wt);
    }
    if let Some(parent) = wt.parent() {
        std::fs::create_dir_all(parent)?;
    }
    git::worktree_add(repo_root, &wt, sha)?;

    let built = rebuild(
        &RebuildOptions {
            root: wt.clone(),
            directed,
            force: true,
        },
        &ChangeSet::Full,
        None,
    );
    // Always tear the worktree down, even on error.
    let _ = git::worktree_remove(repo_root, &wt);
    let _ = std::fs::remove_dir_all(&wt);

    let outcome = built.map_err(|e| HistoryError::Rebuild(e.to_string()))?;
    let mut gd: GraphData = outcome.kg.to_graph_data();
    gd.built_at_commit = Some(sha.to_string());
    if use_cache {
        snapshot::save(repo_root, sha, directed, &gd)?;
        snapshot::prune(repo_root, 32);
    }
    Ok(KnowledgeGraph::from_graph_data(gd))
}

/// Build the graph for the current working tree (never cached).
pub fn build_working_tree(
    repo_root: &Path,
    directed: bool,
) -> Result<KnowledgeGraph, HistoryError> {
    let outcome = rebuild(
        &RebuildOptions {
            root: repo_root.to_path_buf(),
            directed,
            force: true,
        },
        &ChangeSet::Full,
        None,
    )
    .map_err(|e| HistoryError::Rebuild(e.to_string()))?;
    Ok(outcome.kg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git_run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .output()
            .expect("git")
            .status
            .success();
        assert!(ok, "git {:?}", args);
    }

    #[test]
    fn builds_graph_at_a_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git_run(root, &["init", "-q"]);
        std::fs::write(root.join("a.py"), b"def foo():\n    return 1\n").unwrap();
        git_run(root, &["add", "-A"]);
        git_run(root, &["commit", "-q", "-m", "c1", "--no-gpg-sign"]);
        let sha = git::rev_parse(root, "HEAD").unwrap();
        let kg = build_at_rev(root, &sha, false, true).unwrap();
        assert!(kg.node_count() >= 1, "graph should have nodes");
        // Second call hits the snapshot store.
        let kg2 = build_at_rev(root, &sha, false, true).unwrap();
        assert_eq!(kg.node_count(), kg2.node_count());
    }
}
