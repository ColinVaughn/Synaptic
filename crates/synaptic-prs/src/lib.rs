//! PR intelligence — a graph-aware PR dashboard.
//!
//! Pure logic (classification, CI rollup parsing, blast-radius over the graph)
//! takes plain primitives, so this crate depends only on `serde_json` (to read
//! `gh`'s JSON) + `thiserror`; the caller adapts its `KnowledgeGraph` nodes into
//! the blast-radius iterators. `gh`/`git` are reached through a [`CommandRunner`]
//! trait so tests can feed canned output. Consumed by the CLI (`synaptic prs`)
//! and (C3) the MCP server's `list_prs`/`get_pr_impact`/`triage_prs` tools.
//!
//! Deferred (C2e): `--worktrees`/`--conflicts` views, LLM `--triage`, and a REST
//! `gh`-free provider.
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

// dates (no chrono: day-granular, which is all `days_old`/STALE need)

/// Days from the civil date `y-m-d` to 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // Mar=0..Feb=11
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Parse the `YYYY-MM-DD` prefix of an ISO-8601 timestamp into epoch-days.
/// Unparseable → 0 (epoch), which just makes a PR look maximally old.
fn iso_date_to_epoch_days(ts: &str) -> i64 {
    let date = ts.split(['T', ' ']).next().unwrap_or("");
    let mut parts = date.split('-');
    let y = parts.next().and_then(|s| s.parse::<i64>().ok());
    let m = parts.next().and_then(|s| s.parse::<i64>().ok());
    let d = parts.next().and_then(|s| s.parse::<i64>().ok());
    match (y, m, d) {
        (Some(y), Some(m), Some(d)) if (1..=12).contains(&m) && (1..=31).contains(&d) => {
            days_from_civil(y, m, d)
        }
        _ => 0,
    }
}

/// Today as epoch-days (UTC). Pass to [`PrInfo::classify`]/[`PrInfo::days_old`]
/// (inject a fixed value in tests).
pub fn today_epoch_days() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86_400) as i64)
        .unwrap_or(0)
}

// CI status

/// Aggregate CI state of a PR's check rollup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiStatus {
    Success,
    Failure,
    Pending,
    None,
}

impl CiStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CiStatus::Success => "SUCCESS",
            CiStatus::Failure => "FAILURE",
            CiStatus::Pending => "PENDING",
            CiStatus::None => "NONE",
        }
    }
}

/// Conclusions that count as a CI failure.
const CI_FAILURE_CONCLUSIONS: &[&str] = &[
    "FAILURE",
    "CANCELLED",
    "TIMED_OUT",
    "ACTION_REQUIRED",
    "STARTUP_FAILURE",
];

/// Collapse a `statusCheckRollup` array into one [`CiStatus`]:
/// any failing conclusion → Failure; else any in-progress/queued → Pending; else
/// a SUCCESS conclusion → Success; else None.
pub fn parse_ci(rollup: &[Value]) -> CiStatus {
    if rollup.is_empty() {
        return CiStatus::None;
    }
    let conclusions: HashSet<&str> = rollup
        .iter()
        .filter_map(|r| r.get("conclusion").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .collect();
    if conclusions
        .iter()
        .any(|c| CI_FAILURE_CONCLUSIONS.contains(c))
    {
        return CiStatus::Failure;
    }
    let statuses: HashSet<&str> = rollup
        .iter()
        .filter_map(|r| r.get("status").and_then(Value::as_str))
        .collect();
    if statuses.contains("IN_PROGRESS") || statuses.contains("QUEUED") {
        return CiStatus::Pending;
    }
    if conclusions.contains("SUCCESS") {
        return CiStatus::Success;
    }
    CiStatus::None
}

// classification

const STALE_DAYS: i64 = 14;

/// A PR's triage status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    WrongBase,
    CiFail,
    ChangesReq,
    Draft,
    Stale,
    Approved,
    Pending,
    Ready,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::WrongBase => "WRONG-BASE",
            Status::CiFail => "CI-FAIL",
            Status::ChangesReq => "CHANGES-REQ",
            Status::Draft => "DRAFT",
            Status::Stale => "STALE",
            Status::Approved => "APPROVED",
            Status::Pending => "PENDING",
            Status::Ready => "READY",
        }
    }

    /// Dashboard sort rank. Note this differs from the
    /// classify precedence: PENDING sorts before APPROVED.
    pub fn sort_rank(self) -> u8 {
        match self {
            Status::WrongBase => 0,
            Status::CiFail => 1,
            Status::ChangesReq => 2,
            Status::Draft => 3,
            Status::Stale => 4,
            Status::Pending => 5,
            Status::Approved => 6,
            Status::Ready => 7,
        }
    }
}

// PR model

/// One open PR with CI/review state and (optionally) graph blast radius.
#[derive(Debug, Clone)]
pub struct PrInfo {
    pub number: u64,
    pub title: String,
    pub branch: String,
    pub base_branch: String,
    pub author: String,
    pub is_draft: bool,
    /// `APPROVED` | `CHANGES_REQUESTED` | "".
    pub review_decision: String,
    pub ci_status: CiStatus,
    /// `updatedAt` as epoch-days.
    pub updated_epoch_days: i64,
    /// The base this PR *should* target (the repo default branch, or `--base`).
    pub expected_base: String,
    pub worktree_path: Option<String>,
    pub communities_touched: Vec<u32>,
    pub nodes_affected: usize,
    pub files_changed: Vec<String>,
}

impl PrInfo {
    /// Whole days since the PR was last updated (clamped at 0).
    pub fn days_old(&self, now_epoch_days: i64) -> i64 {
        (now_epoch_days - self.updated_epoch_days).max(0)
    }

    /// Triage status as of `now_epoch_days` (precedence:
    /// WRONG-BASE > CI-FAIL > CHANGES-REQ > DRAFT > STALE > APPROVED > PENDING > READY).
    pub fn classify(&self, now_epoch_days: i64) -> Status {
        if self.base_branch != self.expected_base {
            Status::WrongBase
        } else if self.ci_status == CiStatus::Failure {
            Status::CiFail
        } else if self.review_decision == "CHANGES_REQUESTED" {
            Status::ChangesReq
        } else if self.is_draft {
            Status::Draft
        } else if self.days_old(now_epoch_days) >= STALE_DAYS {
            Status::Stale
        } else if self.review_decision == "APPROVED" {
            Status::Approved
        } else if self.ci_status == CiStatus::Pending {
            Status::Pending
        } else {
            Status::Ready
        }
    }

    /// `"N nodes / M communities"`, or "" when no impact was computed.
    pub fn blast_radius(&self) -> String {
        if self.nodes_affected == 0 {
            return String::new();
        }
        let n = self.nodes_affected;
        let c = self.communities_touched.len();
        format!(
            "{n} node{} / {c} communit{}",
            if n != 1 { "s" } else { "" },
            if c != 1 { "ies" } else { "y" }
        )
    }
}

/// Build a [`PrInfo`] from one gh JSON object. `number` is taken from the JSON
/// when present, else from `number_override` (gh `pr view` omits it).
fn pr_from_json(v: &Value, expected_base: &str, number_override: Option<u64>) -> PrInfo {
    let rollup = v
        .get("statusCheckRollup")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    PrInfo {
        number: v
            .get("number")
            .and_then(Value::as_u64)
            .or(number_override)
            .unwrap_or(0),
        title: v
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        branch: v
            .get("headRefName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        base_branch: v
            .get("baseRefName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        author: v
            .get("author")
            .and_then(|a| a.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string(),
        is_draft: v.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        review_decision: v
            .get("reviewDecision")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        ci_status: parse_ci(&rollup),
        updated_epoch_days: iso_date_to_epoch_days(
            v.get("updatedAt").and_then(Value::as_str).unwrap_or(""),
        ),
        expected_base: expected_base.to_string(),
        worktree_path: None,
        communities_touched: Vec::new(),
        nodes_affected: 0,
        files_changed: Vec::new(),
    }
}

// command runner (gh / git)

/// Runs an external command, returning its stdout on success (rc==0). Injected
/// so tests can supply canned `gh`/`git` output without a real CLI.
pub trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[&str]) -> Option<String>;
}

/// The real runner — shells out via [`std::process::Command`].
pub struct SystemCommands;

impl CommandRunner for SystemCommands {
    fn run(&self, program: &str, args: &[&str]) -> Option<String> {
        let out = std::process::Command::new(program)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

fn gh_json(r: &dyn CommandRunner, args: &[&str]) -> Option<Value> {
    let stdout = r.run("gh", args)?;
    serde_json::from_str(&stdout).ok()
}

/// Errors from PR fetching.
#[derive(Debug, thiserror::Error)]
pub enum PrError {
    #[error("gh CLI not found or not authenticated (run: gh auth login). PR data is skipped; graph audit continues offline.")]
    GhUnavailable,
}

/// Detect the repo's default branch: gh `repo view` → `git symbolic-ref` → "main".
pub fn detect_default_branch(r: &dyn CommandRunner, repo: Option<&str>) -> String {
    let mut args = vec!["repo", "view", "--json", "defaultBranchRef"];
    if let Some(repo) = repo {
        args.push("--repo");
        args.push(repo);
    }
    if let Some(v) = gh_json(r, &args) {
        if let Some(name) = v
            .get("defaultBranchRef")
            .and_then(|d| d.get("name"))
            .and_then(Value::as_str)
        {
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    if let Some(out) = r.run("git", &["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        if let Some(last) = out.trim().rsplit('/').next() {
            if !last.is_empty() {
                return last.to_string();
            }
        }
    }
    "main".to_string()
}

/// Fetch open PRs. `base` overrides the auto-detected
/// default branch. Errors if `gh` is unavailable/unauthenticated.
pub fn fetch_prs(
    r: &dyn CommandRunner,
    repo: Option<&str>,
    base: Option<&str>,
    limit: usize,
) -> Result<Vec<PrInfo>, PrError> {
    let resolved_base = base
        .map(str::to_string)
        .unwrap_or_else(|| detect_default_branch(r, repo));
    let limit = limit.to_string();
    let mut args = vec![
        "pr", "list", "--state", "open", "--limit", &limit, "--json",
        "number,title,headRefName,baseRefName,author,isDraft,reviewDecision,statusCheckRollup,updatedAt",
    ];
    if let Some(repo) = repo {
        args.push("--repo");
        args.push(repo);
    }
    let raw = gh_json(r, &args).ok_or(PrError::GhUnavailable)?;
    let arr = raw.as_array().ok_or(PrError::GhUnavailable)?;
    Ok(arr
        .iter()
        .map(|v| pr_from_json(v, &resolved_base, None))
        .collect())
}

/// Fetch a single PR via `gh pr view` (works regardless of its base).
/// `None` if gh fails.
pub fn fetch_pr(
    r: &dyn CommandRunner,
    number: u64,
    repo: Option<&str>,
    expected_base: &str,
) -> Option<PrInfo> {
    let num = number.to_string();
    let mut args =
        vec![
        "pr", "view", &num, "--json",
        "title,headRefName,baseRefName,author,isDraft,reviewDecision,statusCheckRollup,updatedAt",
    ];
    if let Some(repo) = repo {
        args.push("--repo");
        args.push(repo);
    }
    let v = gh_json(r, &args)?;
    Some(pr_from_json(&v, expected_base, Some(number)))
}

/// Changed files of a PR (`gh pr diff <n> --name-only`). Empty on failure.
pub fn fetch_pr_files(r: &dyn CommandRunner, number: u64, repo: Option<&str>) -> Vec<String> {
    let num = number.to_string();
    let mut args = vec!["pr", "diff", &num, "--name-only"];
    if let Some(repo) = repo {
        args.push("--repo");
        args.push(repo);
    }
    match r.run("gh", &args) {
        Some(out) => out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        None => Vec::new(),
    }
}

/// `{branch: worktree_path}` from `git worktree list --porcelain`.
/// A blank line resets the current path so a
/// detached HEAD doesn't leak into the next record.
pub fn fetch_worktrees(r: &dyn CommandRunner) -> BTreeMap<String, String> {
    let mut mapping = BTreeMap::new();
    let Some(out) = r.run("git", &["worktree", "list", "--porcelain"]) else {
        return mapping;
    };
    let mut current: Option<String> = None;
    for line in out.lines() {
        if line.is_empty() {
            current = None;
        } else if let Some(p) = line.strip_prefix("worktree ") {
            current = Some(p.to_string());
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            if let Some(path) = &current {
                mapping.insert(b.to_string(), path.clone());
            }
        }
    }
    mapping
}

// graph blast radius

/// True if `graph_src` and `pr_file` name the same file, path-boundary safe:
/// equal, or one ends with `"/" + other`.
pub fn path_match(graph_src: &str, pr_file: &str) -> bool {
    if graph_src == pr_file {
        return true;
    }
    // `hay` ends with `"/" + needle` iff it ends with `needle` and the byte just
    // before is `/`. Allocation-free (was two `format!`s per call, in a hot
    // nested per-PR loop). `/` is ASCII, so the byte check is UTF-8-safe. M6.
    let ends_on_boundary = |hay: &str, needle: &str| {
        hay.len() > needle.len()
            && hay.ends_with(needle)
            && hay.as_bytes()[hay.len() - needle.len() - 1] == b'/'
    };
    ends_on_boundary(graph_src, pr_file) || ends_on_boundary(pr_file, graph_src)
}

/// Precomputed `source_file → (communities, node_count)` index for PR blast
/// radius. It depends only on the graph, so build it **once** and reuse it
/// across every PR via [`impact_for_files`](ImpactIndex::impact_for_files),
/// rather than rebuilding it per PR (H5).
pub struct ImpactIndex {
    /// Distinct source files in first-seen order (deterministic match order).
    order: Vec<String>,
    /// source_file → set of communities its nodes belong to.
    comms: BTreeMap<String, HashSet<u32>>,
    /// source_file → number of graph nodes from that file.
    count: BTreeMap<String, usize>,
}

impl ImpactIndex {
    /// Build the index from each graph node's `(source_file, community)`.
    pub fn build<'a, I>(nodes: I) -> Self
    where
        I: IntoIterator<Item = (&'a str, Option<u32>)>,
    {
        let mut order: Vec<String> = Vec::new();
        let mut comms: BTreeMap<String, HashSet<u32>> = BTreeMap::new();
        let mut count: BTreeMap<String, usize> = BTreeMap::new();
        for (src, community) in nodes {
            if src.is_empty() {
                continue;
            }
            if !comms.contains_key(src) {
                order.push(src.to_string());
                comms.insert(src.to_string(), HashSet::new());
                count.insert(src.to_string(), 0);
            }
            if let Some(c) = community {
                comms.get_mut(src).expect("src inserted above").insert(c);
            }
            *count.get_mut(src).expect("src inserted above") += 1;
        }
        ImpactIndex {
            order,
            comms,
            count,
        }
    }

    /// `(communities_touched, nodes_affected)` for a set of changed `files`. A
    /// `matched` set prevents double-counting in either direction (one PR file
    /// matching several graph paths, or a graph file listed twice in the diff).
    pub fn impact_for_files(&self, files: &[String]) -> (Vec<u32>, usize) {
        let mut touched: HashSet<u32> = HashSet::new();
        let mut nodes_affected = 0usize;
        let mut matched: HashSet<&str> = HashSet::new();
        for f in files {
            for src in &self.order {
                if !matched.contains(src.as_str()) && path_match(src, f) {
                    touched.extend(self.comms[src].iter().copied());
                    nodes_affected += self.count[src];
                    matched.insert(src.as_str());
                }
            }
        }
        let mut sorted: Vec<u32> = touched.into_iter().collect();
        sorted.sort_unstable();
        (sorted, nodes_affected)
    }
}

/// `(communities_touched, nodes_affected)` for a set of changed `files`.
/// Convenience wrapper that builds a one-shot
/// [`ImpactIndex`]; callers handling many PRs should build one index and reuse
/// it via [`ImpactIndex::impact_for_files`] (H5).
pub fn compute_pr_impact<'a, I>(nodes: I, files: &[String]) -> (Vec<u32>, usize)
where
    I: IntoIterator<Item = (&'a str, Option<u32>)>,
{
    ImpactIndex::build(nodes).impact_for_files(files)
}

/// `{community → first `top_n` node labels}`.
/// `nodes` yields each node's `(label, community)`.
pub fn build_community_labels<'a, I>(nodes: I, top_n: usize) -> BTreeMap<u32, Vec<String>>
where
    I: IntoIterator<Item = (&'a str, Option<u32>)>,
{
    let mut out: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for (label, community) in nodes {
        if let Some(c) = community {
            if label.is_empty() {
                continue;
            }
            let v = out.entry(c).or_default();
            if v.len() < top_n {
                v.push(label.to_string());
            }
        }
    }
    out
}

// rendering

/// Plain-text dashboard of PRs targeting `base`, sorted by status then age.
/// `now_epoch_days` drives staleness.
pub fn format_prs_text(prs: &[PrInfo], base: &str, now_epoch_days: i64) -> String {
    let mut actionable: Vec<&PrInfo> = prs.iter().filter(|p| p.base_branch == base).collect();
    let wrong = prs.len() - actionable.len();
    actionable.sort_by_key(|p| {
        (
            p.classify(now_epoch_days).sort_rank(),
            p.days_old(now_epoch_days),
        )
    });

    let mut lines = vec![format!(
        "Open PRs targeting {base}: {}  ({wrong} on wrong base, not shown)",
        actionable.len()
    )];
    for p in actionable {
        let status = p.classify(now_epoch_days);
        let impact = if p.blast_radius().is_empty() {
            String::new()
        } else {
            format!("  blast_radius={}", p.blast_radius())
        };
        let review = if p.review_decision.is_empty() {
            "none"
        } else {
            &p.review_decision
        };
        lines.push(format!(
            "#{} [{}] CI={} review={} age={}d author={}{}\n  {}",
            p.number,
            status.as_str(),
            p.ci_status.as_str(),
            review,
            p.days_old(now_epoch_days),
            p.author,
            impact,
            p.title
        ));
    }
    lines.join("\n\n")
}

/// Multi-line detail for one PR (plain text).
pub fn format_pr_detail(pr: &PrInfo, now_epoch_days: i64, max_files: usize) -> String {
    let mut out = vec![
        format!("PR #{} — {}", pr.number, pr.title),
        format!("  {} → {}", pr.branch, pr.base_branch),
        format!("  status: {}", pr.classify(now_epoch_days).as_str()),
        format!(
            "  author: {}   age: {}d",
            pr.author,
            pr.days_old(now_epoch_days)
        ),
        format!(
            "  CI: {}   review: {}",
            pr.ci_status.as_str(),
            if pr.review_decision.is_empty() {
                "none"
            } else {
                &pr.review_decision
            }
        ),
    ];
    if let Some(wt) = &pr.worktree_path {
        out.push(format!("  worktree: {wt}"));
    }
    if !pr.blast_radius().is_empty() {
        out.push(format!("  blast radius: {}", pr.blast_radius()));
        if !pr.communities_touched.is_empty() {
            let cs: Vec<String> = pr
                .communities_touched
                .iter()
                .map(|c| c.to_string())
                .collect();
            out.push(format!("  communities: {}", cs.join(", ")));
        }
    }
    if !pr.files_changed.is_empty() {
        out.push(format!("  files ({}):", pr.files_changed.len()));
        for f in pr.files_changed.iter().take(max_files) {
            out.push(format!("    {f}"));
        }
        if pr.files_changed.len() > max_files {
            out.push(format!(
                "    … and {} more",
                pr.files_changed.len() - max_files
            ));
        }
    }
    out.join("\n")
}

// triage & conflicts (CLI `prs --triage` / `--conflicts`)

/// Actionable PRs targeting `base`, sorted by triage rank then age. Drops PRs on
/// the wrong base and stale PRs (the ones not worth acting on now). Mirrors the
/// filter+sort of the MCP `triage_prs` tool.
pub fn select_actionable(prs: Vec<PrInfo>, base: &str, now_epoch_days: i64) -> Vec<PrInfo> {
    let mut actionable: Vec<PrInfo> = prs
        .into_iter()
        .filter(|p| {
            p.base_branch == base
                && !matches!(
                    p.classify(now_epoch_days),
                    Status::WrongBase | Status::Stale
                )
        })
        .collect();
    actionable.sort_by_key(|p| {
        (
            p.classify(now_epoch_days).sort_rank(),
            p.days_old(now_epoch_days),
        )
    });
    actionable
}

/// Plain-text ranked view of `actionable` PRs (the output of [`select_actionable`],
/// with blast radius populated) for the CLI `prs --triage`. Deterministic — no LLM;
/// for LLM summarization run the MCP server and let the assistant rank `triage_prs`.
pub fn format_triage(actionable: &[PrInfo], base: &str, now_epoch_days: i64) -> String {
    if actionable.is_empty() {
        return format!("No actionable PRs targeting {base}.");
    }
    let mut lines = vec![format!(
        "Actionable PRs targeting {base}: {} (ranked by review priority)",
        actionable.len()
    )];
    for p in actionable {
        let impact = if p.blast_radius().is_empty() {
            String::new()
        } else {
            format!("  blast_radius={}", p.blast_radius())
        };
        let review = if p.review_decision.is_empty() {
            "none"
        } else {
            &p.review_decision
        };
        lines.push(format!(
            "#{} [{}] CI={} review={} age={}d author={}{}\n  {}",
            p.number,
            p.classify(now_epoch_days).as_str(),
            p.ci_status.as_str(),
            review,
            p.days_old(now_epoch_days),
            p.author,
            impact,
            p.title
        ));
    }
    lines.join("\n\n")
}

/// Plain-text report of PRs that touch the same graph community (= merge-order
/// risk), grouped by community, most-overlapping first. Only considers PRs
/// targeting `base` that have graph impact data (`communities_touched`).
pub fn format_conflicts(prs: &[PrInfo], base: &str, now_epoch_days: i64) -> String {
    let actionable: Vec<&PrInfo> = prs
        .iter()
        .filter(|p| p.base_branch == base && !p.communities_touched.is_empty())
        .collect();
    if actionable.is_empty() {
        return "No graph impact data — run with a valid graph.json to detect conflicts.".into();
    }
    let mut comm_to_prs: BTreeMap<u32, Vec<&PrInfo>> = BTreeMap::new();
    for p in &actionable {
        for &c in &p.communities_touched {
            comm_to_prs.entry(c).or_default().push(p);
        }
    }
    let mut conflicts: Vec<(u32, Vec<&PrInfo>)> = comm_to_prs
        .into_iter()
        .filter(|(_, ps)| ps.len() > 1)
        .collect();
    if conflicts.is_empty() {
        return "No community overlap between open PRs — safe to merge in any order.".into();
    }
    // Most-overlapping community first; ties broken by community id for determinism.
    conflicts.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    let mut lines = vec!["Community conflicts (PRs sharing the same graph community):".to_string()];
    for (comm, ps) in conflicts {
        let mut block = format!("\nCommunity {comm}  ({} PRs overlap)", ps.len());
        for p in ps {
            block.push_str(&format!(
                "\n  #{:<4} {:<12} {}",
                p.number,
                p.classify(now_epoch_days).as_str(),
                p.title
            ));
        }
        lines.push(block);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// PrInfo with a distinct number, base, and graph communities (impact derived).
    fn pr_n(number: u64, base: &str, communities: Vec<u32>, updated_days: i64) -> PrInfo {
        let nodes_affected = communities.len() * 3;
        PrInfo {
            number,
            communities_touched: communities,
            nodes_affected,
            ..pr(base, CiStatus::Success, "", false, updated_days)
        }
    }

    #[test]
    fn select_actionable_drops_wrong_base_and_stale_and_sorts() {
        let now = 1000;
        let mut ci_fail = pr_n(4, "main", vec![], now);
        ci_fail.ci_status = CiStatus::Failure;
        let prs = vec![
            pr_n(1, "main", vec![], now),      // READY (rank 7)
            pr_n(2, "other", vec![], now),     // wrong base, dropped
            pr_n(3, "main", vec![], now - 30), // STALE, dropped
            ci_fail,                           // CI-FAIL (rank 1)
        ];
        let nums: Vec<u64> = select_actionable(prs, "main", now)
            .iter()
            .map(|p| p.number)
            .collect();
        assert_eq!(
            nums,
            vec![4, 1],
            "wrong-base/stale dropped; CI-FAIL ranks first"
        );
    }

    #[test]
    fn format_triage_lists_ranked_prs_with_blast_radius() {
        let now = 1000;
        let mut p = pr_n(4, "main", vec![1, 2], now);
        p.ci_status = CiStatus::Failure;
        p.title = "Fix auth".into();
        let out = format_triage(&[p], "main", now);
        assert!(out.contains("Actionable PRs targeting main: 1"), "{out}");
        assert!(out.contains("#4 [CI-FAIL]"), "{out}");
        assert!(out.contains("blast_radius="), "{out}");
        assert!(out.contains("Fix auth"), "{out}");
    }

    #[test]
    fn format_triage_empty_is_explicit() {
        assert!(format_triage(&[], "main", 1000).contains("No actionable PRs targeting main"));
    }

    #[test]
    fn format_conflicts_reports_shared_communities_only() {
        let now = 1000;
        let prs = vec![
            pr_n(1, "main", vec![1, 2], now),
            pr_n(2, "main", vec![2, 3], now), // shares community 2 with #1
            pr_n(3, "main", vec![9], now),    // no overlap
        ];
        let out = format_conflicts(&prs, "main", now);
        assert!(out.contains("Community 2"), "{out}");
        assert!(out.contains("#1"), "{out}");
        assert!(out.contains("#2"), "{out}");
        assert!(
            !out.contains("Community 9"),
            "non-overlapping community omitted: {out}"
        );
    }

    #[test]
    fn format_conflicts_no_overlap_is_safe() {
        let now = 1000;
        let prs = vec![pr_n(1, "main", vec![1], now), pr_n(2, "main", vec![2], now)];
        assert!(format_conflicts(&prs, "main", now).contains("safe to merge"));
    }

    #[test]
    fn format_conflicts_without_impact_data_says_so() {
        let now = 1000;
        let prs = vec![pr_n(1, "main", vec![], now)];
        assert!(format_conflicts(&prs, "main", now).contains("No graph impact data"));
    }

    fn pr(base: &str, ci: CiStatus, review: &str, draft: bool, updated_days: i64) -> PrInfo {
        PrInfo {
            number: 1,
            title: "t".into(),
            branch: "feat".into(),
            base_branch: base.into(),
            author: "a".into(),
            is_draft: draft,
            review_decision: review.into(),
            ci_status: ci,
            updated_epoch_days: updated_days,
            expected_base: "main".into(),
            worktree_path: None,
            communities_touched: vec![],
            nodes_affected: 0,
            files_changed: vec![],
        }
    }

    #[test]
    fn classify_precedence() {
        let now = 1000;
        // Wrong base beats everything.
        assert_eq!(
            pr("other", CiStatus::Failure, "", false, now).classify(now),
            Status::WrongBase
        );
        // CI failure beats changes-requested/draft.
        assert_eq!(
            pr("main", CiStatus::Failure, "CHANGES_REQUESTED", true, now).classify(now),
            Status::CiFail
        );
        assert_eq!(
            pr("main", CiStatus::Success, "CHANGES_REQUESTED", true, now).classify(now),
            Status::ChangesReq
        );
        assert_eq!(
            pr("main", CiStatus::Success, "", true, now).classify(now),
            Status::Draft
        );
        // Stale (≥14 days) beats approved.
        assert_eq!(
            pr("main", CiStatus::Success, "APPROVED", false, now - 20).classify(now),
            Status::Stale
        );
        // Approved checked before pending.
        assert_eq!(
            pr("main", CiStatus::Pending, "APPROVED", false, now).classify(now),
            Status::Approved
        );
        assert_eq!(
            pr("main", CiStatus::Pending, "", false, now).classify(now),
            Status::Pending
        );
        assert_eq!(
            pr("main", CiStatus::Success, "", false, now).classify(now),
            Status::Ready
        );
    }

    #[test]
    fn parse_ci_rollup() {
        assert_eq!(parse_ci(&[]), CiStatus::None);
        assert_eq!(
            parse_ci(&[
                json!({"conclusion": "FAILURE"}),
                json!({"conclusion": "SUCCESS"})
            ]),
            CiStatus::Failure,
            "any failing conclusion → Failure"
        );
        assert_eq!(
            parse_ci(&[
                json!({"status": "IN_PROGRESS"}),
                json!({"conclusion": "SUCCESS"})
            ]),
            CiStatus::Pending,
            "in-progress → Pending"
        );
        assert_eq!(
            parse_ci(&[json!({"conclusion": "SUCCESS"})]),
            CiStatus::Success
        );
        assert_eq!(parse_ci(&[json!({"status": "COMPLETED"})]), CiStatus::None);
    }

    #[test]
    fn path_match_is_boundary_safe() {
        assert!(path_match("src/auth/api.py", "src/auth/api.py"));
        assert!(
            path_match("src/auth/api.py", "api.py"),
            "suffix on a boundary"
        );
        assert!(path_match("api.py", "pkg/api.py"));
        assert!(!path_match("config.py", "g.py"), "no mid-token match");
    }

    #[test]
    fn blast_radius_dedups_in_both_directions() {
        // Graph: api.py (community 1, 2 nodes), src/auth/api.py (community 2, 1 node).
        let nodes = vec![
            ("api.py", Some(1u32)),
            ("api.py", Some(1)),
            ("src/auth/api.py", Some(2)),
        ];
        // A PR diff listing the same logical file twice must count each graph
        // file once.
        let files = vec!["api.py".to_string(), "src/auth/api.py".to_string()];
        let (comms, n) = compute_pr_impact(nodes, &files);
        assert_eq!(comms, vec![1, 2]);
        assert_eq!(n, 3, "2 + 1 nodes, no double count");
    }

    #[test]
    fn impact_index_reused_matches_compute_pr_impact() {
        // H5: a single ImpactIndex reused across PRs must match the per-call
        // compute_pr_impact for every file set.
        let nodes = [
            ("api.py", Some(1u32)),
            ("api.py", Some(1)),
            ("src/auth/api.py", Some(2)),
            ("db.py", Some(3)),
        ];
        let index = ImpactIndex::build(nodes.iter().copied());
        let cases: [&[&str]; 4] = [
            &["api.py"],
            &["src/auth/api.py", "db.py"],
            &["nomatch.py"],
            &[],
        ];
        for case in cases {
            let files: Vec<String> = case.iter().map(|s| s.to_string()).collect();
            assert_eq!(
                index.impact_for_files(&files),
                compute_pr_impact(nodes.iter().copied(), &files),
                "reused ImpactIndex must match compute_pr_impact for {files:?}"
            );
        }
    }

    #[test]
    fn community_labels_caps_at_top_n() {
        let nodes = vec![
            ("a", Some(0u32)),
            ("b", Some(0)),
            ("c", Some(0)),
            ("x", Some(1)),
        ];
        let labels = build_community_labels(nodes, 2);
        assert_eq!(labels[&0], vec!["a", "b"], "capped at top_n in order");
        assert_eq!(labels[&1], vec!["x"]);
    }

    #[test]
    fn iso_date_parses_to_epoch_days() {
        // 1970-01-01 is day 0; 1970-01-02 is day 1.
        assert_eq!(iso_date_to_epoch_days("1970-01-01T00:00:00Z"), 0);
        assert_eq!(iso_date_to_epoch_days("1970-01-02T12:00:00Z"), 1);
        assert!(iso_date_to_epoch_days("2026-06-13T10:00:00Z") > 20_000);
        assert_eq!(iso_date_to_epoch_days("garbage"), 0);
    }

    /// A mock runner returning canned output keyed by the first arg.
    struct MockGh {
        list: String,
    }
    impl CommandRunner for MockGh {
        fn run(&self, program: &str, args: &[&str]) -> Option<String> {
            if program == "gh" && args.first() == Some(&"pr") && args.get(1) == Some(&"list") {
                Some(self.list.clone())
            } else if program == "gh" && args.first() == Some(&"repo") {
                Some(r#"{"defaultBranchRef": {"name": "main"}}"#.to_string())
            } else {
                None
            }
        }
    }

    #[test]
    fn fetch_prs_parses_gh_json() {
        let list = json!([{
            "number": 7,
            "title": "Add auth",
            "headRefName": "feat/auth",
            "baseRefName": "main",
            "author": {"login": "alice"},
            "isDraft": false,
            "reviewDecision": "APPROVED",
            "statusCheckRollup": [{"conclusion": "SUCCESS"}],
            "updatedAt": "2026-06-10T08:00:00Z"
        }])
        .to_string();
        let prs = fetch_prs(&MockGh { list }, None, None, 50).unwrap();
        assert_eq!(prs.len(), 1);
        let p = &prs[0];
        assert_eq!(p.number, 7);
        assert_eq!(p.author, "alice");
        assert_eq!(p.base_branch, "main");
        assert_eq!(p.expected_base, "main", "default branch detected");
        assert_eq!(p.ci_status, CiStatus::Success);
        assert_eq!(p.review_decision, "APPROVED");
    }

    #[test]
    fn fetch_prs_errors_when_gh_unavailable() {
        struct Dead;
        impl CommandRunner for Dead {
            fn run(&self, _p: &str, _a: &[&str]) -> Option<String> {
                None
            }
        }
        assert!(matches!(
            fetch_prs(&Dead, None, Some("main"), 50),
            Err(PrError::GhUnavailable)
        ));
    }
}
