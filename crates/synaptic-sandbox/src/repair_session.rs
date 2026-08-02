use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use synaptic_history::git;

use crate::SandboxError;

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// A branch-backed API repair worktree. The checkout is always removed on drop;
/// its branch is retained only after an explicit verified handoff.
pub struct RepairSession {
    repo_root: PathBuf,
    path: PathBuf,
    branch: String,
    base_sha: String,
    retain_branch: bool,
    removed: bool,
}

impl RepairSession {
    pub fn create(
        repo_root: &Path,
        base: &str,
        vendor: &str,
        event_id: &str,
    ) -> Result<Self, SandboxError> {
        let repo_root = repo_root.canonicalize()?;
        let vendor = safe_component(vendor)?;
        let event = safe_component(event_id)?;
        let base_sha = git::rev_parse(&repo_root, base)
            .map_err(|error| SandboxError::Git(error.to_string()))?;
        git::worktree_prune(&repo_root);
        let short_event = event.chars().take(16).collect::<String>();
        let branch = format!("synaptic/api/{vendor}/{short_event}");
        let nonce = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = repo_root
            .join("synaptic-out")
            .join("api-maintenance")
            .join("worktrees")
            .join(format!("{short_event}-{}-{nonce}", std::process::id()));
        validate_session_path(&repo_root, &path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        git::worktree_add(&repo_root, &path, &base_sha)
            .map_err(|error| SandboxError::Git(error.to_string()))?;
        if let Err(error) = git_command(&path, &["switch", "-C", &branch, &base_sha]) {
            let _ = git::worktree_remove(&repo_root, &path);
            return Err(error);
        }
        Ok(Self {
            repo_root,
            path,
            branch,
            base_sha,
            retain_branch: false,
            removed: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    pub fn base_sha(&self) -> &str {
        &self.base_sha
    }

    /// Apply a pre-validated unified diff without invoking a project command.
    pub fn apply_patch(&self, patch: &[u8]) -> Result<(), SandboxError> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args(["apply", "--index", "--whitespace=nowarn", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        use std::io::Write;
        child
            .stdin
            .take()
            .ok_or_else(|| SandboxError::Apply("git apply stdin unavailable".into()))?
            .write_all(patch)?;
        let output = child.wait_with_output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(SandboxError::Apply(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ))
        }
    }

    /// Restore the pinned base between bounded attempts. This command is scoped
    /// to the validated disposable worktree, never the user's checkout.
    pub fn reset_attempt(&self) -> Result<(), SandboxError> {
        git_command(
            &self.path,
            &[
                "restore",
                "--source",
                &self.base_sha,
                "--staged",
                "--worktree",
                ".",
            ],
        )?;
        git_command(&self.path, &["clean", "-fd"])
    }

    /// Commit the already-verified staged patch locally. No network or publish
    /// credential is used; the publisher pushes this commit in a later stage.
    pub fn commit_verified(
        &self,
        title: &str,
        event_id: &str,
        files: &[String],
    ) -> Result<String, SandboxError> {
        if files.is_empty() {
            return Err(SandboxError::Apply("verified patch has no files".into()));
        }
        let mut add = Command::new("git");
        add.arg("-C").arg(&self.path).args(["add", "--"]);
        add.args(files);
        let output = add.output()?;
        if !output.status.success() {
            return Err(SandboxError::Git(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args([
                "commit",
                "--no-gpg-sign",
                "-m",
                title,
                "-m",
                &format!("Synaptic-API-Event: {event_id}"),
            ])
            .env("GIT_AUTHOR_NAME", "Synaptic API Maintainer")
            .env("GIT_AUTHOR_EMAIL", "synaptic@localhost")
            .env("GIT_COMMITTER_NAME", "Synaptic API Maintainer")
            .env("GIT_COMMITTER_EMAIL", "synaptic@localhost")
            .output()?;
        if !output.status.success() {
            return Err(SandboxError::Git(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        git::rev_parse(&self.path, "HEAD").map_err(|error| SandboxError::Git(error.to_string()))
    }

    /// Remove the checkout but preserve its deterministic branch for the
    /// credential-separated publisher.
    pub fn retain_verified_branch(mut self) -> Result<String, SandboxError> {
        self.retain_branch = true;
        self.remove_worktree()?;
        Ok(self.branch.clone())
    }

    fn remove_worktree(&mut self) -> Result<(), SandboxError> {
        if self.removed {
            return Ok(());
        }
        validate_session_path(&self.repo_root, &self.path)?;
        git::worktree_remove(&self.repo_root, &self.path)
            .map_err(|error| SandboxError::Git(error.to_string()))?;
        self.removed = true;
        Ok(())
    }

    fn cleanup(&mut self) {
        let _ = self.remove_worktree();
        if !self.retain_branch {
            let _ = git_command(&self.repo_root, &["branch", "-D", &self.branch]);
        }
    }
}

impl Drop for RepairSession {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn safe_component(value: &str) -> Result<String, SandboxError> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty()
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(SandboxError::Git(format!(
            "unsafe repair identity {value:?}"
        )));
    }
    Ok(value)
}

fn validate_session_path(repo_root: &Path, path: &Path) -> Result<(), SandboxError> {
    let expected = repo_root
        .join("synaptic-out")
        .join("api-maintenance")
        .join("worktrees");
    if !path.starts_with(&expected) || path == expected {
        return Err(SandboxError::Git(format!(
            "repair worktree path escaped its root: {}",
            path.display()
        )));
    }
    Ok(())
}

fn git_command(directory: &Path, args: &[&str]) -> Result<(), SandboxError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(SandboxError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_run(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        git_run(root.path(), &["init", "-q"]);
        std::fs::write(root.path().join("client.txt"), "base\n").unwrap();
        git_run(root.path(), &["add", "-A"]);
        git_run(
            root.path(),
            &["commit", "-q", "-m", "base", "--no-gpg-sign"],
        );
        root
    }

    #[test]
    fn dirty_checkout_is_untouched_and_failure_cleans_branch_and_worktree() {
        let repo = repository();
        std::fs::write(repo.path().join("client.txt"), "dirty user work\n").unwrap();
        let worktree;
        let branch;
        {
            let session = RepairSession::create(repo.path(), "HEAD", "acme", "event_123").unwrap();
            worktree = session.path().to_path_buf();
            branch = session.branch().to_string();
            assert_eq!(
                std::fs::read_to_string(session.path().join("client.txt"))
                    .unwrap()
                    .trim_end(),
                "base"
            );
            std::fs::write(session.path().join("client.txt"), "agent edit\n").unwrap();
        }
        assert!(!worktree.exists());
        assert_eq!(
            std::fs::read_to_string(repo.path().join("client.txt")).unwrap(),
            "dirty user work\n"
        );
        let branches = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["branch", "--list", &branch])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&branches.stdout).trim().is_empty());
    }

    #[test]
    fn panic_cleans_up_and_verified_handoff_retains_only_branch() {
        let repo = repository();
        let panic_path = std::sync::Mutex::new(None);
        let _ = std::panic::catch_unwind(|| {
            let session =
                RepairSession::create(repo.path(), "HEAD", "acme", "event_panic").unwrap();
            *panic_path.lock().unwrap() = Some(session.path().to_path_buf());
            panic!("fixture");
        });
        assert!(!panic_path.lock().unwrap().as_ref().unwrap().exists());

        let session = RepairSession::create(repo.path(), "HEAD", "acme", "event_ok").unwrap();
        let path = session.path().to_path_buf();
        let branch = session.retain_verified_branch().unwrap();
        assert!(!path.exists());
        let branches = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["branch", "--list", &branch])
            .output()
            .unwrap();
        assert!(!String::from_utf8_lossy(&branches.stdout).trim().is_empty());
    }
}
