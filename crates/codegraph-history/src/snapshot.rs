//! Per-SHA snapshot store under `codegraph-out/history/<sha>.json`.

use std::path::{Path, PathBuf};

use codegraph_core::GraphData;

use crate::HistoryError;

/// Directory holding cached per-commit graphs.
pub fn history_dir(repo_root: &Path) -> PathBuf {
    repo_root.join("codegraph-out").join("history")
}

fn snapshot_path(repo_root: &Path, sha: &str, directed: bool) -> PathBuf {
    // The directedness is part of the key: a directed and an undirected build of
    // the same commit are different graphs (edge identity differs in graph_diff).
    let kind = if directed { "d" } else { "u" };
    history_dir(repo_root).join(format!("{sha}-{kind}.json"))
}

/// Load a cached graph for `sha` (built with `directed`), if present and parseable.
pub fn load(repo_root: &Path, sha: &str, directed: bool) -> Option<GraphData> {
    let text = std::fs::read_to_string(snapshot_path(repo_root, sha, directed)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Persist a graph for `sha` (built with `directed`).
pub fn save(
    repo_root: &Path,
    sha: &str,
    directed: bool,
    gd: &GraphData,
) -> Result<(), HistoryError> {
    let dir = history_dir(repo_root);
    std::fs::create_dir_all(&dir)?;
    let text = serde_json::to_string(gd)?;
    std::fs::write(snapshot_path(repo_root, sha, directed), text)?;
    Ok(())
}

/// Keep the `keep` most-recently-modified snapshots; delete the rest.
pub fn prune(repo_root: &Path, keep: usize) {
    let dir = history_dir(repo_root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|e| {
            e.metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| (t, e.path()))
        })
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0)); // newest first
    for (_, p) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let gd = GraphData {
            built_at_commit: Some("deadbeef".into()),
            ..GraphData::default()
        };
        save(root, "deadbeef", false, &gd).unwrap();
        let back = load(root, "deadbeef", false).unwrap();
        assert_eq!(back.built_at_commit.as_deref(), Some("deadbeef"));
        assert!(load(root, "missing", false).is_none());
        // A directed lookup must not hit the undirected snapshot.
        assert!(load(root, "deadbeef", true).is_none());
    }
}
