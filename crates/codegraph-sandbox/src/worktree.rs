//! A throwaway `git worktree` that removes itself on drop. Reuses the worktree
//! plumbing from `codegraph-history` so the speculative run is isolated from the
//! user's real working tree and leaves nothing behind, even on error or panic.

use std::path::{Path, PathBuf};

use codegraph_history::git;

use crate::SandboxError;

/// A detached worktree checked out at a commit, removed when this value drops.
pub(crate) struct Worktree {
    repo_root: PathBuf,
    path: PathBuf,
    removed: bool,
}

impl Worktree {
    /// Materialize `sha` into a fresh worktree under `codegraph-out/sandbox/`
    /// (gitignored, so it never shows up in the user's status). A per-process
    /// suffix keeps concurrent runs from colliding on the same commit.
    ///
    /// Placing the worktree *inside* the repo is deliberate: a clean checkout has
    /// no installed dependencies, but tooling that resolves dependencies upward
    /// (Node's `node_modules` lookup) then finds the parent repo's installed deps,
    /// so `npm`/`tsc`/etc. work without an install step. Ecosystems that do not
    /// resolve upward (Python venv, etc.) still need their environment available;
    /// see the speculate docs.
    pub fn create(repo_root: &Path, sha: &str) -> Result<Worktree, SandboxError> {
        git::worktree_prune(repo_root);
        let path = repo_root
            .join("codegraph-out")
            .join("sandbox")
            .join("wt")
            .join(format!("{sha}-{}", std::process::id()));
        // A leftover dir from a crashed run must not wedge this one.
        if path.exists() {
            let _ = git::worktree_remove(repo_root, &path);
            let _ = std::fs::remove_dir_all(&path);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        git::worktree_add(repo_root, &path, sha).map_err(|e| SandboxError::Git(e.to_string()))?;
        Ok(Worktree {
            repo_root: repo_root.to_path_buf(),
            path,
            removed: false,
        })
    }

    /// The worktree's root path (where the proposed change is applied and the
    /// commands run).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Tear the worktree down. Idempotent; also called from `Drop`.
    pub fn remove(&mut self) {
        if self.removed {
            return;
        }
        let _ = git::worktree_remove(&self.repo_root, &self.path);
        let _ = std::fs::remove_dir_all(&self.path);
        self.removed = true;
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        self.remove();
    }
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
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn creates_and_removes_a_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git_run(root, &["init", "-q"]);
        std::fs::write(root.join("a.txt"), b"hello\n").unwrap();
        git_run(root, &["add", "-A"]);
        git_run(root, &["commit", "-q", "-m", "c1", "--no-gpg-sign"]);
        let sha = git::rev_parse(root, "HEAD").unwrap();

        let saved_path;
        {
            let wt = Worktree::create(root, &sha).unwrap();
            saved_path = wt.path().to_path_buf();
            assert!(wt.path().join("a.txt").exists(), "checkout has the file");
        } // drop removes it
        assert!(!saved_path.exists(), "worktree dir removed on drop");
    }

    #[test]
    fn remove_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git_run(root, &["init", "-q"]);
        std::fs::write(root.join("a.txt"), b"hello\n").unwrap();
        git_run(root, &["add", "-A"]);
        git_run(root, &["commit", "-q", "-m", "c1", "--no-gpg-sign"]);
        let sha = git::rev_parse(root, "HEAD").unwrap();
        let mut wt = Worktree::create(root, &sha).unwrap();
        wt.remove();
        wt.remove(); // second call is a no-op, no panic
    }
}
