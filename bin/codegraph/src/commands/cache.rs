//! `cache` command(s) split from main.rs.

use crate::cli::CacheAction;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn run_cache(action: CacheAction) -> Result<()> {
    match action {
        CacheAction::Clear { path, recursive } => {
            let removed = clear_caches(&path, recursive)?;
            if removed.is_empty() {
                println!("No cache found under {}.", path.display());
            } else {
                for dir in &removed {
                    println!("Removed {}", dir.display());
                }
                println!("Cleared {} cache director(ies).", removed.len());
            }
            Ok(())
        }
    }
}

/// Remove `codegraph-out/cache` under `root` (and, with `recursive`, every such
/// directory beneath it). Only ever touches the codegraph-owned, regenerable
/// `codegraph-out/cache` subtree. Returns the directories actually removed.
pub(crate) fn clear_caches(root: &Path, recursive: bool) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    let top = root.join("codegraph-out").join("cache");
    if top.is_dir() {
        fs::remove_dir_all(&top).with_context(|| format!("removing {}", top.display()))?;
        removed.push(top);
    }
    if recursive {
        find_member_caches(root, 8, &mut removed)?;
    }
    Ok(removed)
}

/// Bounded, noise-pruned walk collecting + removing nested `codegraph-out/cache`
/// dirs (skips `node_modules`/`.git`; does not descend into a matched `codegraph-out`).
pub(crate) fn find_member_caches(
    dir: &Path,
    depth: usize,
    removed: &mut Vec<PathBuf>,
) -> Result<()> {
    if depth == 0 {
        return Ok(());
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in rd.filter_map(std::result::Result::ok) {
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(name.as_ref(), "node_modules" | ".git") {
            continue;
        }
        if name == "codegraph-out" {
            let cache = path.join("cache");
            if cache.is_dir() && !removed.iter().any(|r| r == &cache) {
                fs::remove_dir_all(&cache)
                    .with_context(|| format!("removing {}", cache.display()))?;
                removed.push(cache);
            }
            continue; // don't descend into codegraph-out
        }
        find_member_caches(&path, depth - 1, removed)?;
    }
    Ok(())
}

#[cfg(test)]
mod cache_clear_tests {
    use super::clear_caches;
    use std::fs;

    fn make_cache(root: &std::path::Path) -> std::path::PathBuf {
        let dir = root.join("codegraph-out").join("cache").join("ast");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("x.json"), b"{}").unwrap();
        root.join("codegraph-out").join("cache")
    }

    #[test]
    fn cache_clear_removes_cache_dir() {
        let d = tempfile::tempdir().unwrap();
        let cache = make_cache(d.path());
        assert!(cache.is_dir());
        let removed = clear_caches(d.path(), false).unwrap();
        assert_eq!(removed, vec![cache.clone()]);
        assert!(!cache.exists(), "cache removed");
    }

    #[test]
    fn cache_clear_absent_is_ok() {
        let d = tempfile::tempdir().unwrap();
        let removed = clear_caches(d.path(), false).unwrap();
        assert!(removed.is_empty());
    }

    #[test]
    fn cache_clear_recursive_removes_nested() {
        let d = tempfile::tempdir().unwrap();
        let root_cache = make_cache(d.path());
        let member_cache = make_cache(&d.path().join("packages").join("hub"));
        // A noise dir that must be skipped (and would blow depth if descended).
        fs::create_dir_all(d.path().join("node_modules").join("dep")).unwrap();
        let removed = clear_caches(d.path(), true).unwrap();
        assert!(removed.contains(&root_cache));
        assert!(removed.contains(&member_cache));
        assert!(!member_cache.exists(), "nested member cache removed");
    }
}
