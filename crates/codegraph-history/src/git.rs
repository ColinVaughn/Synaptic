//! Git shelling for time-travel: rev resolution, worktrees, numstat.

use std::path::Path;
use std::process::Command;

use crate::HistoryError;

/// Strip the Windows `\\?\` verbatim prefix git refuses to accept.
fn deverbatim(p: &Path) -> String {
    let s = p.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

fn run(repo_root: &Path, args: &[&str]) -> Result<String, HistoryError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .map_err(|e| HistoryError::Git(format!("spawning git: {e}")))?;
    if !out.status.success() {
        return Err(HistoryError::Git(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Resolve a revision to a full commit SHA (errors if it is not a commit).
pub fn rev_parse(repo_root: &Path, rev: &str) -> Result<String, HistoryError> {
    // `^{commit}` forces commit resolution; reject a bare tree/blob.
    run(
        repo_root,
        &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
    )
}

/// The most recent commit on `HEAD` at or before `date` (any git date spec, e.g.
/// `2026-01-01` or `3 weeks ago`). Errors if no commit precedes the date.
pub fn rev_before(repo_root: &Path, date: &str) -> Result<String, HistoryError> {
    let before = format!("--before={date}");
    let sha = run(repo_root, &["rev-list", "-1", &before, "HEAD"])?;
    if sha.is_empty() {
        return Err(HistoryError::Git(format!(
            "no commit on HEAD before {date}"
        )));
    }
    Ok(sha)
}

/// Create a detached worktree of `sha` at `dest`. `dest` must not already exist.
pub fn worktree_add(repo_root: &Path, dest: &Path, sha: &str) -> Result<(), HistoryError> {
    let dest_s = deverbatim(dest);
    run(
        repo_root,
        &["worktree", "add", "--detach", "--force", &dest_s, sha],
    )?;
    Ok(())
}

/// Remove a worktree previously added at `dest`.
pub fn worktree_remove(repo_root: &Path, dest: &Path) -> Result<(), HistoryError> {
    let dest_s = deverbatim(dest);
    run(repo_root, &["worktree", "remove", "--force", &dest_s])?;
    Ok(())
}

/// Prune stale worktree administrative entries (best-effort).
pub fn worktree_prune(repo_root: &Path) {
    let _ = run(repo_root, &["worktree", "prune"]);
}

/// Per-file `(added, removed, path)` between `rev1` and `rev2` (or working tree
/// when `rev2` is `None`). Binary files (`-`) count as 0.
pub fn numstat(
    repo_root: &Path,
    rev1: &str,
    rev2: Option<&str>,
) -> Result<Vec<(usize, usize, String)>, HistoryError> {
    // `--no-renames`: report a rename as a delete + add of plain paths, so the
    // path column is always a bare path (never the `{old => new}` form), which we
    // can match directly against node `source_file`s.
    let mut args = vec!["diff", "--numstat", "--no-color", "--no-renames", rev1];
    if let Some(r2) = rev2 {
        args.push(r2);
    }
    let out = run(repo_root, &args)?;
    let mut rows = Vec::new();
    for line in out.lines() {
        let mut parts = line.splitn(3, '\t');
        let (a, d, p) = match (parts.next(), parts.next(), parts.next()) {
            (Some(a), Some(d), Some(p)) => (a, d, p),
            _ => continue,
        };
        let added = a.parse::<usize>().unwrap_or(0);
        let removed = d.parse::<usize>().unwrap_or(0);
        rows.push((added, removed, p.to_string()));
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
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
    fn rev_parse_resolves_head() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git_run(root, &["init", "-q"]);
        std::fs::write(root.join("a.txt"), b"hi").unwrap();
        git_run(root, &["add", "-A"]);
        git_run(root, &["commit", "-q", "-m", "c1", "--no-gpg-sign"]);
        let sha = rev_parse(root, "HEAD").unwrap();
        assert_eq!(sha.len(), 40, "full sha");
    }

    #[test]
    fn rev_before_resolves_a_dated_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git_run(root, &["init", "-q"]);
        std::fs::write(root.join("a.txt"), b"one\n").unwrap();
        git_run(root, &["add", "-A"]);
        // rev-list --before filters by COMMIT date, so pin the committer date.
        let ok = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["commit", "-q", "-m", "old", "--no-gpg-sign"])
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00")
            .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00")
            .output()
            .expect("git")
            .status
            .success();
        assert!(ok, "commit");
        // A commit dated 2020 is reachable from a 2021 cutoff.
        let sha = rev_before(root, "2021-01-01").unwrap();
        assert_eq!(sha.len(), 40, "full sha");
        // No commit exists before 2019.
        assert!(rev_before(root, "2019-01-01").is_err());
    }

    #[test]
    fn numstat_reports_changed_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git_run(root, &["init", "-q"]);
        std::fs::write(root.join("a.txt"), b"one\n").unwrap();
        git_run(root, &["add", "-A"]);
        git_run(root, &["commit", "-q", "-m", "c1", "--no-gpg-sign"]);
        let r1 = rev_parse(root, "HEAD").unwrap();
        std::fs::write(root.join("a.txt"), b"one\ntwo\n").unwrap();
        git_run(root, &["add", "-A"]);
        git_run(root, &["commit", "-q", "-m", "c2", "--no-gpg-sign"]);
        let r2 = rev_parse(root, "HEAD").unwrap();
        let stats = numstat(root, &r1, Some(&r2)).unwrap();
        assert!(stats.iter().any(|(_, _, p)| p == "a.txt"));
    }
}
