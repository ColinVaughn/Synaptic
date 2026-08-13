//! Scale benchmark: clone pinned external repositories at fixed SHAs and measure
//! `extract` throughput across size tiers and language families.
//!
//! For each repo it reports raw samples and summaries: an AST-cache-**cold** full
//! build, a cache-**warm** full build, and unchanged-file **incremental** path
//! overhead, plus files, lines, and graph nodes/edges. The renderer calls the
//! nearest-rank tail sample `max` below 20 repetitions and `p95` otherwise.
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

use synaptic_core::GraphData;
use synaptic_incremental::{ChangeSet, RebuildOptions, rebuild, topology};

/// One pinned external repository.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ScaleRepo {
    pub url: String,
    pub sha: String,
    pub family: String,
    /// Size tier: "small" | "medium" | "large" (advisory grouping label).
    pub tier: String,
    /// Representative primary-language file used for incremental-path timing.
    pub incremental_file: Option<String>,
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
    pub synaptic_version: String,
    pub source_revision: Option<String>,
    pub source_dirty: Option<bool>,
}

impl ScaleEnv {
    pub fn detect() -> Self {
        ScaleEnv {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            logical_cpus: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(0),
            synaptic_version: env!("CARGO_PKG_VERSION").to_string(),
            source_revision: git_output(
                &["rev-parse", "HEAD"],
                Some(Path::new(env!("CARGO_MANIFEST_DIR"))),
            )
            .ok(),
            source_dirty: git_output(
                &["status", "--porcelain"],
                Some(Path::new(env!("CARGO_MANIFEST_DIR"))),
            )
            .ok()
            .map(|output| !output.is_empty()),
        }
    }
}

/// One measured repository.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScaleResult {
    pub name: String,
    pub url: String,
    pub sha: String,
    pub family: String,
    pub tier: String,
    pub files: usize,
    pub lines: usize,
    pub nodes: usize,
    pub edges: usize,
    pub reps: usize,
    /// Raw samples are retained so published summaries can be audited.
    pub cold_secs: Vec<f64>,
    pub warm_secs: Vec<f64>,
    pub incremental_secs: Vec<f64>,
    pub cold_secs_median: f64,
    pub cold_secs_p95: f64,
    pub warm_secs_median: f64,
    pub warm_secs_p95: f64,
    /// Median overhead of re-extracting one unchanged file through the incremental path.
    pub incremental_secs_median: f64,
    /// Deterministically selected unchanged file re-extracted by the incremental sample.
    pub incremental_file: Option<String>,
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
    git_output(args, cwd).map(|_| ())
}

fn git_output(args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    let mut cmd = Command::new("git");
    if let Some(dir) = cwd {
        cmd.arg("-C").arg(dir);
    }
    let out = cmd
        .args(args)
        .output()
        .map_err(|e| format!("running git {args:?}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
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

/// Remove Synaptic's build cache. This does not clear the OS filesystem cache.
fn clear_cache(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir.join("synaptic-out"));
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

const PROGRAMMING_EXTENSIONS: &[&str] = &[
    "py", "ts", "tsx", "js", "jsx", "go", "rs", "java", "groovy", "cpp", "cc", "cxx", "c", "h",
    "hpp", "hh", "rb", "swift", "kt", "kts", "cs", "scala", "php", "lua", "zig", "ps1", "ex",
    "exs", "m", "mm", "jl", "dart", "v", "sql", "f", "f90", "f95", "f03", "f08", "for", "sh",
    "bash", "ql", "qll",
];

fn is_programming_source(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| PROGRAMMING_EXTENSIONS.contains(&ext))
}

/// Measure one repo over `reps` repetitions.
fn measure(dir: &Path, repo: &ScaleRepo, reps: usize) -> Result<ScaleResult, String> {
    let reps = reps.max(1);
    let mut cold = Vec::with_capacity(reps);
    let mut warm = Vec::with_capacity(reps);
    let mut last_gd: Option<GraphData> = None;

    for _ in 0..reps {
        clear_cache(dir); // AST-cache-cold; the OS filesystem cache may be warm
        let t = Instant::now();
        let gd = build_full(dir)?;
        cold.push(t.elapsed().as_secs_f64());

        let t = Instant::now();
        let _ = build_full(dir)?; // cache now hot
        warm.push(t.elapsed().as_secs_f64());
        last_gd = Some(gd);
    }
    let gd = last_gd.expect("at least one rep");

    // Incremental: re-extract one deterministic, unchanged code file against the prior graph.
    let mut incr = Vec::with_capacity(reps);
    let incremental_file = if let Some(path) = &repo.incremental_file {
        if !dir.join(path).is_file() || !gd.nodes.iter().any(|node| node.source_file == *path) {
            return Err(format!(
                "configured incremental file was not extracted: {path}"
            ));
        }
        Some(path.clone())
    } else {
        gd.nodes
            .iter()
            .map(|n| n.source_file.clone())
            .filter(|s| is_programming_source(s) && dir.join(s).is_file())
            .min()
    };
    if let Some(one) = &incremental_file {
        let path = PathBuf::from(&one);
        for _ in 0..reps {
            let t = Instant::now();
            let outcome = rebuild(
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
            if outcome.reextracted != 1 || !outcome.unreadable.is_empty() || outcome.changed {
                let (before_nodes, before_edges) = topology(&gd);
                let after = outcome.kg.to_graph_data();
                let (after_nodes, after_edges) = topology(&after);
                let before_nodes = before_nodes
                    .into_iter()
                    .collect::<std::collections::BTreeSet<_>>();
                let after_nodes = after_nodes
                    .into_iter()
                    .collect::<std::collections::BTreeSet<_>>();
                let before_edges = before_edges
                    .into_iter()
                    .collect::<std::collections::BTreeSet<_>>();
                let after_edges = after_edges
                    .into_iter()
                    .collect::<std::collections::BTreeSet<_>>();
                return Err(format!(
                    "unchanged incremental validation failed for {one}: reextracted={}, unreadable={}, topology_changed={}; first node -/+ {:?}/{:?}; first edge -/+ {:?}/{:?}",
                    outcome.reextracted,
                    outcome.unreadable.len(),
                    outcome.changed,
                    before_nodes.difference(&after_nodes).next(),
                    after_nodes.difference(&before_nodes).next(),
                    before_edges.difference(&after_edges).next(),
                    after_edges.difference(&before_edges).next(),
                ));
            }
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
        url: repo.url.clone(),
        sha: repo.sha.clone(),
        family: repo.family.clone(),
        tier: repo.tier.clone(),
        files,
        lines: count_lines(dir, &gd),
        nodes: gd.nodes.len(),
        edges: gd.links.len(),
        reps,
        cold_secs: cold.clone(),
        warm_secs: warm.clone(),
        incremental_secs: incr.clone(),
        cold_secs_median: median(&cold),
        cold_secs_p95: p95(&cold),
        warm_secs_median: median(&warm),
        warm_secs_p95: p95(&warm),
        incremental_secs_median: median(&incr),
        incremental_file,
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
        if let Some(t) = tier_filter
            && repo.tier != t
        {
            continue;
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
incremental_file = "src/lib.rs"
"#;
        let m = ScaleManifest::parse(src).unwrap();
        assert_eq!(m.repos.len(), 1);
        assert_eq!(m.repos[0].tier, "small");
        assert_eq!(m.repos[0].incremental_file.as_deref(), Some("src/lib.rs"));
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
            url: "https://example.com/x".into(),
            sha: "deadbeef".into(),
            family: "f".into(),
            tier: "small".into(),
            files: 10,
            lines: 100,
            nodes: 1,
            edges: 0,
            reps: 1,
            cold_secs: vec![1.0],
            warm_secs: vec![0.0],
            incremental_secs: vec![0.0],
            cold_secs_median: 1.0,
            cold_secs_p95: 1.0,
            warm_secs_median: 0.0,
            warm_secs_p95: 0.0,
            incremental_secs_median: 0.0,
            incremental_file: None,
        };
        assert_eq!(r.warm_files_per_sec(), 0.0);
        assert_eq!(r.warm_loc_per_sec(), 0.0);
    }

    #[test]
    fn env_is_populated() {
        let e = ScaleEnv::detect();
        assert!(!e.os.is_empty());
        assert!(!e.synaptic_version.is_empty());
    }

    #[test]
    fn incremental_sample_uses_programming_sources() {
        assert!(is_programming_source("src/main.rs"));
        assert!(!is_programming_source(".github/workflows/ci.yml"));
        assert!(!is_programming_source("README.md"));
    }
}
