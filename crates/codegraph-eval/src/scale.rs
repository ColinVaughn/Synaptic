//! Scale benchmark: clone pinned external repositories at fixed SHAs and measure
//! `extract` throughput (files and graph size per second) cold and warm, grouped
//! by size tier and language family.
//!
//! Network + git are required, so this is opt-in: it is never run in CI by
//! default, and a repo that cannot be reached is logged and skipped rather than
//! failing the run. The manifest pins a full SHA per repo so a measurement is
//! reproducible.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use serde::Deserialize;

use crate::corpus::build_fixture;

/// One pinned external repository.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ScaleRepo {
    pub url: String,
    pub sha: String,
    pub family: String,
    /// Size tier: "small" | "medium" | "large" (advisory grouping label).
    pub tier: String,
}

/// The scale manifest.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ScaleManifest {
    #[serde(default, rename = "repo")]
    pub repos: Vec<ScaleRepo>,
}

impl ScaleManifest {
    pub fn parse(src: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(src)
    }
}

/// One measured repository.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScaleResult {
    pub name: String,
    pub family: String,
    pub tier: String,
    pub files: usize,
    pub nodes: usize,
    pub edges: usize,
    pub cold_secs: f64,
    pub warm_secs: f64,
}

impl ScaleResult {
    /// Files extracted per second on the warm (AST-cache-hot) build.
    pub fn warm_files_per_sec(&self) -> f64 {
        if self.warm_secs <= 0.0 {
            0.0
        } else {
            self.files as f64 / self.warm_secs
        }
    }
}

/// The last path segment of a repo URL, used as the clone directory name.
fn repo_name(url: &str) -> &str {
    url.trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit('/')
        .next()
        .unwrap_or(url)
}

fn git(args: &[&str], cwd: Option<&Path>) -> Result<(), String> {
    let mut cmd = Command::new("git");
    if let Some(dir) = cwd {
        cmd.arg("-C").arg(dir);
    }
    let out = cmd
        .args(args)
        .output()
        .map_err(|e| format!("running git {args:?}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Ensure `<cache_dir>/<name>` is a clone of `repo.url` checked out at `repo.sha`.
fn ensure_checkout(cache_dir: &Path, repo: &ScaleRepo) -> Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(cache_dir).map_err(|e| e.to_string())?;
    let dir = cache_dir.join(repo_name(&repo.url));
    if !dir.join(".git").exists() {
        // Partial clone keeps the initial transfer small; blobs are fetched on
        // checkout as files are materialized.
        git(
            &["clone", "--quiet", "--filter=blob:none", &repo.url, dir.to_str().unwrap()],
            None,
        )?;
    }
    git(&["checkout", "--quiet", &repo.sha], Some(&dir))?;
    Ok(dir)
}

/// Measure one repo: cold build, then warm build (AST cache hot), plus graph size.
fn measure(dir: &Path, repo: &ScaleRepo) -> Result<ScaleResult, String> {
    let t0 = Instant::now();
    let gd = build_fixture(dir)?;
    let cold_secs = t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    let _ = build_fixture(dir)?;
    let warm_secs = t1.elapsed().as_secs_f64();

    let files = gd
        .nodes
        .iter()
        .map(|n| n.source_file.as_str())
        .filter(|s| !s.is_empty())
        .collect::<std::collections::HashSet<_>>()
        .len();

    Ok(ScaleResult {
        name: repo_name(&repo.url).to_string(),
        family: repo.family.clone(),
        tier: repo.tier.clone(),
        files,
        nodes: gd.nodes.len(),
        edges: gd.links.len(),
        cold_secs,
        warm_secs,
    })
}

/// Clone + measure every repo in the manifest (optionally filtered to one tier).
/// Unreachable or failing repos are logged to stderr and skipped, never fatal.
pub fn run_scale(
    manifest_path: &Path,
    cache_dir: &Path,
    tier_filter: Option<&str>,
) -> Result<Vec<ScaleResult>, String> {
    let manifest = ScaleManifest::parse(
        &std::fs::read_to_string(manifest_path).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let mut results = Vec::new();
    for repo in &manifest.repos {
        if let Some(t) = tier_filter {
            if repo.tier != t {
                continue;
            }
        }
        match ensure_checkout(cache_dir, repo).and_then(|dir| measure(&dir, repo)) {
            Ok(r) => results.push(r),
            Err(e) => eprintln!("skip {}: {e}", repo.url),
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scale_manifest() {
        let src = r#"
[[repo]]
url = "https://github.com/x/y"
sha = "deadbeef"
family = "systems-rust"
tier = "small"
"#;
        let m = ScaleManifest::parse(src).unwrap();
        assert_eq!(m.repos.len(), 1);
        assert_eq!(m.repos[0].tier, "small");
    }

    #[test]
    fn repo_name_is_last_segment() {
        assert_eq!(repo_name("https://github.com/BurntSushi/memchr"), "memchr");
        assert_eq!(repo_name("https://github.com/BurntSushi/memchr.git"), "memchr");
        assert_eq!(repo_name("https://github.com/a/b/"), "b");
    }

    #[test]
    fn warm_throughput_is_safe_when_instant() {
        let r = ScaleResult {
            name: "x".into(),
            family: "f".into(),
            tier: "small".into(),
            files: 10,
            nodes: 1,
            edges: 0,
            cold_secs: 1.0,
            warm_secs: 0.0,
        };
        assert_eq!(r.warm_files_per_sec(), 0.0);
    }
}
