//! Scale benchmark: clone pinned external repositories at fixed SHAs and measure
//! `extract` throughput across size tiers and language families.
//!
//! For each repo it reports, over several repetitions (median + p95): a **cold**
//! full build (AST cache cleared first, so cold is genuinely cold), a **warm**
//! full build (AST cache hot), and an **incremental** rebuild of a single file
//! (the steady-state edit latency), plus files, lines, and graph nodes/edges.
//! Environment (OS/arch/CPUs/version) is recorded so a published number is
//! interpretable.
//!
//! Network + git are required, so this is opt-in (never run in CI by default). A
//! repo that cannot be cloned or built is recorded in `skipped` and reported
//! prominently rather than silently dropped.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use serde::Deserialize;

use codegraph_core::GraphData;
use codegraph_incremental::{rebuild, ChangeSet, RebuildOptions};

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

/// Host/build environment, so absolute timings are interpretable.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScaleEnv {
    pub os: String,
    pub arch: String,
    pub logical_cpus: usize,
    pub codegraph_version: String,
}

impl ScaleEnv {
    pub fn detect() -> Self {
        ScaleEnv {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            logical_cpus: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(0),
            codegraph_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// One measured repository.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScaleResult {
    pub name: String,
    pub family: String,
    pub tier: String,
    pub files: usize,
    pub lines: usize,
    pub nodes: usize,
    pub edges: usize,
    pub reps: usize,
    pub cold_secs_median: f64,
    pub cold_secs_p95: f64,
    pub warm_secs_median: f64,
    pub warm_secs_p95: f64,
    /// Single-file incremental rebuild latency (median), the steady-state edit cost.
    pub incremental_secs_median: f64,
}

impl ScaleResult {
    /// Files per second on the warm (AST-cache-hot) median build.
    pub fn warm_files_per_sec(&self) -> f64 {
        if self.warm_secs_median <= 0.0 {
            0.0
        } else {
            self.files as f64 / self.warm_secs_median
        }
    }

    /// Lines of code per second on the warm median build.
    pub fn warm_loc_per_sec(&self) -> f64 {
        if self.warm_secs_median <= 0.0 {
            0.0
        } else {
            self.lines as f64 / self.warm_secs_median
        }
    }
}

/// The full report: environment, per-repo results, and prominently-recorded skips.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScaleReport {
    pub env: ScaleEnv,
    pub results: Vec<ScaleResult>,
    pub skipped: Vec<Skip>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Skip {
    pub url: String,
    pub reason: String,
}

/// Median of a sample (sorted copy; empty -> 0).
fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = v.len() / 2;
    if v.len() % 2 == 1 {
        v[mid]
    } else {
        (v[mid - 1] + v[mid]) / 2.0
    }
}

/// The value at the 95th percentile (nearest-rank; empty -> 0).
fn p95(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let rank = (((v.len() as f64) * 0.95).ceil() as usize).max(1) - 1;
    v[rank.min(v.len() - 1)]
}

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

fn ensure_checkout(cache_dir: &Path, repo: &ScaleRepo) -> Result<PathBuf, String> {
    std::fs::create_dir_all(cache_dir).map_err(|e| e.to_string())?;
    let dir = cache_dir.join(repo_name(&repo.url));
    if !dir.join(".git").exists() {
        git(
            &[
                "clone",
                "--quiet",
                "--filter=blob:none",
                &repo.url,
                dir.to_str().unwrap(),
            ],
            None,
        )?;
    }
    git(&["checkout", "--quiet", &repo.sha], Some(&dir))?;
    Ok(dir)
}

fn build_full(dir: &Path) -> Result<GraphData, String> {
    let out = rebuild(
        &RebuildOptions {
            root: dir.to_path_buf(),
            directed: true,
            force: true,
        },
        &ChangeSet::Full,
        None,
    )
    .map_err(|e| e.to_string())?;
    Ok(out.kg.to_graph_data())
}

/// Remove the build cache so the next build is genuinely cold. The AST cache and
/// graph artifacts live under `<dir>/codegraph-out`.
fn clear_cache(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir.join("codegraph-out"));
}

/// Count lines across the distinct source files that produced graph nodes.
fn count_lines(dir: &Path, gd: &GraphData) -> usize {
    let mut files: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for n in &gd.nodes {
        if !n.source_file.is_empty() {
            files.insert(n.source_file.as_str());
        }
    }
    files
        .iter()
        .filter_map(|rel| std::fs::read_to_string(dir.join(rel)).ok())
        .map(|s| s.lines().count())
        .sum()
}

/// Measure one repo over `reps` repetitions.
fn measure(dir: &Path, repo: &ScaleRepo, reps: usize) -> Result<ScaleResult, String> {
    let reps = reps.max(1);
    let mut cold = Vec::with_capacity(reps);
    let mut warm = Vec::with_capacity(reps);
    let mut last_gd: Option<GraphData> = None;

    for _ in 0..reps {
        clear_cache(dir); // genuinely cold: no AST cache from a prior build/run
        let t = Instant::now();
        let gd = build_full(dir)?;
        cold.push(t.elapsed().as_secs_f64());

        let t = Instant::now();
        let _ = build_full(dir)?; // cache now hot
        warm.push(t.elapsed().as_secs_f64());
        last_gd = Some(gd);
    }
    let gd = last_gd.expect("at least one rep");

    // Incremental: re-extract a single existing code file against the prior graph.
    let mut incr = Vec::with_capacity(reps);
    if let Some(one) = gd
        .nodes
        .iter()
        .map(|n| n.source_file.clone())
        .find(|s| !s.is_empty() && dir.join(s).is_file())
    {
        let path = PathBuf::from(&one);
        for _ in 0..reps {
            let t = Instant::now();
            let _ = rebuild(
                &RebuildOptions {
                    root: dir.to_path_buf(),
                    directed: true,
                    force: true,
                },
                &ChangeSet::Incremental(vec![path.clone()]),
                Some(&gd),
            )
            .map_err(|e| e.to_string())?;
            incr.push(t.elapsed().as_secs_f64());
        }
    }

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
        lines: count_lines(dir, &gd),
        nodes: gd.nodes.len(),
        edges: gd.links.len(),
        reps,
        cold_secs_median: median(&cold),
        cold_secs_p95: p95(&cold),
        warm_secs_median: median(&warm),
        warm_secs_p95: p95(&warm),
        incremental_secs_median: median(&incr),
    })
}

/// Clone + measure every repo in the manifest (optionally filtered to one tier),
/// over `reps` repetitions each. Failures are recorded in `skipped`, never fatal.
pub fn run_scale(
    manifest_path: &Path,
    cache_dir: &Path,
    tier_filter: Option<&str>,
    reps: usize,
) -> Result<ScaleReport, String> {
    let manifest =
        ScaleManifest::parse(&std::fs::read_to_string(manifest_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let mut results = Vec::new();
    let mut skipped = Vec::new();
    for repo in &manifest.repos {
        if let Some(t) = tier_filter {
            if repo.tier != t {
                continue;
            }
        }
        match ensure_checkout(cache_dir, repo).and_then(|dir| measure(&dir, repo, reps)) {
            Ok(r) => results.push(r),
            Err(e) => {
                eprintln!("SKIP {}: {e}", repo.url);
                skipped.push(Skip {
                    url: repo.url.clone(),
                    reason: e,
                });
            }
        }
    }
    Ok(ScaleReport {
        env: ScaleEnv::detect(),
        results,
        skipped,
    })
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
        assert_eq!(
            repo_name("https://github.com/BurntSushi/memchr.git"),
            "memchr"
        );
        assert_eq!(repo_name("https://github.com/a/b/"), "b");
    }

    #[test]
    fn median_and_p95() {
        assert_eq!(median(&[]), 0.0);
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
        assert_eq!(p95(&[1.0]), 1.0);
        assert_eq!(p95(&[1.0, 2.0, 3.0, 4.0, 5.0]), 5.0);
    }

    #[test]
    fn throughput_is_safe_when_instant() {
        let r = ScaleResult {
            name: "x".into(),
            family: "f".into(),
            tier: "small".into(),
            files: 10,
            lines: 100,
            nodes: 1,
            edges: 0,
            reps: 1,
            cold_secs_median: 1.0,
            cold_secs_p95: 1.0,
            warm_secs_median: 0.0,
            warm_secs_p95: 0.0,
            incremental_secs_median: 0.0,
        };
        assert_eq!(r.warm_files_per_sec(), 0.0);
        assert_eq!(r.warm_loc_per_sec(), 0.0);
    }

    #[test]
    fn env_is_populated() {
        let e = ScaleEnv::detect();
        assert!(!e.os.is_empty());
        assert!(!e.codegraph_version.is_empty());
    }
}
