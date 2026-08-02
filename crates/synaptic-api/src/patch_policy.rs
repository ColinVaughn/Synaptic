use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchPolicy {
    pub allowed_files: Vec<String>,
    #[serde(default)]
    pub scope_expansions: BTreeMap<String, String>,
    pub max_files: usize,
    pub max_changed_lines: usize,
    #[serde(default)]
    pub allow_workflows: bool,
    #[serde(default)]
    pub allow_generated: bool,
    #[serde(default = "enabled")]
    pub allow_dependency_files: bool,
}

const fn enabled() -> bool {
    true
}

impl Default for PatchPolicy {
    fn default() -> Self {
        Self {
            allowed_files: Vec::new(),
            scope_expansions: BTreeMap::new(),
            max_files: 12,
            max_changed_lines: 800,
            allow_workflows: false,
            allow_generated: false,
            allow_dependency_files: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchInspection {
    pub version: u32,
    pub changed_files: Vec<String>,
    pub added_lines: usize,
    pub removed_lines: usize,
    pub expansion_reasons: BTreeMap<String, String>,
}

pub fn validate_patch(
    worktree: &Path,
    patch: &str,
    policy: &PatchPolicy,
) -> Result<PatchInspection, PatchPolicyError> {
    if policy.max_files == 0 || policy.max_changed_lines == 0 {
        return Err(PatchPolicyError::Rejected(vec![
            "patch limits must be positive".into(),
        ]));
    }
    if patch.len() > 10 * 1024 * 1024 {
        return Err(PatchPolicyError::Rejected(vec![
            "patch exceeds the 10 MiB parser cap".into(),
        ]));
    }
    let root = worktree
        .canonicalize()
        .map_err(|error| PatchPolicyError::Io(error.to_string()))?;
    let mut files = BTreeSet::new();
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut violations = Vec::new();
    let mut in_hunk = false;
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            in_hunk = false;
            let parts = rest.split_whitespace().collect::<Vec<_>>();
            if parts.len() != 2 {
                violations.push("diff header paths must not be quoted or ambiguous".into());
                continue;
            }
            let old = parts[0].strip_prefix("a/");
            let new = parts[1].strip_prefix("b/");
            match (old, new) {
                (Some(old), Some(new)) => {
                    files.insert(normalize_path(if new == "/dev/null" { old } else { new }));
                }
                _ => violations
                    .push("diff header must use repository-relative a/ and b/ paths".into()),
            }
        } else if line.starts_with("@@") {
            in_hunk = true;
        } else if in_hunk && line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
            if secret_like(&line[1..]) {
                violations.push("patch introduces a secret-like value".into());
            }
        } else if in_hunk && line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
        if line == "GIT binary patch" || line.starts_with("Binary files ") {
            violations.push("binary patches are disabled".into());
        }
        if line.starts_with("new file mode 100755")
            || line.starts_with("old mode ")
            || line.starts_with("new mode ")
            || line.contains(" mode 160000")
            || line.contains(" mode 120000")
        {
            violations
                .push("executable-bit, symlink, submodule, and mode changes are disabled".into());
        }
    }
    if files.is_empty() {
        violations.push("patch contains no recognized file diff".into());
    }
    if files.len() > policy.max_files {
        violations.push(format!(
            "patch changes {} files; maximum is {}",
            files.len(),
            policy.max_files
        ));
    }
    if added.saturating_add(removed) > policy.max_changed_lines {
        violations.push(format!(
            "patch changes {} lines; maximum is {}",
            added + removed,
            policy.max_changed_lines
        ));
    }
    let allow = policy
        .allowed_files
        .iter()
        .map(|path| normalize_path(path))
        .collect::<BTreeSet<_>>();
    let expansions = policy
        .scope_expansions
        .iter()
        .map(|(path, reason)| (normalize_path(path), reason.clone()))
        .collect::<BTreeMap<_, _>>();
    for file in &files {
        if let Err(reason) = validate_repo_path(&root, file) {
            violations.push(reason);
            continue;
        }
        if is_protected(file) && !policy.allow_workflows {
            violations.push(format!(
                "protected repository file is out of policy: {file}"
            ));
        }
        if is_generated(file) && !policy.allow_generated {
            violations.push(format!("generated artifact is out of policy: {file}"));
        }
        let permitted = allow.contains(file)
            || expansions.contains_key(file)
            || (policy.allow_dependency_files && is_dependency_file(file));
        if !permitted {
            violations.push(format!("file is outside graph-derived patch scope: {file}"));
        }
        if expansions
            .get(file)
            .is_some_and(|reason| reason.trim().is_empty())
        {
            violations.push(format!(
                "scope expansion has no machine-readable reason: {file}"
            ));
        }
    }
    violations.sort();
    violations.dedup();
    if !violations.is_empty() {
        return Err(PatchPolicyError::Rejected(violations));
    }
    Ok(PatchInspection {
        version: 1,
        changed_files: files.into_iter().collect(),
        added_lines: added,
        removed_lines: removed,
        expansion_reasons: expansions
            .into_iter()
            .filter(|(file, _)| allow.contains(file) || policy.scope_expansions.contains_key(file))
            .collect(),
    })
}

fn validate_repo_path(root: &Path, file: &str) -> Result<(), String> {
    let relative = Path::new(file);
    if file.is_empty()
        || relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(format!("unsafe repository-relative path: {file}"));
    }
    let candidate = root.join(relative);
    if let Ok(metadata) = std::fs::symlink_metadata(&candidate) {
        if metadata.file_type().is_symlink() {
            return Err(format!("patch target is a symlink: {file}"));
        }
        let canonical = candidate
            .canonicalize()
            .map_err(|_| format!("cannot resolve patch path: {file}"))?;
        if !canonical.starts_with(root) {
            return Err(format!("patch path escapes worktree: {file}"));
        }
    } else {
        let parent = candidate
            .parent()
            .ok_or_else(|| format!("unsafe patch path: {file}"))?;
        let canonical = parent
            .canonicalize()
            .map_err(|_| format!("new file parent does not exist: {file}"))?;
        if !canonical.starts_with(root) {
            return Err(format!("new patch path escapes worktree: {file}"));
        }
    }
    Ok(())
}

fn normalize_path(path: &str) -> String {
    path.trim().trim_start_matches("./").replace('\\', "/")
}

fn is_protected(path: &str) -> bool {
    let lowercase = path.to_ascii_lowercase();
    lowercase.starts_with(".github/workflows/")
        || lowercase.ends_with("codeowners")
        || lowercase.contains("security.md")
        || lowercase.contains("credentials")
        || lowercase.contains(".env")
}

fn is_generated(path: &str) -> bool {
    let lowercase = path.to_ascii_lowercase();
    lowercase.starts_with("dist/")
        || lowercase.starts_with("build/")
        || lowercase.contains("/generated/")
        || lowercase.ends_with(".min.js")
}

fn is_dependency_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    matches!(
        name.as_str(),
        "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "pyproject.toml"
            | "poetry.lock"
            | "uv.lock"
            | "requirements.txt"
            | "cargo.toml"
            | "cargo.lock"
            | "go.mod"
            | "go.sum"
            | "pom.xml"
            | "gradle.lockfile"
            | "packages.lock.json"
            | "directory.packages.props"
    ) || name.ends_with(".csproj")
}

fn secret_like(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "sk_live_",
        "rk_live_",
        "ghp_",
        "github_pat_",
        "akia",
        "-----begin private key-----",
        "-----begin rsa private key-----",
        "password=",
        "client_secret=",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[derive(Debug, thiserror::Error)]
pub enum PatchPolicyError {
    #[error("patch rejected: {0:?}")]
    Rejected(Vec<String>),
    #[error("patch inspection I/O failed: {0}")]
    Io(String),
}
