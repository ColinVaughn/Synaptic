//! Git hook install/uninstall/status + merge-driver registration.
//!
//! Hooks call the native `codegraph` binary directly. `post-commit` runs an
//! incremental update on the commit's changed files (backgrounded so it never
//! blocks the commit); `post-checkout` runs a full rebuild on a branch switch.
//! Both are marker-guarded for idempotent install/uninstall and skip during
//! rebase/merge/cherry-pick and when only `codegraph-out/` changed (anti-loop).
//! `hook install` also registers the `graph.json` merge driver via
//! `.gitattributes` + git config.

use std::path::{Path, PathBuf};
use std::process::Command;

const MARKER_START: &str = "# >>> codegraph hook >>>";
const MARKER_END: &str = "# <<< codegraph hook <<<";
const HOOKS: &[&str] = &["post-commit", "post-checkout"];

/// Per-hook install/uninstall state for reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookState {
    pub name: String,
    pub installed: bool,
}

/// Errors from hook operations.
#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("not a git repository (or git unavailable): {0}")]
    NotGit(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

fn git(repo_root: &Path, args: &[&str]) -> Result<String, HookError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .map_err(|e| HookError::NotGit(e.to_string()))?;
    if !out.status.success() {
        return Err(HookError::NotGit(format!("git {} failed", args.join(" "))));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_config_get(repo_root: &Path, key: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if out.status.success() {
        let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!v.is_empty()).then_some(v)
    } else {
        None
    }
}

/// Resolve the git repository top-level containing `start`.
pub fn repo_root(start: &Path) -> Result<PathBuf, HookError> {
    Ok(PathBuf::from(git(
        start,
        &["rev-parse", "--show-toplevel"],
    )?))
}

/// Lexically resolve `.`/`..` without touching the filesystem (so a `..` is
/// canceled rather than left as a component the ancestor-walk could bounce off).
fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `candidate` resolves to a location inside `repo_root`. First resolves
/// `..`/`.` lexically (so `repo/../evil` doesn't bounce back into the repo via
/// `.parent()`), then compares canonicalized paths (resolving symlinks + Windows
/// `\\?\`/8.3 forms). Since `candidate` may not exist yet (e.g. a Husky dir), it
/// canonicalizes the nearest existing ancestor. Rejects an escaping `core.hooksPath`.
fn is_within_repo(repo_root: &Path, candidate: &Path) -> bool {
    let Ok(root) = repo_root.canonicalize() else {
        return false;
    };
    let norm = normalize_lexical(candidate);
    let mut probe = norm.as_path();
    loop {
        if let Ok(real) = probe.canonicalize() {
            return real.starts_with(&root);
        }
        match probe.parent() {
            Some(p) if p != probe => probe = p,
            _ => return false,
        }
    }
}

/// The default git hooks dir: the common git dir (shared across worktrees) +
/// `hooks`. Unlike `--git-path hooks`, `--git-common-dir` does NOT honor
/// `core.hooksPath`, so this is the correct literal fallback when a configured
/// hooksPath was rejected for escaping the repo.
fn default_hooks_dir(repo_root: &Path) -> Result<PathBuf, HookError> {
    let common = git(repo_root, &["rev-parse", "--git-common-dir"])?;
    let gd = PathBuf::from(common);
    let gd = if gd.is_absolute() {
        gd
    } else {
        repo_root.join(gd)
    };
    Ok(gd.join("hooks"))
}

/// Husky 9 points `core.hooksPath` at `.husky/_` (auto-generated wrapper scripts
/// Husky overwrites); the user-editable hooks live in the parent `.husky/`.
/// Redirect to the parent so our hook isn't clobbered.
fn user_hooks_dir(dir: PathBuf) -> PathBuf {
    if dir.file_name() == Some(std::ffi::OsStr::new("_")) {
        dir.parent().map(Path::to_path_buf).unwrap_or(dir)
    } else {
        dir
    }
}

/// Resolve the directory git looks in for hooks, honoring `core.hooksPath`
/// (Husky) and worktrees.
fn hooks_dir(repo_root: &Path) -> Result<PathBuf, HookError> {
    let base = if let Some(hp) = git_config_get(repo_root, "core.hooksPath") {
        let p = PathBuf::from(&hp);
        let abs = if p.is_absolute() {
            p
        } else {
            repo_root.join(p)
        };
        // Reject a `core.hooksPath` that escapes the repo root: a malicious
        // committed value must not redirect our hook install outside the tree
        // (supply-chain hardening).
        if is_within_repo(repo_root, &abs) {
            abs
        } else {
            default_hooks_dir(repo_root)?
        }
    } else {
        default_hooks_dir(repo_root)?
    };
    Ok(user_hooks_dir(base))
}

/// The `codegraph` binary path, forward-slashed so it's usable from git's POSIX
/// `sh` on every platform (git-for-Windows runs hooks under MSYS sh).
fn current_bin() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| "codegraph".to_string())
}

fn post_commit_script(bin: &str) -> String {
    format!(
        r#"{MARKER_START}
[ "$CODEGRAPH_SKIP_HOOK" = "1" ] && exit 0
GD=$(git rev-parse --git-dir 2>/dev/null) || exit 0
for m in rebase-merge rebase-apply MERGE_HEAD CHERRY_PICK_HEAD; do
  [ -e "$GD/$m" ] && exit 0
done
CHANGED=$(git diff --name-only HEAD~1 HEAD 2>/dev/null || git diff --name-only HEAD 2>/dev/null)
REAL=$(printf '%s\n' "$CHANGED" | grep -v '^codegraph-out/' || true)
[ -z "$REAL" ] && exit 0
# Pass changed files via an env var (newline-delimited), never as argv, so paths
# with spaces or shell-glob characters aren't word-split / glob-expanded.
export CODEGRAPH_CHANGED="$REAL"
( "{bin}" update >codegraph-out/.rebuild.log 2>&1 & )
{MARKER_END}"#
    )
}

fn post_checkout_script(bin: &str) -> String {
    format!(
        r#"{MARKER_START}
[ "$CODEGRAPH_SKIP_HOOK" = "1" ] && exit 0
[ "$3" = "1" ] || exit 0
[ -d codegraph-out ] || exit 0
( "{bin}" update --full >codegraph-out/.rebuild.log 2>&1 & )
{MARKER_END}"#
    )
}

fn script_for(hook: &str, bin: &str) -> String {
    match hook {
        "post-commit" => post_commit_script(bin),
        "post-checkout" => post_checkout_script(bin),
        _ => unreachable!("unknown hook {hook}"),
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Insert (or replace) our marker block in `existing`, returning the new content.
fn upsert_block(existing: Option<&str>, block: &str) -> String {
    match existing {
        None => format!("#!/bin/sh\n{block}\n"),
        Some(body) => {
            if let (Some(s), Some(e)) = (body.find(MARKER_START), body.find(MARKER_END)) {
                // Replace the existing block in place (idempotent upgrade).
                let end = e + MARKER_END.len();
                format!("{}{}{}", &body[..s], block, &body[end..])
            } else {
                // Append to an existing foreign hook.
                let sep = if body.ends_with('\n') { "" } else { "\n" };
                format!("{body}{sep}{block}\n")
            }
        }
    }
}

/// Remove our marker block from `body`; `None` if nothing remains but a shebang.
fn strip_block(body: &str) -> Option<String> {
    let (Some(s), Some(e)) = (body.find(MARKER_START), body.find(MARKER_END)) else {
        return Some(body.to_string()); // our block absent, leave file untouched
    };
    let end = e + MARKER_END.len();
    let mut out = format!("{}{}", &body[..s], &body[end..]);
    // Trim trailing whitespace lines left behind.
    while out.ends_with('\n') || out.ends_with(' ') {
        out.pop();
    }
    let trimmed = out.trim();
    if trimmed.is_empty() || trimmed == "#!/bin/sh" {
        None // nothing meaningful left, so caller removes the file
    } else {
        Some(format!("{out}\n"))
    }
}

/// Install the post-commit + post-checkout hooks and register the merge driver.
/// Idempotent: re-running replaces our block in place.
pub fn install(repo_root: &Path) -> Result<Vec<HookState>, HookError> {
    let dir = hooks_dir(repo_root)?;
    std::fs::create_dir_all(&dir)?;
    let bin = current_bin();
    let mut states = Vec::new();
    for &hook in HOOKS {
        let path = dir.join(hook);
        let existing = std::fs::read_to_string(&path).ok();
        let content = upsert_block(existing.as_deref(), &script_for(hook, &bin));
        std::fs::write(&path, content)?;
        make_executable(&path)?;
        states.push(HookState {
            name: hook.to_string(),
            installed: true,
        });
    }
    register_merge_driver(repo_root, &bin)?;
    Ok(states)
}

/// Remove our hook blocks (and the hook files if nothing else remains).
pub fn uninstall(repo_root: &Path) -> Result<Vec<HookState>, HookError> {
    let dir = hooks_dir(repo_root)?;
    let mut states = Vec::new();
    for &hook in HOOKS {
        let path = dir.join(hook);
        if let Ok(body) = std::fs::read_to_string(&path) {
            match strip_block(&body) {
                None => {
                    let _ = std::fs::remove_file(&path);
                }
                Some(rest) => std::fs::write(&path, rest)?,
            }
        }
        states.push(HookState {
            name: hook.to_string(),
            installed: false,
        });
    }
    Ok(states)
}

/// Report which hooks currently contain our marker block.
pub fn status(repo_root: &Path) -> Result<Vec<HookState>, HookError> {
    let dir = hooks_dir(repo_root)?;
    Ok(HOOKS
        .iter()
        .map(|&hook| {
            let installed = std::fs::read_to_string(dir.join(hook))
                .map(|b| b.contains(MARKER_START))
                .unwrap_or(false);
            HookState {
                name: hook.to_string(),
                installed,
            }
        })
        .collect())
}

/// Register the graph.json merge driver: a `.gitattributes` line + git config so
/// `codegraph merge-driver` union-merges `graph.json` (no textual conflicts).
fn register_merge_driver(repo_root: &Path, bin: &str) -> Result<(), HookError> {
    // .gitattributes line (idempotent).
    let attrs_path = repo_root.join(".gitattributes");
    let line = "codegraph-out/graph.json merge=codegraph";
    // Treat a missing file as empty, but DO NOT swallow a real read error
    // (e.g. permissions); that would clobber an existing .gitattributes we
    // just couldn't read.
    let existing = match std::fs::read_to_string(&attrs_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(HookError::Io(e)),
    };
    if !existing.lines().any(|l| l.trim() == line) {
        let sep = if existing.is_empty() || existing.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        std::fs::write(&attrs_path, format!("{existing}{sep}{line}\n"))?;
    }
    // git config: name + driver invocation.
    let _ = git(
        repo_root,
        &[
            "config",
            "merge.codegraph.name",
            "CodeGraph graph.json union merge",
        ],
    )?;
    let _ = git(
        repo_root,
        &[
            "config",
            "merge.codegraph.driver",
            &format!("\"{bin}\" merge-driver %O %A %B"),
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .arg("init")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git init failed — git must be on PATH for this test");
        dir
    }

    #[test]
    fn install_is_idempotent_and_registers_merge_driver() {
        let repo = init_repo();
        let root = repo.path();

        let s1 = install(root).unwrap();
        assert!(s1.iter().all(|h| h.installed));
        // Both hook files exist with our marker.
        let dir = hooks_dir(root).unwrap();
        for h in HOOKS {
            let body = std::fs::read_to_string(dir.join(h)).unwrap();
            assert!(body.contains(MARKER_START), "{h} has marker");
            assert!(body.contains("codegraph"), "{h} invokes codegraph");
        }
        // .gitattributes registers the merge driver.
        let attrs = std::fs::read_to_string(root.join(".gitattributes")).unwrap();
        assert!(attrs.contains("codegraph-out/graph.json merge=codegraph"));
        assert!(
            git_config_get(root, "merge.codegraph.driver")
                .unwrap()
                .contains("merge-driver"),
            "merge driver registered in git config"
        );

        // Re-install: still exactly one marker block per hook (idempotent).
        install(root).unwrap();
        for h in HOOKS {
            let body = std::fs::read_to_string(dir.join(h)).unwrap();
            assert_eq!(body.matches(MARKER_START).count(), 1, "{h}: one block");
        }
    }

    #[test]
    fn install_appends_to_an_existing_hook_and_uninstall_restores_it() {
        let repo = init_repo();
        let root = repo.path();
        let dir = hooks_dir(root).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        // Pre-existing foreign post-commit hook.
        let pc = dir.join("post-commit");
        std::fs::write(&pc, "#!/bin/sh\necho existing-hook\n").unwrap();

        install(root).unwrap();
        let body = std::fs::read_to_string(&pc).unwrap();
        assert!(body.contains("echo existing-hook"), "foreign content kept");
        assert!(body.contains(MARKER_START), "our block appended");

        uninstall(root).unwrap();
        let body = std::fs::read_to_string(&pc).unwrap();
        assert!(
            body.contains("echo existing-hook"),
            "foreign content survives uninstall"
        );
        assert!(!body.contains(MARKER_START), "our block removed");
    }

    #[test]
    fn uninstall_removes_a_hook_we_solely_created() {
        let repo = init_repo();
        let root = repo.path();
        install(root).unwrap();
        assert!(status(root).unwrap().iter().all(|h| h.installed));
        uninstall(root).unwrap();
        assert!(status(root).unwrap().iter().all(|h| !h.installed));
        // A hook we created (no foreign content) is removed entirely.
        let dir = hooks_dir(root).unwrap();
        assert!(
            !dir.join("post-checkout").exists(),
            "solely-ours hook removed"
        );
    }

    #[test]
    fn husky_underscore_dir_redirects_to_parent() {
        // Husky 9's .husky/_ wrapper dir: user hooks live in the parent.
        let p = user_hooks_dir(PathBuf::from("/repo/.husky/_"));
        assert_eq!(p, PathBuf::from("/repo/.husky"));
        // A normal hooks dir is returned unchanged.
        let n = user_hooks_dir(PathBuf::from("/repo/.git/hooks"));
        assert_eq!(n, PathBuf::from("/repo/.git/hooks"));
    }

    #[test]
    fn hookspath_escape_is_rejected() {
        let repo = init_repo();
        let root = repo.path();
        // A malicious core.hooksPath escaping the repo (the realistic vector: a
        // relative `../` path) must be ignored; hooks_dir falls back to the
        // default inside the repo.
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["config", "core.hooksPath", "../evil-hooks"])
            .output()
            .unwrap();
        let dir = hooks_dir(root).unwrap();
        assert!(
            is_within_repo(root, &dir),
            "escaping hooksPath must fall back inside the repo, got {dir:?}"
        );
        assert!(
            !dir.to_string_lossy().contains("evil-hooks"),
            "must not use the escaping path, got {dir:?}"
        );
    }

    #[test]
    fn upsert_and_strip_round_trip() {
        let block = "# >>> codegraph hook >>>\necho hi\n# <<< codegraph hook <<<";
        let created = upsert_block(None, block);
        assert!(created.starts_with("#!/bin/sh\n"));
        assert!(created.contains(MARKER_START));
        // Idempotent replace.
        let replaced = upsert_block(Some(&created), block);
        assert_eq!(replaced.matches(MARKER_START).count(), 1);
        // Strip a solely-ours file returns None.
        assert!(strip_block(&created).is_none());
    }
}
