//! MCP server for Synaptic.
//!
//! C3a — the read-only tool surface over **stdio**: an AI assistant drives the
//! graph via MCP. Rather than depend on `rmcp` (whose API churns), we speak the
//! MCP stdio transport directly — newline-delimited JSON-RPC 2.0 — through a
//! pure [`Server::handle_request`] dispatcher, which makes the whole protocol
//! unit-testable without an async runtime.
//!
//! 28 read-only tools by default (29 with `--allow-exec`, which adds the
//! command-running `speculate`), over a graph loaded at startup: graph navigation
//! (`query_graph`, `get_node`, `get_source`, `get_neighbors`, `get_community`,
//! `god_nodes`, `graph_stats`, `shortest_path`, `find_callers`, `find_callees`),
//! impact and forecasting (`affected`, `working_changes_impact`, `predict_impact`,
//! `affected_tests`, `predict_edit`), advanced (`structural_search`, `describe_node`,
//! `time_travel_diff`, plan-only `plan_rename`), SQL (`audit_sql`, `advise_sql`),
//! federation (`list_repos`, `repo_stats`), and PR (`list_prs`, `get_pr_impact`,
//! `triage_prs`), plus six resources. The `initialize` reply
//! returns server `instructions` orienting the agent, and each tool documents its
//! parameters, so an assistant uses them correctly. Every label is run through
//! [`synaptic_core::sanitize_label`] before it reaches tool text (a security
//! boundary on LLM/corpus-derived names).
#![forbid(unsafe_code)]
// The `tools_list` JSON schema literal is large and deeply nested (tool input +
// output schemas); the default 128 macro-expansion depth is not enough.
#![recursion_limit = "256"]

mod http;
mod prompts;
mod search;
pub mod session;
mod source;
pub use http::serve_http;
pub use session::{SessionStore, DEFAULT_SESSION_IDLE};

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use synaptic_core::{sanitize_label, GraphData, Node, NodeId};
use synaptic_graph::{
    god_nodes, graph_stats, suggest_questions, surprising_connections, GodNode, GraphStats,
    KnowledgeGraph,
};
use synaptic_predict::{
    assess_edit, forecast_changes_with_index, ChangeForecast, EditKind, ForecastOptions,
};
use synaptic_prs::{
    compute_pr_impact, detect_default_branch, fetch_pr, fetch_pr_files, fetch_prs, fetch_worktrees,
    format_pr_detail, format_prs_text, today_epoch_days, CommandRunner, ImpactIndex, PrInfo,
    Status, SystemCommands,
};
use synaptic_query::{
    affected_including_members, candidate_details, dependents_caveat, describe_node, explain,
    is_type_like, references_to, resolve_detailed, shortest_path, type_member_ids, AffectedHit,
    DynamicCaveat, DynamicHazardIndex, QueryIndex, Recency, RecencyMode, Resolution,
    ReverseImpactIndex, TraversalMode, DEFAULT_AFFECTED_RELATIONS,
};
use synaptic_sandbox::{
    render_markdown as render_speculate_md, speculate, Change, SpeculateOptions,
};

const SUPPORTED_PROTOCOLS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// `query_graph` recency-boost strength: a max-churn changed-file node gains an
/// additive `RECENCY_BOOST` on its relevance (IDF scores run ~2-6), so changed
/// code ranks well above otherwise-equal unchanged code without burying a strong
/// query match. Tuned against the live test.
const RECENCY_BOOST: f64 = 4.0;

/// The changed-files signal resolved from a `since` argument: which graph nodes
/// live in changed files, each node's normalised churn weight, and a label for
/// the output header. Built by [`Server::resolve_recency`] via git.
struct ResolvedRecency {
    changed: std::collections::HashSet<NodeId>,
    churn: std::collections::HashMap<NodeId, f64>,
    base_label: String,
    n_files: usize,
}
const LATEST_PROTOCOL: &str = "2025-11-25";

/// Hard cap on how many god nodes one page renders. Each row costs a depth-3
/// reverse-impact walk over a hub, so an unbounded page could run a walk per node
/// across the whole graph; page past the cap with `offset`.
const GOD_NODES_PAGE_CAP: usize = 200;

/// One annotated god-node row: a hub plus how many tests transitively exercise it.
/// The test count is the reverse-impact work computed once per page and shared
/// between the text and structured channels.
struct GodNodeRow {
    id: NodeId,
    label: String,
    degree: usize,
    test_count: usize,
    /// True when an evidence-link resolved a dynamic site to this hub: a high-degree
    /// node reachable via reflection is extra dangerous to change.
    dynamically_referenced: bool,
}

/// Echo the client's requested protocol when we support it, else our latest.
fn negotiate_protocol(requested: Option<&str>) -> &'static str {
    match requested {
        Some(v) => SUPPORTED_PROTOCOLS
            .iter()
            .copied()
            .find(|s| *s == v)
            .unwrap_or(LATEST_PROTOCOL),
        None => LATEST_PROTOCOL,
    }
}

/// A loaded graph + the server's view of it. Hot-reloads when `graph.json`
/// changes (C3c). [`handle_request`](Server::handle_request) takes `&mut self`
/// (the stdio path); the HTTP transport shares one behind an `Arc<RwLock<Server>>`,
/// so read requests run concurrently and the write lock is taken only to hot-reload.
pub struct Server {
    kg: KnowledgeGraph,
    communities: BTreeMap<u32, Vec<NodeId>>,
    /// IDF + adjacency index for `query_graph`, built once at load/reload so
    /// queries don't rebuild it per request (H1).
    query_index: QueryIndex,
    /// Reverse-impact adjacency over `DEFAULT_AFFECTED_RELATIONS`, built once at
    /// load/reload so the predict tools (`predict_impact`, `affected_tests`,
    /// `speculate`) walk the blast radius without rebuilding it per request.
    affected_index: ReverseImpactIndex,
    /// Dynamic-dispatch hazard catalog (opaque reflection sites + evidence-linked
    /// node set), built once at load/reload to power the honesty caveat and the
    /// `dynamic_hazards` tool.
    hazard_index: DynamicHazardIndex,
    /// Headline stats, computed once at load/reload (H3).
    stats: GraphStats,
    /// Full degree-ranked god-node list, computed once at load/reload; tools
    /// slice the requested `top_n` from it instead of recomputing (H3).
    god_nodes_all: Vec<GodNode>,
    /// Path the graph was loaded from (its parent dir holds `GRAPH_REPORT.md`).
    graph_path: Option<PathBuf>,
    /// `(mtime_secs, size)` of the loaded graph.json, for the hot-reload check.
    reload_key: Option<(u64, u64)>,
    /// Runs `gh`/`git` for the PR tools (injectable for tests).
    runner: Box<dyn CommandRunner>,
    /// JSONL query-log path (opt-in via `SYNAPTIC_QUERY_LOG`); `None` = off.
    log_path: Option<PathBuf>,
    /// Trusted root for resolving repo-relative `source_file` paths to real
    /// files (the code-retrieval tools). `None` disables source reading.
    source_root: Option<PathBuf>,
    /// Per-repo source roots for a federated/global graph, keyed by the repo tag
    /// (`Node::repo`). Federation repo-prefixes each node's `source_file` with
    /// `tag/` and the member repos live in sibling dirs outside a single
    /// `source_root`, so the code-retrieval tools resolve a federated node under
    /// its own repo root from this map before falling back to `source_root`.
    repo_roots: HashMap<String, PathBuf>,
    /// Per-repo content fingerprint (`tag -> short source_hash`) read from the
    /// sibling `workspace-state.json` of a federated graph. Surfaced by
    /// `list_repos` so an agent can see each member's extraction fingerprint and
    /// detect per-repo drift; empty for a single-repo graph or when the state file
    /// is absent.
    repo_hashes: HashMap<String, String>,
    /// Whether the command-running `speculate` tool is exposed. OFF by default so
    /// the server stays strictly read-only; enabled only by an explicit operator
    /// opt-in (`serve --allow-exec`). When off, `speculate` is neither advertised
    /// in tools/list nor runnable.
    allow_exec: bool,
    /// Token-lean output mode. When on (env `SYNAPTIC_CONCISE` or `serve
    /// --concise`), tools that take a size/limit knob fall back to lower defaults
    /// so a default call returns less to the model; an explicit per-call argument
    /// still wins. Off by default to preserve existing output sizes.
    concise: bool,
    /// On-query catch-up config (repo root, output dir, debounce, caps). `None`
    /// disables auto-freshen (e.g. no source root, or no graph path).
    freshen: Option<FreshenConfig>,
    /// Last time the catch-up staleness walk ran, for debouncing. Interior
    /// mutability so the cheap gate can run under the HTTP shared read lock.
    last_fresh_check: Mutex<Option<Instant>>,
}

/// Configuration for the serve catch-up path: detect files an agent
/// added/changed since the graph was built and incrementally rebuild before
/// answering, so live-coded files are queryable without a separate `watch`.
#[derive(Debug, Clone)]
struct FreshenConfig {
    /// Repo root scanned for source changes (the source root).
    root: PathBuf,
    /// Output dir holding `graph.json`, the manifest, and the rebuild lock.
    out_dir: PathBuf,
    /// Whether auto-freshen is on (env `SYNAPTIC_SERVE_AUTOFRESH`).
    enabled: bool,
    /// Minimum gap between staleness walks, so a burst of queries walks once.
    debounce: Duration,
    /// Skip auto-freshen when more than this many files changed (a branch switch
    /// shouldn't block a query on a near-full rebuild); 0 = unlimited.
    max_files: usize,
}

impl FreshenConfig {
    /// Derive config from the repo root + graph path, honoring env overrides.
    /// Returns `None` (disabling auto-freshen) when there is no graph path to
    /// locate the output dir.
    fn from_env(root: PathBuf, graph_path: Option<&Path>) -> Option<FreshenConfig> {
        let out_dir = graph_path?.parent()?.to_path_buf();
        let enabled = std::env::var("SYNAPTIC_SERVE_AUTOFRESH")
            .map(|v| !matches!(v.trim(), "0" | "false" | "no" | "off"))
            .unwrap_or(true);
        let debounce = std::env::var("SYNAPTIC_SERVE_AUTOFRESH_DEBOUNCE_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_millis(1000));
        let max_files = std::env::var("SYNAPTIC_SERVE_AUTOFRESH_MAX_FILES")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(500);
        Some(FreshenConfig {
            root,
            out_dir,
            enabled,
            debounce,
            max_files,
        })
    }
}

/// Read per-repo content fingerprints from the `workspace-state.json` sibling of
/// a federated `graph.json` (`{ members: { <tag>: { source_hash } } }`). Returns
/// `tag -> short (12-char) source_hash`. Empty for a single-repo graph, a missing
/// or malformed state file, or no graph path. Best-effort: never fails a load.
fn read_repo_hashes(graph_path: Option<&Path>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(dir) = graph_path.and_then(Path::parent) else {
        return out;
    };
    let Ok(bytes) = std::fs::read(dir.join("workspace-state.json")) else {
        return out;
    };
    let Ok(state) = serde_json::from_slice::<Value>(&bytes) else {
        return out;
    };
    let Some(members) = state.get("members").and_then(Value::as_object) else {
        return out;
    };
    for (tag, entry) in members {
        if let Some(hash) = entry.get("source_hash").and_then(Value::as_str) {
            let short: String = hash.chars().take(12).collect();
            out.insert(tag.clone(), short);
        }
    }
    out
}

/// Whether token-lean output mode is on from the environment. Truthy unless the
/// value is an explicit off token, mirroring the `SYNAPTIC_SERVE_AUTOFRESH`
/// parse; unset = off (default output sizes unchanged).
fn concise_from_env() -> bool {
    std::env::var("SYNAPTIC_CONCISE")
        .map(|v| !matches!(v.trim(), "" | "0" | "false" | "no" | "off"))
        .unwrap_or(false)
}

fn reload_key_for(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some((mtime, meta.len()))
}

fn query_log_path() -> Option<PathBuf> {
    let disabled = std::env::var("SYNAPTIC_QUERY_LOG_DISABLE")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false);
    if disabled {
        return None;
    }
    std::env::var("SYNAPTIC_QUERY_LOG").ok().map(PathBuf::from)
}

/// Append a `Title (N):` section listing up to `cap` rename edit sites, with a
/// `+N more` summary when truncated. Shared by `plan_rename`'s Edits and Review
/// lists; the per-site rendering is reused from the CLI so the two never drift.
fn append_capped_sites(
    o: &mut String,
    title: &str,
    sites: &[synaptic_refactor::EditSite],
    cap: usize,
) {
    if sites.is_empty() {
        return;
    }
    o.push_str(&format!("\n{title} ({}):", sites.len()));
    for s in sites.iter().take(cap) {
        o.push_str(&format!("\n  {}", synaptic_refactor::emit::site_line(s)));
    }
    if sites.len() > cap {
        o.push_str(&format!(
            "\n  ... (+{} more; pass verbose=true for the full list)",
            sites.len() - cap
        ));
    }
}

/// `(neighbor, relation, direction)` -> the distinct `(source_file,
/// source_location)` call sites on the edges between a node and that neighbor.
/// Populated by [`Server::edge_sites`] for the `show_sites` affordance.
type SiteMap = HashMap<(NodeId, String, &'static str), Vec<(String, Option<String>)>>;

/// Result of locating a node's source on disk for the code-retrieval tools.
enum SourceLookup {
    /// A readable file inside the trusted root.
    Found(PathBuf),
    /// No source root configured at all (source reading disabled).
    NotConfigured,
    /// No file at the resolved path under `root`.
    Missing { root: PathBuf },
    /// The path resolved outside `root` (jail escape / wrong root).
    Outside { root: PathBuf },
}

/// One `dynamic_hazards` row: `(repo, file, line, kind, key, host)`.
type HazardRow = (String, String, u32, &'static str, Option<String>, String);

/// Symbol name from a node label: up to `(`, lowercased (`runJob(a)` -> `runjob`).
fn hazard_bare(label: &str) -> String {
    label
        .split('(')
        .next()
        .unwrap_or(label)
        .trim()
        .to_ascii_lowercase()
}

/// Last path/namespace segment of a reflection key, lowercased (`com.x.Y` -> `y`).
fn hazard_key_seg(k: &str) -> String {
    k.rsplit(['.', ':', '/', '\\'])
        .next()
        .unwrap_or(k)
        .trim()
        .to_ascii_lowercase()
}

/// Translate a `*` / `**` / `?` path glob into an anchored regex over `/`-paths.
/// `**/` matches zero or more directories; `*` does not cross `/`. Used by
/// `dynamic_hazards` to filter graph `source_file` strings.
fn glob_to_regex(glob: &str) -> String {
    let chars: Vec<char> = glob.replace('\\', "/").chars().collect();
    let mut re = String::from("^");
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                if chars.get(i + 1) == Some(&'*') {
                    if chars.get(i + 2) == Some(&'/') {
                        re.push_str("(?:.*/)?");
                        i += 3;
                        continue;
                    }
                    re.push_str(".*");
                    i += 2;
                    continue;
                }
                re.push_str("[^/]*");
            }
            '?' => re.push_str("[^/]"),
            c @ ('.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\') => {
                re.push('\\');
                re.push(c);
            }
            c => re.push(c),
        }
        i += 1;
    }
    re.push('$');
    re
}

impl Server {
    /// Build a server from already-parsed graph data.
    pub fn from_graph_data(gd: GraphData, graph_path: Option<PathBuf>) -> Server {
        let kg = KnowledgeGraph::from_graph_data(gd);
        let communities = communities_of(&kg);
        let query_index = QueryIndex::build(&kg);
        let affected_index = ReverseImpactIndex::build(&kg, DEFAULT_AFFECTED_RELATIONS);
        let hazard_index = DynamicHazardIndex::build(&kg);
        let stats = graph_stats(&kg);
        let god_nodes_all = god_nodes(&kg, usize::MAX);
        let reload_key = graph_path.as_deref().and_then(reload_key_for);
        let repo_hashes = read_repo_hashes(graph_path.as_deref());
        Server {
            kg,
            communities,
            query_index,
            affected_index,
            hazard_index,
            stats,
            god_nodes_all,
            graph_path,
            reload_key,
            runner: Box::new(SystemCommands),
            log_path: query_log_path(),
            source_root: None,
            repo_roots: HashMap::new(),
            repo_hashes,
            allow_exec: false,
            concise: concise_from_env(),
            freshen: None,
            last_fresh_check: Mutex::new(None),
        }
    }

    /// Load a server from a `graph.json` path.
    pub fn load(path: PathBuf) -> std::io::Result<Server> {
        let bytes = std::fs::read(&path)?;
        let gd: GraphData = serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Server::from_graph_data(gd, Some(path)))
    }

    /// Override the gh/git command runner (tests inject a mock).
    pub fn with_runner(mut self, runner: Box<dyn CommandRunner>) -> Server {
        self.runner = runner;
        self
    }

    /// Set the trusted source root for `get_source` (and other code-reading
    /// tools). Stored as-is; resolution canonicalizes per request.
    pub fn with_source_root(mut self, root: PathBuf) -> Server {
        self.freshen = FreshenConfig::from_env(root.clone(), self.graph_path.as_deref());
        self.source_root = Some(root);
        self
    }

    /// Opt in to the command-running `speculate` tool. OFF by default; turning it
    /// on means the server can execute the project's test/build commands in a
    /// throwaway worktree, which is no longer read-only -- the caller is asserting
    /// that is acceptable for this deployment.
    pub fn with_allow_exec(mut self, allow: bool) -> Server {
        self.allow_exec = allow;
        self
    }

    /// Turn on token-lean output mode (lower default list/budget sizes). Only
    /// ever enables it: the env (`SYNAPTIC_CONCISE`) may already have, and a
    /// `serve --concise` flag should not be able to turn it back off.
    pub fn with_concise(mut self, concise: bool) -> Server {
        self.concise = self.concise || concise;
        self
    }

    /// Register per-repo source roots for a federated/global graph (`tag ->
    /// repo root`). Lets the code-retrieval tools read a federated node from its
    /// own repo even though it lives outside the single `source_root`.
    pub fn with_repo_roots(mut self, roots: HashMap<String, PathBuf>) -> Server {
        self.repo_roots = roots;
        self
    }

    /// Pick the root a node's source should be read from and the path relative to
    /// it. A federated node (`repo` set, `source_file` prefixed with `tag/`) is
    /// resolved under its own repo root when one is registered; otherwise the
    /// single `source_root` and the raw `source_file` are used.
    fn root_for_node(&self, n: &Node) -> Option<(PathBuf, String)> {
        if let Some(tag) = n.repo.as_deref() {
            if let Some(root) = self.repo_roots.get(tag) {
                let rel = n
                    .source_file
                    .strip_prefix(&format!("{tag}/"))
                    .unwrap_or(&n.source_file);
                return Some((root.clone(), rel.to_string()));
            }
        }
        self.source_root
            .as_ref()
            .map(|r| (r.clone(), n.source_file.clone()))
    }

    /// Resolve a node's source to a real, in-jail path, or an explanation of why
    /// it could not be read (no root, missing file, or outside the trusted root).
    fn locate_source(&self, n: &Node) -> SourceLookup {
        let Some((root, rel)) = self.root_for_node(n) else {
            return SourceLookup::NotConfigured;
        };
        match source::resolve_in_root_detailed(&root, &rel) {
            source::ResolveOutcome::Found(p) => SourceLookup::Found(p),
            source::ResolveOutcome::Missing => SourceLookup::Missing { root },
            source::ResolveOutcome::OutsideRoot => SourceLookup::Outside { root },
        }
    }

    /// Pick the root a RAW path should be read from and the path relative to it.
    /// Mirrors [`root_for_node`](Server::root_for_node) but for a path the caller
    /// supplies directly (e.g. a `get_source` `file` argument or an edge's
    /// `source_file`): a leading `tag/` that names a registered repo resolves
    /// under that member's root, otherwise the single `source_root` is used.
    fn root_for_path(&self, file: &str) -> Option<(PathBuf, String)> {
        let norm = file.replace('\\', "/");
        if let Some((tag, rest)) = norm.split_once('/') {
            if let Some(root) = self.repo_roots.get(tag) {
                return Some((root.clone(), rest.to_string()));
            }
        }
        self.source_root.as_ref().map(|r| (r.clone(), norm))
    }

    /// Resolve a raw path to a real, in-jail file, or say why it could not be read.
    fn locate_path(&self, file: &str) -> SourceLookup {
        let Some((root, rel)) = self.root_for_path(file) else {
            return SourceLookup::NotConfigured;
        };
        match source::resolve_in_root_detailed(&root, &rel) {
            source::ResolveOutcome::Found(p) => SourceLookup::Found(p),
            source::ResolveOutcome::Missing => SourceLookup::Missing { root },
            source::ResolveOutcome::OutsideRoot => SourceLookup::Outside { root },
        }
    }

    /// Read a single 1-based line from a jailed source file, trimmed and capped
    /// for display. `None` if the file is unreadable/outside the jail or the line
    /// is past the end -- callers fall back to showing just `file:line`.
    fn read_source_line(&self, file: &str, line: usize) -> Option<String> {
        let SourceLookup::Found(path) = self.locate_path(file) else {
            return None;
        };
        let text = std::fs::read_to_string(path).ok()?;
        let raw = text.lines().nth(line.saturating_sub(1))?;
        let trimmed = raw.trim();
        Some(if trimmed.chars().count() > 200 {
            let mut s: String = trimmed.chars().take(200).collect();
            s.push_str("...");
            s
        } else {
            trimmed.to_string()
        })
    }

    /// The call-site edges incident to `id`, keyed by `(neighbor, relation,
    /// direction)` -> the distinct `(source_file, source_location)` sites. The
    /// site lives in the CALLER's file: for an out-edge it is where `id` calls the
    /// neighbor; for an in-edge it is where the neighbor calls `id`. Used by
    /// `show_sites` to turn "A calls B" into "A calls B at file:line: <code>".
    fn edge_sites(&self, id: &NodeId, into: &mut SiteMap) {
        for e in self.kg.incident_edges(id) {
            let (nb, dir): (NodeId, &'static str) = if &e.source == id && &e.target != id {
                (e.target.clone(), "out")
            } else if &e.target == id && &e.source != id {
                (e.source.clone(), "in")
            } else {
                continue;
            };
            let v = into.entry((nb, e.relation.clone(), dir)).or_default();
            let site = (e.source_file.clone(), e.source_location.clone());
            if !v.contains(&site) {
                v.push(site);
            }
        }
    }

    /// Render up to `cap` call sites as indented `at file:line: <code>` lines (the
    /// code is read from the jail; absent a readable line, just `at file:line`).
    fn render_sites(&self, sites: &[(String, Option<String>)], indent: &str, cap: usize) -> String {
        let mut out = String::new();
        for (file, loc) in sites.iter().take(cap) {
            let line = loc.as_deref().and_then(source::parse_line_marker);
            let rendered = match line {
                Some(l) => match self.read_source_line(file, l) {
                    Some(text) => {
                        format!("at {}:{}: {}", sanitize_label(file), l, text)
                    }
                    None => format!("at {}:{}", sanitize_label(file), l),
                },
                None if !file.is_empty() => format!("at {}", sanitize_label(file)),
                None => continue,
            };
            out.push_str(&format!("\n{indent}{rendered}"));
        }
        let extra = sites.len().saturating_sub(cap);
        if extra > 0 {
            out.push_str(&format!("\n{indent}... (+{extra} more site(s))"));
        }
        out
    }

    /// Reload `graph.json` if it changed on disk since the last check.
    /// Best-effort: a missing/corrupt file keeps the current graph
    /// (serve-stale-on-error).
    fn maybe_reload(&mut self) {
        let Some(path) = self.graph_path.clone() else {
            return;
        };
        let Some(key) = reload_key_for(&path) else {
            return; // file vanished, keep serving the current graph
        };
        if self.reload_key == Some(key) {
            return; // unchanged
        }
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(gd) = serde_json::from_slice::<GraphData>(&bytes) {
                self.reindex_from(KnowledgeGraph::from_graph_data(gd));
                self.reload_key = Some(key);
            }
        }
    }

    /// Swap in a new graph and rebuild every derived index (query/affected/stats/
    /// god-nodes). Shared by [`maybe_reload`](Server::maybe_reload) and the
    /// catch-up path so both refresh the server's view identically.
    fn reindex_from(&mut self, kg: KnowledgeGraph) {
        self.kg = kg;
        self.communities = communities_of(&self.kg);
        self.query_index = QueryIndex::build(&self.kg);
        self.affected_index = ReverseImpactIndex::build(&self.kg, DEFAULT_AFFECTED_RELATIONS);
        self.hazard_index = DynamicHazardIndex::build(&self.kg);
        self.stats = graph_stats(&self.kg);
        self.god_nodes_all = god_nodes(&self.kg, usize::MAX);
        self.repo_hashes = read_repo_hashes(self.graph_path.as_deref());
    }

    /// Cheap, read-lock-safe staleness gate for the catch-up path: debounced so a
    /// burst of queries walks the tree at most once per window. Returns the
    /// repo-relative paths an agent added/changed/removed since the graph was
    /// built, or `None` when auto-freshen is off, within the debounce window,
    /// nothing changed, or the change set is too large.
    fn needs_freshen(&self) -> Option<synaptic_incremental::ChangeReport> {
        let cfg = self.freshen.as_ref()?;
        if !cfg.enabled {
            return None;
        }
        // Debounce: walk the tree at most once per window. Interior mutability so
        // this gate runs under the HTTP shared read lock.
        {
            let mut last = self.last_fresh_check.lock().ok()?;
            if let Some(t) = *last {
                if t.elapsed() < cfg.debounce {
                    return None;
                }
            }
            *last = Some(Instant::now());
        }
        let report = synaptic_incremental::detect_changes(&cfg.out_dir, &cfg.root);
        if report.is_empty() {
            return None;
        }
        let changed = report.changed_paths().len();
        if cfg.max_files != 0 && changed > cfg.max_files {
            eprintln!(
                "[synaptic] {} files changed since the graph was built (> autofresh max {}); \
                 run `synaptic update` to refresh -- serving the current graph.",
                changed, cfg.max_files
            );
            return None;
        }
        Some(report)
    }

    /// Run a synchronous incremental rebuild under the rebuild lock, persist
    /// `graph.json` + the provenance manifest, and refresh the in-memory indices.
    /// Reuses the detect result and freshly built manifest from `report` so the
    /// whole catch-up walks the tree only once. Best-effort: lock contention or a
    /// rebuild error leaves the current graph in place.
    fn apply_freshen(&mut self, report: synaptic_incremental::ChangeReport) {
        let Some(cfg) = self.freshen.clone() else {
            return;
        };
        let Some(graph_path) = self.graph_path.clone() else {
            return;
        };
        // Serialize with `watch`/`update`: if another rebuild holds the lock,
        // leave the current graph in place -- that rebuild rewrites graph.json and
        // the mtime hot-reload picks it up on a later request.
        let _lock = match synaptic_incremental::try_acquire_lock(&cfg.out_dir) {
            Ok(Some(guard)) => guard,
            Ok(None) => return,
            Err(e) => {
                eprintln!("[synaptic] auto-freshen: could not acquire rebuild lock: {e}");
                return;
            }
        };
        let existing = self.kg.to_graph_data();
        let opts = synaptic_incremental::RebuildOptions {
            root: cfg.root.clone(),
            directed: self.kg.directed,
            force: false,
        };
        let changes = synaptic_incremental::ChangeSet::Incremental(report.changed_paths());
        // Reuse the scan from detect_changes instead of walking the tree again.
        let outcome = match synaptic_incremental::rebuild_with_detect(
            &opts,
            &changes,
            Some(&existing),
            &report.det,
        ) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("[synaptic] auto-freshen: rebuild failed: {e}");
                return;
            }
        };
        // Advance provenance even when topology is unchanged (a comment-only edit
        // still moves the manifest, so we don't re-detect it on every query). The
        // manifest was already built by detect_changes; just persist it.
        if let Err(e) = report
            .current
            .save(&synaptic_incremental::manifest_path(&cfg.out_dir))
        {
            eprintln!("[synaptic] auto-freshen: could not write manifest: {e}");
        }
        if outcome.changed {
            // Persist graph.json so other processes and our own next mtime check
            // agree, then update reload_key so that check is a no-op.
            if let Ok(bytes) = serde_json::to_vec_pretty(&outcome.kg.to_graph_data()) {
                if std::fs::write(&graph_path, &bytes).is_ok() {
                    self.reload_key = reload_key_for(&graph_path);
                }
            }
            self.reindex_from(outcome.kg);
        }
    }

    fn label_of(&self, id: &NodeId) -> String {
        self.kg
            .node(id)
            .map(|n| n.label.clone())
            .unwrap_or_else(|| id.0.clone())
    }

    fn degree(&self, id: &NodeId) -> usize {
        self.kg.degree(id)
    }

    /// The type-container members of `id` when it is a class/struct/interface/...
    /// node, else empty. Used to fold a type's members into reverse-impact so the
    /// blast radius reflects the members' incoming calls (where a class's real
    /// coupling lives), not just the bare type symbol.
    fn type_members(&self, id: &NodeId) -> Vec<NodeId> {
        match self.kg.node(id).and_then(|n| n.kind()) {
            Some(k) if is_type_like(k) => type_member_ids(&self.kg, id),
            _ => Vec::new(),
        }
    }

    /// Reverse-impact for `id`, folding a type's members in (shared with the CLI
    /// `affected` command so both surfaces give a class the same non-empty blast
    /// radius). Returns the hits and the member count (0 for a non-type node).
    fn affected_for(&self, id: &NodeId, rels: &[&str], depth: usize) -> (Vec<AffectedHit>, usize) {
        affected_including_members(&self.kg, id, rels, depth)
    }

    /// The dynamic-dispatch honesty caveat for `id`, if its "0 dependents" answer
    /// is untrustworthy (reflection in its file, or it was evidence-linked). `None`
    /// when the node has real static dependents or no dynamic-hazard signal.
    fn dynamic_caveat_for(&self, id: &NodeId) -> Option<DynamicCaveat> {
        let node = self.kg.node(id)?;
        dependents_caveat(&self.kg, &self.hazard_index, node)
    }

    /// One-line note prepended to a type's reverse-impact / caller output,
    /// explaining that the result is aggregated across the type and its members
    /// (so an agent does not misread a class's impact as living on the bare
    /// symbol). Empty when `member_count` is 0 (a non-type or member-less node).
    fn class_fold_note(&self, id: &NodeId, seed: &str, member_count: usize) -> String {
        if member_count == 0 {
            return String::new();
        }
        let kind = self
            .kg
            .node(id)
            .and_then(|n| n.kind())
            .map(|k| k.as_str())
            .unwrap_or("type");
        format!(
            "{seed} is a {kind} with {member_count} members; impact is aggregated across the {kind} and its members (a class's callers attach to its methods).\n"
        )
    }

    // tools

    /// Retrieve the subgraph for `question` and apply `context_filter`, returning
    /// the raw result, the indices into `r.nodes` that survived the filter, and the
    /// resolved recency (changed-files) signal when `since` was given. Shared by the
    /// text and structured renderers so a request runs the index query once.
    fn query_filtered(
        &self,
        question: &str,
        mode: TraversalMode,
        token_budget: usize,
        context_filter: &[String],
        since: Option<&str>,
        recency_mode: RecencyMode,
    ) -> (
        synaptic_query::QueryResult,
        Vec<usize>,
        Option<ResolvedRecency>,
    ) {
        // Map a token budget to a node cap (heuristic); the text render is later
        // truncated to the budget by truncate_to_tokens.
        let max_nodes = (token_budget / 40).clamp(10, 400);
        let resolved = since.and_then(|s| self.resolve_recency(s));
        let rec = resolved.as_ref().map(|rr| Recency {
            changed: &rr.changed,
            churn: Some(&rr.churn),
            mode: recency_mode,
            boost: RECENCY_BOOST,
        });
        let r =
            self.query_index
                .query_with_recency(&self.kg, question, max_nodes, mode, rec.as_ref());
        let keep: Vec<usize> = r
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, id)| {
                if context_filter.is_empty() {
                    return true;
                }
                let sf = self
                    .kg
                    .node(id)
                    .map(|n| n.source_file.as_str())
                    .unwrap_or("");
                context_filter.iter().any(|f| sf.contains(f.as_str()))
            })
            .map(|(i, _)| i)
            .collect();
        (r, keep, resolved)
    }

    /// Resolve a `since` argument (a git ref, a date, or the literal `"auto"`) to
    /// the set of graph nodes living in files changed on the current branch, with
    /// per-node churn weights. Runs git through `self.runner` so it is unit-testable
    /// with a mock. Returns `None` (graceful degrade to a plain query) when git is
    /// unavailable, the ref does not resolve, or nothing changed.
    ///
    /// Scope: `merge-base(base, HEAD)..working-tree` — the branch's commits since it
    /// diverged from `base`, plus uncommitted edits. Churn weight per file is
    /// `ln(1+lines) / ln(1+max_lines)`, so weights land in `(0, 1]`.
    fn resolve_recency(&self, since: &str) -> Option<ResolvedRecency> {
        let git = |args: &[&str]| {
            self.runner
                .run("git", args)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        // 1. Resolve the base ref. Try as a git rev first, then as a date; "auto"
        //    uses the detected default branch. No syntax-guessing of ref vs date.
        let base = if since == "auto" {
            detect_default_branch(&*self.runner, None)
        } else {
            git(&["rev-parse", "--verify", &format!("{since}^{{commit}}")])
                .or_else(|| git(&["rev-list", "-1", &format!("--before={since}"), "HEAD"]))?
        };
        // 2. merge-base(base, HEAD): scope to the branch point, not main's later work.
        let mb = git(&["merge-base", &base, "HEAD"]).unwrap_or_else(|| base.clone());
        // 3. Churn vs the working tree (includes uncommitted edits).
        let out = git(&["diff", "--numstat", "--no-color", "--no-renames", &mb])?;
        let rows = synaptic_history::git::parse_numstat(&out);

        // Total churn (added+removed) per changed file, normalised forward slashes.
        let mut file_churn: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for (a, d, p) in rows {
            *file_churn.entry(p.replace('\\', "/")).or_default() += a + d;
        }
        if file_churn.is_empty() {
            return None;
        }
        let max = file_churn.values().copied().max().unwrap_or(1).max(1) as f64;
        let denom = (1.0 + max).ln();

        // Map changed files to graph nodes (one pass over the graph).
        let mut changed = std::collections::HashSet::new();
        let mut churn = std::collections::HashMap::new();
        for n in self.kg.nodes() {
            let sf = n.source_file.replace('\\', "/");
            if let Some(&lines) = file_churn.get(&sf) {
                // Binary files (lines == 0) keep a small floor so they still boost.
                let w = if lines == 0 {
                    0.1
                } else {
                    ((1.0 + lines as f64).ln() / denom).max(0.1)
                };
                changed.insert(n.id.clone());
                churn.insert(n.id.clone(), w);
            }
        }
        if changed.is_empty() {
            return None; // files changed, but none map to graph nodes
        }
        let short = &mb[..mb.len().min(7)];
        Some(ResolvedRecency {
            changed,
            churn,
            base_label: format!("{since} (merge-base {short})"),
            n_files: file_churn.len(),
        })
    }

    fn render_query_text(
        &self,
        r: &synaptic_query::QueryResult,
        keep: &[usize],
        mode: TraversalMode,
        token_budget: usize,
        recency: Option<&ResolvedRecency>,
        edge_cap: usize,
    ) -> String {
        let in_set: std::collections::HashSet<&NodeId> =
            keep.iter().map(|&i| &r.nodes[i]).collect();
        let mode_str = match mode {
            TraversalMode::Bfs => "bfs",
            TraversalMode::Dfs => "dfs",
        };
        let seeds: Vec<String> = r
            .seeds
            .iter()
            .map(|s| sanitize_label(&self.label_of(s)))
            .collect();
        let mut out = format!(
            "Traversal: {mode_str} | Start: [{}] | {} nodes found\n",
            seeds.join(", "),
            keep.len()
        );
        if let Some(rr) = recency {
            out.push_str(&format!(
                "Recency: since {} | {} changed file(s); changed nodes boosted and marked (changed)\n",
                sanitize_label(&rr.base_label),
                rr.n_files
            ));
        }
        for &i in keep {
            if let Some(n) = self.kg.node(&r.nodes[i]) {
                let mark = if recency.is_some_and(|rr| rr.changed.contains(&r.nodes[i])) {
                    " (changed)"
                } else {
                    ""
                };
                // An external stub has no source file and is not openable with
                // get_source; mark it so the empty source column is self-explanatory.
                let stub_mark = if n.is_external_stub() {
                    " (external)"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "NODE [{:.2}]{} {} [{}] {}{}\n",
                    r.scores.get(i).copied().unwrap_or(0.0),
                    mark,
                    sanitize_label(&n.label),
                    file_type_str(&n.file_type),
                    sanitize_label(&n.source_file),
                    stub_mark
                ));
            }
        }
        // Edges are relevance-ordered; cap them so a dense neighbourhood cannot
        // dominate the result (edge_cap == 0 omits them, the terse default).
        let mut edges_emitted = 0usize;
        for e in &r.edges {
            if edges_emitted >= edge_cap {
                break;
            }
            if in_set.contains(&e.source) && in_set.contains(&e.target) {
                out.push_str(&format!(
                    "EDGE {} --{}--> {}\n",
                    sanitize_label(&self.label_of(&e.source)),
                    sanitize_label(&e.relation),
                    sanitize_label(&self.label_of(&e.target))
                ));
                edges_emitted += 1;
            }
        }
        truncate_to_tokens(out, token_budget)
    }

    /// `query_graph` text render. The MCP `tools/call` path renders text and
    /// structured output from a single `query_filtered` retrieval; this stays
    /// for the REST surface and direct callers.
    pub fn tool_query_graph(
        &self,
        question: &str,
        mode: TraversalMode,
        token_budget: usize,
        context_filter: &[String],
    ) -> String {
        let (r, keep, recency) = self.query_filtered(
            question,
            mode,
            token_budget,
            context_filter,
            None,
            RecencyMode::Boost,
        );
        self.render_query_text(&r, &keep, mode, token_budget, recency.as_ref(), usize::MAX)
    }

    /// Resolve a user-supplied name/id to a single node, or a consistent error
    /// message. On ambiguity the message lists candidate ids (unlike a bare "no
    /// node matches"), so every tool reports the same way. Shared by all
    /// name-taking tools.
    fn resolve_or_msg(&self, label: &str) -> Result<NodeId, String> {
        match resolve_detailed(&self.kg, label) {
            Resolution::Unique(id) => Ok(id),
            Resolution::Ambiguous(ids) => {
                // List each candidate with its file + degree inline so a caller can
                // pick one without a follow-up get_node round-trip. Enrich only the
                // shown prefix (`+N more` already conveys the rest from ids.len()),
                // so a name shared by thousands of files does not pay degree/clone
                // for candidates it will never print.
                let shown = ids.len().min(10);
                let lines: String = candidate_details(&self.kg, &ids[..shown])
                    .iter()
                    .map(|c| {
                        let file = if c.file.is_empty() {
                            "-".to_string()
                        } else {
                            sanitize_label(&c.file)
                        };
                        format!(
                            "\n  {} [{}] (degree {})",
                            sanitize_label(&c.id.0),
                            file,
                            c.degree
                        )
                    })
                    .collect();
                let more = if ids.len() > 10 {
                    format!("\n  +{} more", ids.len() - 10)
                } else {
                    String::new()
                };
                Err(format!(
                    "'{}' is ambiguous - {} candidates:{}{}\nPass a node id (or qualify as name@file) to disambiguate.",
                    sanitize_label(label),
                    ids.len(),
                    lines,
                    more
                ))
            }
            Resolution::NotFound => Err(format!("No node matches '{}'.", sanitize_label(label))),
        }
    }

    /// `get_node` — metadata + degree for the node matching `label`.
    pub fn tool_get_node(&self, label: &str) -> String {
        let id = match self.resolve_or_msg(label) {
            Ok(id) => id,
            Err(msg) => return msg,
        };
        let Some(n) = self.kg.node(&id) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        // Enrichment is shown only when present, so a pre-enrichment
        // graph yields the original output.
        let mut extra = String::new();
        if let Some(k) = n.kind() {
            extra.push_str(&format!("\nKind: {}", k.as_str()));
        }
        if let Some(v) = n.visibility() {
            extra.push_str(&format!("\nVisibility: {}", v.as_str()));
        }
        if let Some(loc) = n.loc() {
            extra.push_str(&format!("\nLOC: {loc}"));
        }
        format!(
            "Node: {}\nID: {}\nSource: {}\nType: {}\nCommunity: {}\nDegree: {}{}",
            sanitize_label(&n.label),
            sanitize_label(&n.id.0),
            sanitize_label(&n.source_file),
            file_type_str(&n.file_type),
            n.community
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
            self.degree(&id),
            extra
        )
    }

    /// Structured mirror of `get_node`: node metadata, or an explicit ambiguity /
    /// not-found shape (matching describe_node / affected) so an agent parsing the
    /// structured channel sees the same resolution outcome as the text instead of a
    /// plain-text-only ambiguity message.
    fn get_node_json(&self, label: &str) -> Value {
        let id = match resolve_detailed(&self.kg, label) {
            Resolution::Unique(id) => id,
            Resolution::Ambiguous(ids) => {
                return json!({
                    "found": false,
                    "ambiguous": true,
                    "query": sanitize_label(label),
                    "candidates": self.candidates_json(&ids)
                });
            }
            Resolution::NotFound => {
                return json!({ "found": false, "query": sanitize_label(label) });
            }
        };
        let Some(n) = self.kg.node(&id) else {
            return json!({ "found": false, "query": sanitize_label(label) });
        };
        let mut obj = serde_json::Map::new();
        obj.insert("found".into(), json!(true));
        obj.insert("id".into(), json!(sanitize_label(&n.id.0)));
        obj.insert("label".into(), json!(sanitize_label(&n.label)));
        obj.insert("source_file".into(), json!(sanitize_label(&n.source_file)));
        obj.insert("file_type".into(), json!(file_type_str(&n.file_type)));
        obj.insert("degree".into(), json!(self.degree(&id)));
        if let Some(c) = n.community {
            obj.insert("community".into(), json!(c));
        }
        if let Some(k) = n.kind() {
            obj.insert("kind".into(), json!(k.as_str()));
        }
        if let Some(v) = n.visibility() {
            obj.insert("visibility".into(), json!(v.as_str()));
        }
        if let Some(loc) = n.loc() {
            obj.insert("loc".into(), json!(loc));
        }
        let sites = n.dynamic_sites();
        if !sites.is_empty() {
            let mut kinds: Vec<&str> = sites.iter().map(|s| s.kind.as_str()).collect();
            kinds.sort();
            kinds.dedup();
            obj.insert(
                "dynamic_sites".into(),
                json!({ "count": sites.len(), "kinds": kinds }),
            );
        }
        if n.dynamically_referenced() {
            obj.insert("dynamically_referenced".into(), json!(true));
        }
        if let Some(c) = self.dynamic_caveat_for(&id) {
            obj.insert(
                "dynamic_caveat".into(),
                serde_json::to_value(&c).unwrap_or(Value::Null),
            );
        }
        Value::Object(obj)
    }

    /// `describe_node` — a compact "takes X, returns Y, calls Z" description of a
    /// symbol from its captured signature and outgoing call edges (graph-only, no
    /// source read). Built for feeding tool/function description generation.
    pub fn tool_describe_node(&self, label: &str) -> String {
        let id = match self.resolve_or_msg(label) {
            Ok(id) => id,
            Err(msg) => return msg,
        };
        let Some(d) = describe_node(&self.kg, &id) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        let mut out = sanitize_label(&d.summary);
        if let Some(sig) = &d.signature {
            out.push_str(&format!("\nSignature: {}", sanitize_label(&sig.raw)));
        }
        if !d.callees.is_empty() {
            let calls: Vec<String> = d.callees.iter().map(|c| sanitize_label(c)).collect();
            out.push_str(&format!(
                "\nCalls ({}): {}",
                d.callees.len(),
                calls.join(", ")
            ));
        }
        // For a type node, the "calls" are empty (a class doesn't call; its methods
        // do). List its members so the description isn't just "class X".
        let members = self.type_members(&id);
        if !members.is_empty() {
            let shown = members.len().min(40);
            let names: Vec<String> = members[..shown]
                .iter()
                .map(|m| sanitize_label(&self.label_of(m)))
                .collect();
            let more = if members.len() > shown {
                format!(", +{} more", members.len() - shown)
            } else {
                String::new()
            };
            out.push_str(&format!(
                "\nMembers ({}): {}{}",
                members.len(),
                names.join(", "),
                more
            ));
        }
        if let Some(c) = self.dynamic_caveat_for(&id) {
            out.push_str(&format!("\nnote: {}", c.message));
        }
        out
    }

    /// Typed mirror of [`tool_describe_node`](Server::tool_describe_node). Resolves
    /// through the unified resolver so an ambiguous name reports candidates (not a
    /// silent pick), matching the text path.
    fn describe_node_json(&self, label: &str) -> Value {
        let id = match resolve_detailed(&self.kg, label) {
            Resolution::Unique(id) => id,
            Resolution::Ambiguous(ids) => {
                return json!({
                    "found": false,
                    "ambiguous": true,
                    "query": sanitize_label(label),
                    "candidates": self.candidates_json(&ids)
                });
            }
            Resolution::NotFound => {
                return json!({ "found": false, "query": sanitize_label(label) });
            }
        };
        let Some(d) = describe_node(&self.kg, &id) else {
            return json!({ "found": false, "query": sanitize_label(label) });
        };
        let mut obj = serde_json::Map::new();
        obj.insert("found".into(), json!(true));
        obj.insert("id".into(), json!(sanitize_label(&d.id.0)));
        obj.insert("label".into(), json!(sanitize_label(&d.label)));
        obj.insert("summary".into(), json!(sanitize_label(&d.summary)));
        if let Some(k) = &d.kind {
            obj.insert("kind".into(), json!(k));
        }
        if let Some(sig) = &d.signature {
            obj.insert("signature".into(), signature_json(sig));
        }
        obj.insert(
            "callees".into(),
            Value::Array(d.callees.iter().map(|c| json!(sanitize_label(c))).collect()),
        );
        // A type node's members (its methods carry the calls, not the bare type).
        let members = self.type_members(&id);
        if !members.is_empty() {
            let names: Vec<Value> = members
                .iter()
                .take(40)
                .map(|m| json!(sanitize_label(&self.label_of(m))))
                .collect();
            obj.insert("members".into(), Value::Array(names));
            obj.insert("member_count".into(), json!(members.len()));
        }
        if self
            .kg
            .node(&id)
            .is_some_and(|n| n.dynamically_referenced())
        {
            obj.insert("dynamically_referenced".into(), json!(true));
        }
        if let Some(c) = self.dynamic_caveat_for(&id) {
            obj.insert(
                "dynamic_caveat".into(),
                serde_json::to_value(&c).unwrap_or(Value::Null),
            );
        }
        Value::Object(obj)
    }

    /// `get_source` — the actual source lines for a symbol. Resolves the node,
    /// reads its file under the source-root jail, and returns a window starting
    /// at the node's recorded line (`source_location` = `"L<n>"`): it stops at the
    /// symbol's end line when the node carries a span (bounded by `context_lines`),
    /// otherwise returns `context_lines` lines from the start.
    ///
    /// When `file` is given instead of resolving a node, an arbitrary jailed
    /// range of that file is returned: `lines` is `"start-end"` (or a single
    /// `"start"`, read for `context_lines`). This reads a region that is not a
    /// single symbol -- a config block, a span around a `search_text` hit -- and
    /// is federation-routed by a leading `tag/` just like the node path.
    pub fn tool_get_source(
        &self,
        label: &str,
        file: Option<&str>,
        lines: Option<&str>,
        context_lines: usize,
    ) -> String {
        if let Some(file) = file {
            return self.source_by_file(file, lines, context_lines);
        }
        let id = match self.resolve_or_msg(label) {
            Ok(id) => id,
            Err(msg) => return msg,
        };
        let Some(n) = self.kg.node(&id) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        let path = match self.locate_source(n) {
            SourceLookup::Found(p) => p,
            SourceLookup::NotConfigured => {
                return format!(
                    "Source not available for {} ({}): no source root is configured (the server was started without --source-root).",
                    sanitize_label(&n.label),
                    sanitize_label(&n.source_file)
                );
            }
            SourceLookup::Missing { root } => {
                return format!(
                    "Source file for {} not found under source-root {}.\n  wanted: {}\n  In a federated workspace, the file may live in a sibling repo outside this root; serve the global graph so each repo's source root is registered.",
                    sanitize_label(&n.label),
                    sanitize_label(&root.display().to_string()),
                    sanitize_label(&n.source_file)
                );
            }
            SourceLookup::Outside { root } => {
                return format!(
                    "Source for {} is outside the configured source-root and was refused.\n  wanted: {}\n  source-root: {}",
                    sanitize_label(&n.label),
                    sanitize_label(&n.source_file),
                    sanitize_label(&root.display().to_string())
                );
            }
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return format!("Could not read {}.", sanitize_label(&n.source_file));
        };
        let start = n
            .source_location
            .as_deref()
            .and_then(source::parse_line_marker)
            .unwrap_or(1);
        let window = context_lines.clamp(1, 400);
        let lines: Vec<&str> = text.lines().collect();
        let from = start.saturating_sub(1).min(lines.len());
        // With a span, stop at the symbol's real end line (capped by the
        // window) so the body isn't over- or under-read; else use a fixed window.
        let span_end = n.span().map(|s| s.end_line as usize);
        let to = match span_end {
            Some(end) => end.clamp(from + 1, from + window).min(lines.len()),
            None => (from + window).min(lines.len()),
        };
        // Header labels are sanitized; the code body is returned verbatim.
        let mut out = format!(
            "{} [{}] {}:L{}-L{}\n",
            sanitize_label(&n.label),
            file_type_str(&n.file_type),
            sanitize_label(&n.source_file),
            from + 1,
            to
        );
        for (i, line) in lines[from..to].iter().enumerate() {
            out.push_str(&format!("{:>5}  {}\n", from + 1 + i, line));
        }
        out
    }

    /// `get_source` for a raw `file` + optional `lines` range (the node-free
    /// path). Same jail and federation routing as the node path.
    fn source_by_file(&self, file: &str, lines: Option<&str>, context_lines: usize) -> String {
        let window = context_lines.clamp(1, 400);
        let path = match self.locate_path(file) {
            SourceLookup::Found(p) => p,
            SourceLookup::NotConfigured => {
                return "Source not available: no source root is configured (the server was started without --source-root).".to_string();
            }
            SourceLookup::Missing { root } => {
                return format!(
                    "File not found under source-root {}.\n  wanted: {}",
                    sanitize_label(&root.display().to_string()),
                    sanitize_label(file)
                );
            }
            SourceLookup::Outside { root } => {
                return format!(
                    "Path {} is outside the configured source-root and was refused.\n  source-root: {}",
                    sanitize_label(file),
                    sanitize_label(&root.display().to_string())
                );
            }
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return format!("Could not read {}.", sanitize_label(file));
        };
        let all: Vec<&str> = text.lines().collect();
        // Parse `lines`: "start-end", a single "start" (read `window` lines), or
        // the whole-file top when omitted.
        let (start, end) = match lines {
            Some(spec) => match parse_line_range(spec, window) {
                Some(r) => r,
                None => {
                    return format!(
                        "Invalid `lines` value '{}'. Use 'start-end' (e.g. '108-140') or a single line 'start'.",
                        sanitize_label(spec)
                    )
                }
            },
            None => (1, window),
        };
        if start > all.len() {
            return format!(
                "{} has only {} line(s); requested line {start}.",
                sanitize_label(file),
                all.len()
            );
        }
        let from = start.saturating_sub(1);
        // Cap the span so a huge range cannot blow the response.
        let to = end.min(from + 400).min(all.len()).max(from + 1);
        let mut out = format!("{}:L{}-L{}\n", sanitize_label(file), from + 1, to);
        for (i, line) in all[from..to].iter().enumerate() {
            out.push_str(&format!("{:>5}  {}\n", from + 1 + i, line));
        }
        out
    }

    /// True if any node is tagged with federation member `tag`. Lets `search_text`
    /// honor a `repo` filter for a multi-repo graph served over a single parent
    /// source root (no registered per-member roots).
    fn graph_has_repo(&self, tag: &str) -> bool {
        self.kg.nodes().any(|n| n.repo.as_deref() == Some(tag))
    }

    /// The roots `search_text` walks. With no `only`, that is every registered
    /// federated repo root, or -- for a single-repo graph -- the lone
    /// `--source-root`. A federated graph is searched per-member (never via the
    /// parent `source_root`) so each hit carries the right tag and is not double
    /// counted. `only` restricts to one member tag.
    fn search_roots(&self, only: Option<&str>) -> Vec<search::Root> {
        if let Some(tag) = only {
            // The normal path: a per-member root registered from a global-manifest.
            if let Some(p) = self.repo_roots.get(tag) {
                return vec![search::Root {
                    tag: Some(tag.to_string()),
                    path: p.clone(),
                }];
            }
            // Fallback for a multi-repo graph served over a single parent
            // --source-root with no registered member roots: the graph knows the
            // member (node.repo == tag) and its files live under <source_root>/<tag>
            // (federated graph paths are `tag/rel`). If that subtree exists, search
            // it as the member so the repo filter works and hits carry the tag.
            if self.graph_has_repo(tag) {
                if let Some(member) = self
                    .source_root
                    .as_ref()
                    .map(|r| r.join(tag))
                    .filter(|p| p.is_dir())
                {
                    return vec![search::Root {
                        tag: Some(tag.to_string()),
                        path: member,
                    }];
                }
            }
            return Vec::new();
        }
        if self.repo_roots.is_empty() {
            return self
                .source_root
                .iter()
                .map(|p| search::Root {
                    tag: None,
                    path: p.clone(),
                })
                .collect();
        }
        self.repo_roots
            .iter()
            .map(|(tag, p)| search::Root {
                tag: Some(tag.clone()),
                path: p.clone(),
            })
            .collect()
    }

    /// `search_text` — content (text/regex) search over the source roots, with
    /// every hit attributed to the graph node whose span encloses it. Computes
    /// the text and the structured mirror from a SINGLE walk (the walk is the
    /// cost), so the dispatcher renders both without searching twice.
    fn search_text_dual(
        &self,
        pattern: &str,
        literal: bool,
        case_sensitive: Option<bool>,
        repo: Option<&str>,
        path_glob: Option<&str>,
        max_results: usize,
    ) -> (String, Value) {
        let empty = |msg: String| {
            (
                msg,
                json!({ "pattern": pattern, "total": 0, "truncated": false,
                        "files_scanned": 0, "hits": [] }),
            )
        };
        if pattern.is_empty() {
            return empty("search_text needs a non-empty `pattern`.".to_string());
        }
        let roots = self.search_roots(repo);
        if roots.is_empty() {
            return empty(match repo {
                Some(r) => format!(
                    "No source root is registered for repo '{}'; serve the federated/global graph so its members' roots are known, or check the tag with list_repos.",
                    sanitize_label(r)
                ),
                None => "Source search needs a source root; start the server with --source-root <repo> (or serve a federated graph so each member's root is registered).".to_string(),
            });
        }
        let q = search::Query {
            pattern,
            literal,
            case_sensitive,
            path_glob,
            max_results: max_results.clamp(1, 1000),
            max_line_len: 300,
        };
        let outcome = match search::run(&roots, &q) {
            Ok(o) => o,
            Err(e) => return empty(format!("search_text: {}", sanitize_label(&e))),
        };

        // Bucket nodes by the files that actually matched, so attribution is one
        // pass over the graph instead of a scan per hit.
        let hit_files: std::collections::HashSet<&str> =
            outcome.hits.iter().map(|h| h.graph_path.as_str()).collect();
        let mut by_file: HashMap<&str, Vec<&Node>> = HashMap::new();
        if !hit_files.is_empty() {
            for n in self.kg.nodes() {
                if hit_files.contains(n.source_file.as_str()) && n.span().is_some() {
                    by_file.entry(n.source_file.as_str()).or_default().push(n);
                }
            }
        }
        // The innermost (smallest) span containing the line wins.
        let enclosing = |file: &str, line: u64| -> Option<&Node> {
            let l = line as u32;
            by_file
                .get(file)?
                .iter()
                .filter(|n| {
                    n.span()
                        .map(|s| s.start_line <= l && l <= s.end_line)
                        .unwrap_or(false)
                })
                .min_by_key(|n| {
                    let s = n.span().unwrap();
                    s.end_line.saturating_sub(s.start_line)
                })
                .copied()
        };

        let total = outcome.hits.len();
        let files: std::collections::BTreeSet<&str> =
            outcome.hits.iter().map(|h| h.graph_path.as_str()).collect();
        let mut text = if total == 0 {
            format!(
                "search_text \"{}\": no matches in {} file(s) searched.",
                sanitize_label(pattern),
                outcome.files_scanned
            )
        } else {
            let note = if outcome.truncated {
                format!(
                    " (capped at {}; narrow the pattern or raise max_results)",
                    q.max_results
                )
            } else {
                String::new()
            };
            format!(
                "search_text \"{}\" -- {} match(es) in {} file(s):{}",
                sanitize_label(pattern),
                total,
                files.len(),
                note
            )
        };

        // The federation tags the graph knows about, so a hit can report its repo
        // even when the search ran over a single parent root (tag-less walk): the
        // enclosing node's `repo`, else the `tag/` prefix of the graph path.
        let known_repos: std::collections::HashSet<&str> =
            self.kg.nodes().filter_map(|n| n.repo.as_deref()).collect();
        let repo_of = |h: &search::RawHit, node: Option<&Node>| -> Option<String> {
            h.repo
                .clone()
                .or_else(|| node.and_then(|n| n.repo.clone()))
                .or_else(|| {
                    h.graph_path
                        .split_once('/')
                        .map(|(head, _)| head)
                        .filter(|head| known_repos.contains(head))
                        .map(str::to_string)
                })
        };

        let mut hits_json = Vec::with_capacity(total);
        for h in &outcome.hits {
            let node = enclosing(&h.graph_path, h.line);
            let attr = match node {
                Some(n) => format!(
                    "   [{} {}]",
                    sanitize_label(&n.label),
                    n.kind().map(|k| k.as_str()).unwrap_or("node")
                ),
                None => "   [no enclosing symbol]".to_string(),
            };
            text.push_str(&format!(
                "\n  {}:{}:{}  {}{}",
                sanitize_label(&h.graph_path),
                h.line,
                h.col,
                h.line_text,
                attr
            ));
            hits_json.push(json!({
                "repo": repo_of(h, node),
                "file": h.graph_path,
                "line": h.line,
                "col": h.col,
                "match": h.matched,
                "line_text": h.line_text,
                "node": node.map(|n| json!({
                    "id": n.id.0.as_str(),
                    "label": n.label,
                    "kind": n.kind().map(|k| k.as_str()),
                    "community": n.community,
                })),
            }));
        }

        let structured = json!({
            "pattern": pattern,
            "total": total,
            "truncated": outcome.truncated,
            "files_scanned": outcome.files_scanned,
            "hits": hits_json,
        });
        (text, structured)
    }

    /// `dynamic_hazards` — list reflection / dynamic-dispatch sites recorded on
    /// graph nodes, with optional `repo` / `path_glob` / `kind` / `target` filters.
    /// Reads sites off the graph (no source walk); renders text + structured from a
    /// single pass. Surfaces why a "0 dependents" answer can be incomplete.
    fn dynamic_hazards_dual(
        &self,
        repo: Option<&str>,
        path_glob: Option<&str>,
        kind: Option<&str>,
        target: Option<&str>,
        max_results: usize,
    ) -> (String, Value) {
        let cap = max_results.clamp(1, 1000);
        let empty = |msg: String| (msg, json!({ "total": 0, "truncated": false, "sites": [] }));

        let glob_re = match path_glob {
            Some(g) => match regex::Regex::new(&glob_to_regex(g)) {
                Ok(re) => Some(re),
                Err(e) => {
                    return empty(format!(
                        "dynamic_hazards: invalid path_glob: {}",
                        sanitize_label(&e.to_string())
                    ))
                }
            },
            None => None,
        };

        // target scoping: the symbol name (matches keyed sites) + the files that
        // define it (an opaque site there could reach it).
        let tnorm = target.map(hazard_bare);
        let target_files: std::collections::HashSet<String> = match &tnorm {
            Some(tn) => self
                .kg
                .nodes()
                .filter(|n| !n.source_file.is_empty() && hazard_bare(&n.label) == *tn)
                .map(|n| n.source_file.clone())
                .collect(),
            None => std::collections::HashSet::new(),
        };

        let mut total = 0usize;
        let mut truncated = false;
        let mut rows: Vec<HazardRow> = Vec::new();
        for n in self.kg.nodes() {
            if let Some(r) = repo {
                if n.repo.as_deref() != Some(r) {
                    continue;
                }
            }
            if n.source_file.is_empty() {
                continue;
            }
            if let Some(re) = &glob_re {
                if !re.is_match(&n.source_file.replace('\\', "/")) {
                    continue;
                }
            }
            for s in n.dynamic_sites() {
                let ks = s.kind.as_str();
                if let Some(k) = kind {
                    if ks != k {
                        continue;
                    }
                }
                if let Some(tn) = &tnorm {
                    let key_match = s.key.as_deref().is_some_and(|k| hazard_key_seg(k) == *tn);
                    let opaque_in_file = s.key.is_none() && target_files.contains(&n.source_file);
                    if !key_match && !opaque_in_file {
                        continue;
                    }
                }
                total += 1;
                if rows.len() < cap {
                    rows.push((
                        n.repo.clone().unwrap_or_default(),
                        n.source_file.clone(),
                        s.line,
                        ks,
                        s.key.clone(),
                        n.label.clone(),
                    ));
                } else {
                    truncated = true;
                }
            }
        }
        rows.sort_by(|a, b| a.1.cmp(&b.1).then(a.2.cmp(&b.2)).then(a.3.cmp(b.3)));

        let mut text = if total == 0 {
            "dynamic_hazards: no reflection / dynamic-dispatch sites match.".to_string()
        } else {
            let note = if truncated {
                format!(" (capped at {cap}; narrow with repo/path_glob/kind or raise max_results)")
            } else {
                String::new()
            };
            format!(
                "dynamic_hazards -- {total} site(s){note}:\n(a symbol with 0 static dependents here is not provably unused -- these dispatch dynamically)"
            )
        };
        let mut sites_json: Vec<Value> = Vec::with_capacity(rows.len());
        for (r, f, line, ks, key, host) in &rows {
            let keytxt = key
                .as_deref()
                .map(|k| format!("\"{}\"", sanitize_label(k)))
                .unwrap_or_else(|| "(opaque)".to_string());
            let repotxt = if r.is_empty() {
                String::new()
            } else {
                format!("[{}] ", sanitize_label(r))
            };
            text.push_str(&format!(
                "\n  {}{}:{}  {}  {}  in {}",
                repotxt,
                sanitize_label(f),
                line,
                ks,
                keytxt,
                sanitize_label(host)
            ));
            sites_json.push(json!({
                "repo": if r.is_empty() { Value::Null } else { json!(r) },
                "file": f,
                "line": line,
                "kind": ks,
                "key": key,
                "host": host,
            }));
        }
        (
            text,
            json!({ "total": total, "truncated": truncated, "sites": sites_json }),
        )
    }

    /// `get_neighbors` — in/out neighbours, optionally filtered by relation.
    pub fn tool_get_neighbors(
        &self,
        label: &str,
        relation_filter: Option<&str>,
        show_sites: bool,
        limit: usize,
        verbose: bool,
    ) -> String {
        let id = match self.resolve_or_msg(label) {
            Ok(id) => id,
            Err(msg) => return msg,
        };
        let Some(ex) = explain(&self.kg, &id) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        let seed = sanitize_label(&ex.label);
        let rel_filter = relation_filter.map(str::to_lowercase);
        let sites: SiteMap = if show_sites {
            let mut m = SiteMap::new();
            self.edge_sites(&id, &mut m);
            m
        } else {
            SiteMap::new()
        };
        // A hub can have hundreds of neighbors; cap the rendered list (default,
        // verbose=false) with a '+N more' summary so the output stays bounded,
        // mirroring find_callers/affected. verbose lists every neighbor.
        let cap = if verbose { usize::MAX } else { limit.max(1) };
        // Tally every relation on the node so a filter that excludes everything can
        // still report what the node DOES have. Only needed on the filtered path:
        // with no filter, an empty result means the node simply has no neighbors.
        let mut by_rel: BTreeMap<&str, usize> = BTreeMap::new();
        let mut body = String::new();
        let mut matched = 0usize;
        let mut rendered = 0usize;
        for nb in &ex.neighbors {
            if rel_filter.is_some() {
                *by_rel.entry(nb.relation.as_str()).or_default() += 1;
            }
            if let Some(f) = &rel_filter {
                // Case-insensitive substring (lowercase both sides).
                if !nb.relation.to_lowercase().contains(f.as_str()) {
                    continue;
                }
            }
            matched += 1;
            // Count the rest for the '+N more' line, but stop rendering past the cap.
            if rendered >= cap {
                continue;
            }
            rendered += 1;
            let arrow = if nb.direction == "out" { "-->" } else { "<--" };
            body.push_str(&format!(
                "\n  {} {} [{}]",
                arrow,
                sanitize_label(&nb.label),
                sanitize_label(&nb.relation)
            ));
            if show_sites {
                if let Some(site_list) =
                    sites.get(&(nb.id.clone(), nb.relation.clone(), nb.direction))
                {
                    body.push_str(&self.render_sites(site_list, "        ", 3));
                }
            }
        }
        if matched == 0 {
            // Distinguish "node exists but no edge matched the filter" from "no
            // such node". When a filter hid everything, name the relations the node
            // actually has so the empty list is not misread as a missing node.
            return match (relation_filter, by_rel.is_empty()) {
                (Some(f), false) => {
                    let avail = by_rel
                        .iter()
                        .map(|(r, c)| format!("{}({c})", sanitize_label(r)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        "Neighbors of {seed}:\n  (none with relation '{}'; this node has: {avail})",
                        sanitize_label(f)
                    )
                }
                _ => format!("Neighbors of {seed}:\n  (none)"),
            };
        }
        if matched > rendered {
            body.push_str(&format!(
                "\n  ... +{} more (pass verbose=true to list all, or relation_filter to narrow)",
                matched - rendered
            ));
        }
        format!("Neighbors of {seed} ({matched}):{body}")
    }

    /// Structured mirror of `get_neighbors`: the seed plus each in/out neighbor
    /// and a per-relation tally. Honors the same case-insensitive relation filter
    /// as the text path. `null` when the node does not resolve.
    fn get_neighbors_json(
        &self,
        label: &str,
        relation_filter: Option<&str>,
        limit: usize,
        verbose: bool,
    ) -> Value {
        let Ok(id) = self.resolve_or_msg(label) else {
            return Value::Null;
        };
        let Some(ex) = explain(&self.kg, &id) else {
            return Value::Null;
        };
        let rel_filter = relation_filter.map(str::to_lowercase);
        // Mirror the text path's cap so the structured channel cannot blow past
        // the budget on a hub; `total` carries the true matched count.
        let cap = if verbose { usize::MAX } else { limit.max(1) };
        let mut by_relation: BTreeMap<&str, usize> = BTreeMap::new();
        let mut neighbors = Vec::new();
        let mut matched = 0usize;
        for nb in &ex.neighbors {
            *by_relation.entry(nb.relation.as_str()).or_default() += 1;
            if let Some(f) = &rel_filter {
                if !nb.relation.to_lowercase().contains(f.as_str()) {
                    continue;
                }
            }
            matched += 1;
            if neighbors.len() >= cap {
                continue;
            }
            neighbors.push(json!({
                "label": nb.label,
                "relation": nb.relation,
                "direction": nb.direction,
            }));
        }
        let by_relation: serde_json::Map<String, Value> = by_relation
            .into_iter()
            .map(|(k, v)| (k.to_string(), Value::from(v)))
            .collect();
        json!({
            "seed": ex.label,
            "neighbors": neighbors,
            "by_relation": by_relation,
            "total": matched,
            "truncated": matched > neighbors.len(),
        })
    }

    /// `get_community` — a page of a community's members (`offset`/`limit`), so
    /// a large community cannot blow the context window. Uses the prebuilt,
    /// sorted community index (kept fresh across hot-reloads).
    pub fn tool_get_community(&self, community_id: u32, offset: usize, limit: usize) -> String {
        let Some(ids) = self
            .communities
            .get(&community_id)
            .filter(|v| !v.is_empty())
        else {
            return format!("No community {community_id}.");
        };
        let total = ids.len();
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        let page = &ids[start..end];
        let mut out = format!(
            "Community {community_id} (showing {} of {total}):",
            page.len()
        );
        for id in page {
            if let Some(n) = self.kg.node(id) {
                out.push_str(&format!(
                    "\n  {} [{}]",
                    sanitize_label(&n.label),
                    sanitize_label(&n.source_file)
                ));
            }
        }
        out
    }

    /// `god_nodes` — a page of the degree-ranked hub list (`offset` then `top_n`).
    /// Used by the resource endpoint; the tool dispatch renders both channels from
    /// `god_nodes_page` directly so the test-count walk runs only once.
    pub fn tool_god_nodes(&self, top_n: usize, offset: usize) -> String {
        let (rows, start) = self.god_nodes_page(top_n, offset);
        Self::render_god_nodes_text(&rows, start)
    }

    /// One page of the ranked hub list, each hub annotated with its transitive test
    /// count. `top_n` is clamped to [`GOD_NODES_PAGE_CAP`]: each row costs a
    /// depth-3 reverse-impact walk over a hub (the densest nodes), so an unbounded
    /// page could run a walk per node across the whole graph — page with `offset`
    /// instead. Returns the rows and the absolute start index (for 1-based ranks).
    fn god_nodes_page(&self, top_n: usize, offset: usize) -> (Vec<GodNodeRow>, usize) {
        let total = self.god_nodes_all.len();
        let start = offset.min(total);
        // `max(1)` keeps the historical "one node even for top_n == 0" behavior;
        // `min(cap)` bounds the per-page reverse-impact work.
        let cap = top_n.clamp(1, GOD_NODES_PAGE_CAP);
        let end = start.saturating_add(cap).min(total);
        let rows = self.god_nodes_all[start..end]
            .iter()
            .map(|g| GodNodeRow {
                id: g.id.clone(),
                label: g.label.clone(),
                degree: g.degree,
                test_count: self.test_count_for(&g.id),
                dynamically_referenced: self
                    .kg
                    .node(&g.id)
                    .is_some_and(|n| n.dynamically_referenced()),
            })
            .collect();
        (rows, start)
    }

    /// Render a god-node page as text (`God nodes:` then one ranked line each).
    fn render_god_nodes_text(rows: &[GodNodeRow], start: usize) -> String {
        if rows.is_empty() {
            return "No nodes.".to_string();
        }
        let mut out = String::from(
            "God nodes: most-connected hubs by total degree. Degree counts ALL edges incl. a class's members, so it is structural centrality/size, not an incoming-dependence count -- use `affected` for blast radius.",
        );
        for (i, g) in rows.iter().enumerate() {
            let dyn_note = if g.dynamically_referenced {
                " [reached via dynamic dispatch -- low static dependence is not safety]"
            } else {
                ""
            };
            out.push_str(&format!(
                "\n  {}. {} - {} connections, {} test(s){}",
                start + i + 1,
                sanitize_label(&g.label),
                g.degree,
                g.test_count,
                dyn_note
            ));
        }
        out
    }

    /// Render a god-node page as the structured `{ god_nodes: [...] }` mirror.
    fn render_god_nodes_json(rows: &[GodNodeRow]) -> Value {
        let arr: Vec<Value> = rows
            .iter()
            .map(|g| {
                let mut o = serde_json::Map::new();
                o.insert("label".into(), json!(sanitize_label(&g.label)));
                o.insert("degree".into(), json!(g.degree));
                o.insert("id".into(), json!(sanitize_label(&g.id.0)));
                o.insert("test_count".into(), json!(g.test_count));
                if g.dynamically_referenced {
                    o.insert("dynamically_referenced".into(), json!(true));
                }
                Value::Object(o)
            })
            .collect();
        json!({ "god_nodes": arr })
    }

    /// Number of test nodes that transitively exercise `id`, found by walking the
    /// reverse-impact set (its dependents) and keeping the ones on a test path.
    /// Uses the cached reverse-impact index, so it is O(reached) per node — cheap
    /// enough to annotate the handful of god nodes a page renders. The depth matches
    /// the default impact forecast depth so the count agrees with `affected_tests`.
    fn test_count_for(&self, id: &NodeId) -> usize {
        const GOD_TEST_DEPTH: usize = 3;
        self.affected_index
            .affected_multi(&self.kg, std::slice::from_ref(id), GOD_TEST_DEPTH)
            .iter()
            .filter(|h| {
                self.kg
                    .node(&h.node_id)
                    .map(|n| n.is_test())
                    .unwrap_or(false)
            })
            .count()
    }

    /// `graph_stats` — counts + confidence breakdown (+ cross-repo coupling on a
    /// federated graph).
    pub fn tool_graph_stats(&self) -> String {
        let s = &self.stats;
        let mut out = format!(
            "Graph: {} nodes, {} edges, {} communities\nEdges: {} EXTRACTED, {} INFERRED, {} AMBIGUOUS",
            s.nodes, s.edges, s.communities, s.extracted, s.inferred, s.ambiguous
        );
        // Only meaningful on a federated (multi-repo) graph; silent otherwise.
        if s.cross_repo > 0 {
            out.push_str(&format!(
                "\nCross-repo: {} edge(s) span repositories ({} cross-language: HTTP/RPC/FFI/WebSocket boundaries)",
                s.cross_repo, s.cross_language
            ));
        }
        let (total, opaque, linked) = self.dynamic_stats();
        if total > 0 {
            out.push_str(&format!(
                "\nDynamic-dispatch sites: {total} ({opaque} opaque, {linked} evidence-linked) -- see dynamic_hazards; 0 dependents on a dynamically-dispatched symbol is not proof it is unused"
            ));
        }
        out
    }

    /// `(total dynamic-dispatch sites, opaque sites, evidence-linked dynamic_ref
    /// edges)` across the graph, for the stats surfaces.
    fn dynamic_stats(&self) -> (usize, usize, usize) {
        let total: usize = self.kg.nodes().map(|n| n.dynamic_sites().len()).sum();
        let opaque = self.hazard_index.opaque_total();
        let linked = self
            .kg
            .edges()
            .filter(|e| e.relation == "dynamic_ref")
            .count();
        (total, opaque, linked)
    }

    /// `list_repos` — federated members with node/edge counts.
    /// Edges are counted under their source node's repo. Empty for a single-repo
    /// graph (no `repo` tags).
    pub fn tool_list_repos(&self) -> String {
        use std::collections::{BTreeMap, HashMap};
        let node_repo: HashMap<&str, &str> = self
            .kg
            .nodes()
            .filter_map(|n| n.repo.as_deref().map(|r| (n.id.0.as_str(), r)))
            .collect();
        let mut counts: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for n in self.kg.nodes() {
            if let Some(r) = n.repo.as_deref() {
                counts.entry(r).or_default().0 += 1;
            }
        }
        for e in self.kg.edges() {
            if let Some(r) = node_repo.get(e.source.0.as_str()) {
                counts.entry(r).or_default().1 += 1;
            }
        }
        if counts.is_empty() {
            return "No federated repos (single-repo graph).".to_string();
        }
        let mut out = format!("Repos ({}):", counts.len());
        for (repo, (n, ed)) in &counts {
            let fresh = self
                .repo_hashes
                .get(*repo)
                .map(|h| format!(", src {h}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "\n  {} - {n} nodes, {ed} edges{fresh}",
                sanitize_label(repo)
            ));
        }
        if !self.repo_hashes.is_empty() {
            out.push_str("\n(src = per-repo source fingerprint from workspace-state.json; changes when that repo's sources change.)");
        }
        out
    }

    /// Structured mirror of `list_repos`: `{ repos: [{ repo, nodes, edges }] }`,
    /// an empty array for a single-repo graph.
    fn list_repos_json(&self) -> Value {
        let node_repo: HashMap<&str, &str> = self
            .kg
            .nodes()
            .filter_map(|n| n.repo.as_deref().map(|r| (n.id.0.as_str(), r)))
            .collect();
        let mut counts: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for n in self.kg.nodes() {
            if let Some(r) = n.repo.as_deref() {
                counts.entry(r).or_default().0 += 1;
            }
        }
        for e in self.kg.edges() {
            if let Some(r) = node_repo.get(e.source.0.as_str()) {
                counts.entry(r).or_default().1 += 1;
            }
        }
        let repos: Vec<Value> = counts
            .into_iter()
            .map(|(repo, (nodes, edges))| {
                let mut obj = serde_json::Map::new();
                obj.insert("repo".into(), json!(repo));
                obj.insert("nodes".into(), json!(nodes));
                obj.insert("edges".into(), json!(edges));
                if let Some(h) = self.repo_hashes.get(repo) {
                    obj.insert("source_hash".into(), json!(h));
                }
                Value::Object(obj)
            })
            .collect();
        json!({ "repos": repos })
    }

    /// `repo_stats` — node/edge counts for one federated member.
    pub fn tool_repo_stats(&self, repo: &str) -> String {
        use std::collections::HashSet;
        let ids: HashSet<&str> = self
            .kg
            .nodes()
            .filter(|n| n.repo.as_deref() == Some(repo))
            .map(|n| n.id.0.as_str())
            .collect();
        if ids.is_empty() {
            return format!("No nodes for repo {}.", sanitize_label(repo));
        }
        let edges = self
            .kg
            .edges()
            .filter(|e| ids.contains(e.source.0.as_str()))
            .count();
        format!(
            "Repo {}: {} nodes, {edges} edges",
            sanitize_label(repo),
            ids.len()
        )
    }

    /// `shortest_path` — keyword-resolved source→target path, ≤ max_hops.
    pub fn tool_shortest_path(&self, source: &str, target: &str, max_hops: usize) -> String {
        let from = match self.resolve_or_msg(source) {
            Ok(id) => id,
            Err(msg) => return format!("source: {msg}"),
        };
        let to = match self.resolve_or_msg(target) {
            Ok(id) => id,
            Err(msg) => return format!("target: {msg}"),
        };
        if from == to {
            return "Source and target resolve to the same node.".to_string();
        }
        match shortest_path(&self.kg, &from, &to) {
            Some(path) => {
                let hops = path.len().saturating_sub(1);
                if hops > max_hops {
                    return format!(
                        "Shortest path is {hops} hops, over the max_hops={max_hops} limit."
                    );
                }
                // Annotate each hop with its connecting relation so a path built
                // from low-signal `references` (type) edges is self-evident rather
                // than looking like an authoritative call chain.
                let mut rendered = sanitize_label(&self.label_of(&path[0]));
                for pair in path.windows(2) {
                    let rel = self
                        .relation_between(&pair[0], &pair[1])
                        .unwrap_or_else(|| "?".to_string());
                    rendered.push_str(&format!(
                        " -[{}]-> {}",
                        sanitize_label(&rel),
                        sanitize_label(&self.label_of(&pair[1]))
                    ));
                }
                format!("Shortest path ({hops} hops): {rendered}")
            }
            None => format!(
                "No path between {} and {}.",
                sanitize_label(&self.label_of(&from)),
                sanitize_label(&self.label_of(&to))
            ),
        }
    }

    /// The relation connecting two adjacent path nodes, picking the most
    /// meaningful one deterministically when several edges connect them: calls >
    /// inheritance > imports > uses/depends > references > other, ties broken
    /// lexicographically. Used to annotate `shortest_path` hops.
    fn relation_between(&self, a: &NodeId, b: &NodeId) -> Option<String> {
        fn priority(rel: &str) -> u8 {
            let r = rel.to_lowercase();
            if r.contains("call") {
                0
            } else if r.contains("inherit") || r.contains("implement") || r.contains("extend") {
                1
            } else if r.contains("import") {
                2
            } else if r.contains("use") || r.contains("depend") {
                3
            } else if r.contains("reference") {
                4
            } else {
                5
            }
        }
        self.kg
            .incident_edges(a)
            .filter(|e| (&e.source == a && &e.target == b) || (&e.source == b && &e.target == a))
            .map(|e| e.relation.clone())
            .min_by(|x, y| priority(x).cmp(&priority(y)).then_with(|| x.cmp(y)))
    }

    /// `find_callers` — who calls/uses this node (incoming call-like edges).
    pub fn tool_find_callers(
        &self,
        label: &str,
        limit: usize,
        verbose: bool,
        show_sites: bool,
    ) -> String {
        self.directional("Callers", label, "in", limit, verbose, show_sites)
    }

    /// `find_callees` — what this node calls/uses (outgoing call-like edges).
    pub fn tool_find_callees(
        &self,
        label: &str,
        limit: usize,
        verbose: bool,
        show_sites: bool,
    ) -> String {
        self.directional("Callees", label, "out", limit, verbose, show_sites)
    }

    fn directional(
        &self,
        title: &str,
        label: &str,
        dir: &str,
        limit: usize,
        verbose: bool,
        show_sites: bool,
    ) -> String {
        let id = match self.resolve_or_msg(label) {
            Ok(id) => id,
            Err(msg) => return msg,
        };
        if self.kg.node(&id).is_none() {
            return format!("No node matches '{}'.", sanitize_label(label));
        }
        let seed = sanitize_label(&self.label_of(&id));
        // For a type node, fold its members in: a class's callers/callees attach
        // to its methods, not the bare type symbol. The focus set (type + members)
        // is excluded from results so we list EXTERNAL callers/callees, not the
        // class's own internal structure.
        let members = self.type_members(&id);
        let note = self.class_fold_note(&id, &seed, members.len());
        let mut focus: Vec<NodeId> = Vec::with_capacity(members.len() + 1);
        focus.push(id.clone());
        focus.extend(members.iter().cloned());
        let focus_set: std::collections::HashSet<&NodeId> = focus.iter().collect();
        // Collect call-like neighbors in the requested direction across the focus
        // set, deduped by (neighbor, relation) so a node reached via several
        // members appears once per relation. Owned strings since each focus node's
        // `explain` is a separate borrow.
        let mut seen: std::collections::HashSet<(NodeId, String)> =
            std::collections::HashSet::new();
        // The neighbor id is only needed to look up its call sites, so it is
        // cloned only when show_sites is on -- the default path pays nothing extra.
        let mut hits: Vec<(Option<NodeId>, String, String)> = Vec::new();
        let mut by_rel: BTreeMap<String, usize> = BTreeMap::new();
        for f in &focus {
            let Some(ex) = explain(&self.kg, f) else {
                continue;
            };
            for nb in &ex.neighbors {
                let rel = nb.relation.to_lowercase();
                let call_like =
                    rel.contains("call") || rel.contains("use") || rel.contains("reference");
                if nb.direction != dir || !call_like || focus_set.contains(&nb.id) {
                    continue;
                }
                if !seen.insert((nb.id.clone(), nb.relation.clone())) {
                    continue;
                }
                *by_rel.entry(nb.relation.clone()).or_default() += 1;
                hits.push((
                    show_sites.then(|| nb.id.clone()),
                    nb.label.clone(),
                    nb.relation.clone(),
                ));
            }
        }
        if hits.is_empty() {
            return format!("{note}{title} of {seed}:\n  (none)");
        }
        // For show_sites, gather the call sites on every focus node's edges once,
        // keyed by (neighbor, relation, direction), so each row can show where the
        // call actually happens.
        let sites: SiteMap = if show_sites {
            let mut m = SiteMap::new();
            for f in &focus {
                self.edge_sites(f, &mut m);
            }
            m
        } else {
            SiteMap::new()
        };
        // Per-relation breakdown only when it adds information (>1 kind), mirroring
        // affected's per-depth breakdown.
        let breakdown = if by_rel.len() > 1 {
            let parts = by_rel
                .iter()
                .map(|(r, c)| format!("{}: {c}", sanitize_label(r)))
                .collect::<Vec<_>>()
                .join(", ");
            format!(" [{parts}]")
        } else {
            String::new()
        };
        // Top-N by default; verbose dumps all. Mirrors `affected`.
        let cap = if verbose { usize::MAX } else { limit.max(1) };
        let total = hits.len();
        let mut out = format!("{note}{total} {title} of {seed}{breakdown}:");
        for (nid, lbl, rel) in hits.iter().take(cap) {
            out.push_str(&format!(
                "\n  {} [{}]",
                sanitize_label(lbl),
                sanitize_label(rel)
            ));
            if let Some(nid) = nid {
                if let Some(site_list) = sites.get(&(nid.clone(), rel.clone(), dir)) {
                    out.push_str(&self.render_sites(site_list, "      ", 3));
                }
            }
        }
        if total > cap {
            out.push_str(&format!(
                "\n  ... (+{} more; pass verbose=true for the full list)",
                total - cap
            ));
        }
        // For callees, when not one hit is an actual call edge (only type/reference
        // uses survive the call_like filter), say so: a bare "N Callees" otherwise
        // reads as "this function calls N things". Calls into std / third-party
        // symbols are not graph nodes, so they cannot appear here.
        if title == "Callees" && !by_rel.keys().any(|r| r.to_lowercase().contains("call")) {
            out.push_str(
                "\n  note: no in-graph callee functions; the entries above are type/reference uses (calls to std or third-party symbols are not graph nodes).",
            );
        }
        out
    }

    /// `find_references` — every place a symbol is used (the "find all
    /// references" primitive): all incoming non-ownership edges, including the
    /// import / inheritance / type-use edges `find_callers` omits. Direct
    /// references to the symbol itself; a type's members are NOT folded in.
    pub fn tool_find_references(
        &self,
        label: &str,
        limit: usize,
        verbose: bool,
        show_sites: bool,
    ) -> String {
        let id = match self.resolve_or_msg(label) {
            Ok(id) => id,
            Err(msg) => return msg,
        };
        if self.kg.node(&id).is_none() {
            return format!("No node matches '{}'.", sanitize_label(label));
        }
        let seed = sanitize_label(&self.label_of(&id));
        let refs = references_to(&self.kg, &id);
        if refs.is_empty() {
            return format!("No references to {seed}.");
        }
        let mut by_rel: BTreeMap<String, usize> = BTreeMap::new();
        for r in &refs {
            *by_rel.entry(r.relation.clone()).or_default() += 1;
        }
        // Gather the reference sites once for show_sites, keyed like `directional`.
        let sites: SiteMap = if show_sites {
            let mut m = SiteMap::new();
            self.edge_sites(&id, &mut m);
            m
        } else {
            SiteMap::new()
        };
        // Per-relation breakdown only when it adds information (>1 kind).
        let breakdown = if by_rel.len() > 1 {
            let parts = by_rel
                .iter()
                .map(|(r, c)| format!("{}: {c}", sanitize_label(r)))
                .collect::<Vec<_>>()
                .join(", ");
            format!(" [{parts}]")
        } else {
            String::new()
        };
        let cap = if verbose { usize::MAX } else { limit.max(1) };
        let total = refs.len();
        let mut out = format!("{total} references to {seed}{breakdown}:");
        for r in refs.iter().take(cap) {
            out.push_str(&format!(
                "\n  {} [{}]",
                sanitize_label(&r.label),
                sanitize_label(&r.relation)
            ));
            if show_sites {
                if let Some(site_list) = sites.get(&(r.id.clone(), r.relation.clone(), "in")) {
                    out.push_str(&self.render_sites(site_list, "      ", 3));
                }
            }
        }
        if total > cap {
            out.push_str(&format!(
                "\n  ... (+{} more; pass verbose=true for the full list)",
                total - cap
            ));
        }
        out
    }

    /// `affected` — the nodes that transitively depend on `label`, found by
    /// walking impact edges backward up to `depth` hops. Empty `relations`
    /// uses the default structural-impact set.
    pub fn tool_affected(
        &self,
        label: &str,
        depth: usize,
        relations: &[String],
        limit: usize,
        verbose: bool,
    ) -> String {
        let id = match self.resolve_or_msg(label) {
            Ok(id) => id,
            Err(msg) => return msg,
        };
        let rels: Vec<&str> = if relations.is_empty() {
            DEFAULT_AFFECTED_RELATIONS.to_vec()
        } else {
            relations.iter().map(String::as_str).collect()
        };
        let depth = depth.clamp(1, 16);
        let (hits, member_count) = self.affected_for(&id, &rels, depth);
        let seed = sanitize_label(&self.label_of(&id));
        let note = self.class_fold_note(&id, &seed, member_count);
        if hits.is_empty() {
            let mut msg = format!("{note}Nothing depends on {seed} within {depth} hops.");
            if let Some(c) = self.dynamic_caveat_for(&id) {
                msg.push_str(&format!("\n  note: {}", c.message));
            }
            return msg;
        }
        // Per-depth breakdown so a hub's blast radius is summarized even when the
        // entry list is truncated.
        let mut by_depth: BTreeMap<usize, usize> = BTreeMap::new();
        for h in &hits {
            *by_depth.entry(h.depth).or_default() += 1;
        }
        let breakdown = by_depth
            .iter()
            .map(|(d, c)| format!("depth {d}: {c}"))
            .collect::<Vec<_>>()
            .join(", ");
        // Top-N by default (hits are ordered shallowest-first); verbose dumps all.
        let cap = if verbose { usize::MAX } else { limit.max(1) };
        let mut out = format!(
            "{note}{} nodes depend on {seed} (<= {depth} hops) [{breakdown}]:",
            hits.len()
        );
        for h in hits.iter().take(cap) {
            out.push_str(&format!(
                "\n  [{}h via {}] {}",
                h.depth,
                sanitize_label(&h.via_relation),
                sanitize_label(&self.label_of(&h.node_id))
            ));
        }
        if hits.len() > cap {
            out.push_str(&format!(
                "\n  ... (+{} more; pass verbose=true for the full list)",
                hits.len() - cap
            ));
        }
        out
    }

    // PR tools (via synaptic-prs; data-only, no LLM)

    fn resolve_base(&self, base: Option<&str>, repo: Option<&str>) -> String {
        match base {
            Some(b) => b.to_string(),
            None => detect_default_branch(&*self.runner, repo),
        }
    }

    fn graph_impact(&self, files: &[String], code_only: bool) -> (Vec<u32>, usize) {
        compute_pr_impact(
            self.kg
                .nodes()
                .filter(|n| !code_only || n.file_type == synaptic_core::FileType::Code)
                .map(|n| (n.source_file.as_str(), n.community)),
            files,
        )
    }

    /// `working_changes_impact` — graph blast radius of the working-tree diff
    /// against `base` (default: the detected default branch). `git diff <base>`
    /// covers the branch's committed work plus uncommitted edits, the same set a
    /// PR would. Uses `git`, not `gh`, so it works offline and before any PR.
    pub fn tool_working_changes_impact(
        &self,
        base: Option<&str>,
        limit: usize,
        verbose: bool,
        code_only: bool,
    ) -> String {
        let base = self.resolve_base(base, None);
        // Probe for a usable repo first so a missing/failed git (e.g. the
        // top-level dir of a federated workspace is not itself a repo) reads as a
        // distinct outcome from a genuinely clean tree, instead of both
        // collapsing to "no changes" once the lossy runner maps failure to "".
        let in_repo = self
            .runner
            .run("git", &["rev-parse", "--is-inside-work-tree"])
            .map(|s| s.trim() == "true")
            .unwrap_or(false);
        if !in_repo {
            return "git unavailable or not a git repository (in a federated workspace the top-level dir is not a repo; run inside a member repo). Graph audit continues offline.".to_string();
        }
        let diff = self.runner.run("git", &["diff", "--name-only", &base]);
        let files: Vec<String> = diff
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        if files.is_empty() {
            return format!("No changes vs {base}.");
        }
        let (comms, nodes) = self.graph_impact(&files, code_only);
        let scope = if code_only { " code" } else { "" };
        let mut out = format!(
            "Working changes vs {base}: {} files, {nodes}{scope} graph nodes, {} communities touched",
            files.len(),
            comms.len()
        );
        for f in &files {
            out.push_str(&format!("\n  {}", sanitize_label(f)));
        }
        // Opt-in: list the top touched nodes (most-connected first) and the
        // touched communities with human-readable labels. Default output stays
        // files-only to preserve behavior.
        if verbose {
            self.append_working_impact_detail(&mut out, &files, limit, code_only);
        }
        out
    }

    /// Append `Top nodes` (touched nodes ranked by edge degree) and
    /// `Communities` (labeled) detail to a `working_changes_impact` report.
    fn append_working_impact_detail(
        &self,
        out: &mut String,
        files: &[String],
        limit: usize,
        code_only: bool,
    ) {
        use std::collections::HashMap;
        // Touched nodes: those whose source_file path-matches a changed file
        // (same boundary-safe match `graph_impact` uses). With `code_only`, drop
        // non-code nodes (config/docs) so the list matches the filtered count.
        let touched: Vec<&synaptic_core::Node> = self
            .kg
            .nodes()
            .filter(|n| !code_only || n.file_type == synaptic_core::FileType::Code)
            .filter(|n| {
                files
                    .iter()
                    .any(|f| synaptic_prs::path_match(&n.source_file, f))
            })
            .collect();
        if touched.is_empty() {
            return;
        }
        // Degree = incident edges (in + out), one pass over the edge list.
        let mut degree: HashMap<&str, usize> = HashMap::new();
        for e in self.kg.edges() {
            *degree.entry(e.source.0.as_str()).or_default() += 1;
            *degree.entry(e.target.0.as_str()).or_default() += 1;
        }
        let deg = |n: &synaptic_core::Node| degree.get(n.id.0.as_str()).copied().unwrap_or(0);
        let mut ranked = touched.clone();
        ranked.sort_by(|a, b| deg(b).cmp(&deg(a)).then_with(|| a.label.cmp(&b.label)));

        out.push_str(&format!("\nTop nodes ({}):", touched.len()));
        for n in ranked.iter().take(limit) {
            let kind = n.kind().map(|k| k.as_str()).unwrap_or("node");
            out.push_str(&format!(
                "\n  {} [{}] {} ({} edges)",
                sanitize_label(&n.label),
                sanitize_label(kind),
                sanitize_label(&n.source_file),
                deg(n)
            ));
        }
        if touched.len() > limit {
            out.push_str(&format!("\n  ... (+{} more)", touched.len() - limit));
        }

        let labels = synaptic_prs::build_community_labels(
            touched.iter().map(|n| (n.label.as_str(), n.community)),
            5,
        );
        if !labels.is_empty() {
            out.push_str(&format!("\nCommunities ({}):", labels.len()));
            for (cid, lbls) in &labels {
                let shown: Vec<String> = lbls.iter().map(|l| sanitize_label(l)).collect();
                out.push_str(&format!("\n  community {cid}: {}", shown.join(", ")));
            }
        }
    }

    /// `predict_impact` - forecast the consequences of a change before applying
    /// it. Given `files` (or the working-tree diff vs `base`), maps them to graph
    /// nodes, walks the reverse-impact blast radius, and flags public APIs at
    /// risk plus a verify checklist. Pure-graph and read-only; use
    /// `time_travel_diff` or the `synaptic predict` CLI for cycle / removed-API
    /// detection (those build worktrees).
    /// The changed-file set for the predict tools: explicit `files`, else the
    /// working-tree diff vs `base` (the detected default branch by default).
    fn changed_from_args(&self, files: &[String], base: Option<&str>) -> Vec<String> {
        if !files.is_empty() {
            return files.to_vec();
        }
        let base = self.resolve_base(base, None);
        self.runner
            .run("git", &["diff", "--name-only", &base])
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Build the change forecast shared by `predict_impact` and `affected_tests`:
    /// resolve the changed-file set, then walk the reverse-impact blast radius.
    /// `None` means there were no changed files (each caller phrases that itself).
    fn build_forecast(
        &self,
        files: &[String],
        base: Option<&str>,
        depth: usize,
    ) -> Option<ChangeForecast> {
        let changed = self.changed_from_args(files, base);
        if changed.is_empty() {
            return None;
        }
        let opts = ForecastOptions {
            depth: depth.clamp(1, 16),
            ..Default::default()
        };
        Some(forecast_changes_with_index(
            &self.kg,
            &self.affected_index,
            &changed,
            &opts,
        ))
    }

    pub fn tool_predict_impact(
        &self,
        files: &[String],
        base: Option<&str>,
        depth: usize,
        limit: usize,
        verbose: bool,
    ) -> String {
        let Some(f) = self.build_forecast(files, base, depth) else {
            return "No changed files to forecast (pass `files`, or run on a branch with a diff vs the base)."
                .to_string();
        };
        // Per-section display cap. `verbose` shows everything; otherwise each list
        // is truncated to `limit` with a "+N more" note so the payload stays small.
        let cap = if verbose { usize::MAX } else { limit.max(1) };
        Self::render_predict_text(&f, cap)
    }

    /// Render a `predict_impact` forecast to text, each list capped at `cap`.
    fn render_predict_text(f: &ChangeForecast, cap: usize) -> String {
        let mut out = format!("Forecast: {}", sanitize_label(&f.summary));
        if let Some(r) = &f.risk {
            out.push_str(&format!("\nChange risk: {} ({}/100)", r.level, r.score));
            for factor in &r.factors {
                out.push_str(&format!("\n  - {}", sanitize_label(factor)));
            }
        }
        // Render a labelled "name (file)" section, capped unless verbose.
        let push_section = |out: &mut String, header: &str, items: &[(String, String)]| {
            if items.is_empty() {
                return;
            }
            out.push_str(&format!("\n{} ({}):", header, items.len()));
            for (label, file) in items.iter().take(cap) {
                out.push_str(&format!(
                    "\n  {} ({})",
                    sanitize_label(label),
                    sanitize_label(file)
                ));
            }
            if items.len() > cap {
                out.push_str(&format!(
                    "\n  ... (+{} more; pass verbose=true for the full list)",
                    items.len() - cap
                ));
            }
        };
        let pairs = |xs: &[synaptic_predict::NodeRef]| -> Vec<(String, String)> {
            xs.iter()
                .map(|n| (n.label.clone(), n.file.clone()))
                .collect()
        };
        push_section(&mut out, "Changed nodes", &pairs(&f.changed_nodes));
        push_section(&mut out, "Public API at risk", &pairs(&f.public_api_breaks));
        push_section(
            &mut out,
            "Tests at risk",
            &f.at_risk_tests
                .iter()
                .map(|h| (h.label.clone(), h.file.clone()))
                .collect::<Vec<_>>(),
        );
        out.push_str(&format!(
            "\nBlast radius ({} at-risk dependent(s)):",
            f.blast_radius.len()
        ));
        for h in f.blast_radius.iter().take(cap) {
            out.push_str(&format!(
                "\n  [{}h via {}] {} ({})",
                h.depth,
                sanitize_label(&h.via_relation),
                sanitize_label(&h.label),
                sanitize_label(&h.file)
            ));
        }
        if f.blast_radius.len() > cap {
            out.push_str(&format!(
                "\n  ... (+{} more; pass verbose=true for the full list)",
                f.blast_radius.len() - cap
            ));
        }
        if !f.verify_checklist.is_empty() {
            out.push_str("\nVerify checklist:");
            for step in &f.verify_checklist {
                out.push_str(&format!(
                    "\n  - {}\n    {}",
                    sanitize_label(&step.description),
                    sanitize_label(&step.command)
                ));
            }
        }
        out
    }

    /// The command-running speculative-execution tool (only reachable when the
    /// server was started with `--allow-exec`). Applies the change in a throwaway
    /// worktree under the source root and runs the forecast's at-risk tests plus a
    /// build/type-check, reporting real pass/fail. NOT read-only.
    // The parameters map 1:1 to the MCP input schema; a wrapper struct would only
    // add indirection over what is a thin dispatch shim.
    #[allow(clippy::too_many_arguments)]
    pub fn tool_speculate(
        &self,
        files: &[String],
        base: Option<&str>,
        test_cmd: Option<&str>,
        check_cmd: Option<&str>,
        depth: usize,
        timeout_secs: u64,
        max_tests: usize,
    ) -> String {
        let Some(root) = self.source_root.clone() else {
            return "Speculative execution needs a source root; start the server with --source-root <repo>.".to_string();
        };
        let changed = self.changed_from_args(files, base);
        if changed.is_empty() {
            return "No changed files to speculate (pass `files`, or run on a branch with a diff vs the base).".to_string();
        }
        let opts = ForecastOptions {
            depth: depth.clamp(1, 16),
            ..Default::default()
        };
        let forecast = forecast_changes_with_index(&self.kg, &self.affected_index, &changed, &opts);
        let mut seen = std::collections::HashSet::new();
        let mut test_files = Vec::new();
        for h in &forecast.at_risk_tests {
            if seen.insert(h.file.clone()) {
                test_files.push(h.file.clone());
            }
        }
        // Explicit `files` scope both the at-risk tests and the applied diff;
        // omitting them speculates the whole working-tree change vs the base.
        let paths = if files.is_empty() {
            Vec::new()
        } else {
            changed.clone()
        };
        let change = Change::WorkingTree {
            base: self.resolve_base(base, None),
            paths,
        };
        let sopts = SpeculateOptions {
            test_cmd: test_cmd.map(str::to_string),
            check_cmd: check_cmd.map(str::to_string),
            test_files,
            auto_detect: true,
            timeout: std::time::Duration::from_secs(timeout_secs.clamp(1, 3600)),
            max_tests,
            fail_fast: false,
            ..Default::default()
        };
        match speculate(&root, &change, &sopts) {
            Ok(report) => render_speculate_md(&report),
            Err(e) => format!(
                "Speculation could not run: {}",
                sanitize_label(&e.to_string())
            ),
        }
    }

    /// `affected_tests` - the tests that exercise the changed code (predictive
    /// test selection): walk the reverse-impact set from the changed files and
    /// keep the test nodes. The focused "what should I run for this change" view.
    pub fn tool_affected_tests(
        &self,
        files: &[String],
        base: Option<&str>,
        depth: usize,
    ) -> String {
        let Some(f) = self.build_forecast(files, base, depth) else {
            return "No changed files (pass `files`, or run on a branch with a diff vs the base)."
                .to_string();
        };
        Self::render_affected_tests_text(&f)
    }

    /// Render the test subset of a forecast for `affected_tests`.
    fn render_affected_tests_text(f: &ChangeForecast) -> String {
        if f.at_risk_tests.is_empty() {
            return "No tests in the graph exercise the changed code (within the impact depth)."
                .to_string();
        }
        let mut out = format!(
            "{} test(s) exercise the changed code:",
            f.at_risk_tests.len()
        );
        for h in &f.at_risk_tests {
            out.push_str(&format!(
                "\n  [{}h via {}] {} ({})",
                h.depth,
                sanitize_label(&h.via_relation),
                sanitize_label(&h.label),
                sanitize_label(&h.file)
            ));
        }
        out
    }

    /// `predict_edit` - what breaks if you delete / change the signature of /
    /// narrow the visibility of a symbol. Classifies dependents into "will break"
    /// and "to review". Pure-graph and read-only (no edit plan is produced).
    pub fn tool_predict_edit(
        &self,
        symbol: &str,
        kind: &str,
        depth: usize,
        limit: usize,
        verbose: bool,
    ) -> String {
        let Some(kind_enum) = EditKind::parse(kind) else {
            return format!(
                "Unknown edit kind '{}'. Use: delete, signature, visibility.",
                sanitize_label(kind)
            );
        };
        let Some(impact) = assess_edit(&self.kg, symbol, kind_enum, depth.clamp(1, 16)) else {
            // Surface candidates with file + degree inline when the name is
            // ambiguous, consistent with the other name-taking tools (the @file
            // hint covers disambiguation here).
            if let Resolution::Ambiguous(ids) = resolve_detailed(&self.kg, symbol) {
                let shown = ids.len().min(10);
                let lines: String = candidate_details(&self.kg, &ids[..shown])
                    .iter()
                    .map(|c| {
                        let file = if c.file.is_empty() {
                            "-".to_string()
                        } else {
                            sanitize_label(&c.file)
                        };
                        format!(
                            "\n  {} [{}] (degree {})",
                            sanitize_label(&c.id.0),
                            file,
                            c.degree
                        )
                    })
                    .collect();
                let more = if ids.len() > 10 {
                    format!("\n  +{} more", ids.len() - 10)
                } else {
                    String::new()
                };
                return format!(
                    "'{}' is ambiguous - {} candidates:{}{}\nQualify it as 'name@file-substring' (e.g. 'announce@core/foo.ts'), or pass a node id.",
                    sanitize_label(symbol),
                    ids.len(),
                    lines,
                    more
                );
            }
            return format!(
                "No node matches '{}'. If the name is shared by several files, qualify it as 'name@file-substring' (e.g. 'announce@core/foo.ts'), or pass a node id.",
                sanitize_label(symbol)
            );
        };
        let line = |d: &synaptic_predict::EditDependent| {
            format!(
                "\n  [{}h via {}] {} ({}) - {}",
                d.depth,
                sanitize_label(&d.via_relation),
                sanitize_label(&d.label),
                sanitize_label(&d.file),
                sanitize_label(&d.reason)
            )
        };
        // A "Nh: count" rollup over a dependent set, ascending by hop. Gives a
        // blast-radius shape that survives the per-section truncation.
        let by_depth = |items: &[synaptic_predict::EditDependent]| -> String {
            let mut counts: std::collections::BTreeMap<usize, usize> = Default::default();
            for d in items {
                *counts.entry(d.depth).or_default() += 1;
            }
            counts
                .iter()
                .map(|(d, c)| format!("{d}h: {c}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        // Per-section display cap mirrors affected/predict_impact: `verbose` shows
        // everything, otherwise each list is truncated to `limit` with a "+N more"
        // note so the payload stays small.
        let cap = if verbose { usize::MAX } else { limit.max(1) };
        let push_section =
            |out: &mut String, header: &str, items: &[synaptic_predict::EditDependent]| {
                if items.is_empty() {
                    return;
                }
                out.push_str(&format!(
                    "\n{} ({}) by depth: {}",
                    header,
                    items.len(),
                    by_depth(items)
                ));
                for d in items.iter().take(cap) {
                    out.push_str(&line(d));
                }
                if items.len() > cap {
                    out.push_str(&format!(
                        "\n  ... (+{} more; pass verbose=true for the full list)",
                        items.len() - cap
                    ));
                }
            };
        let mut out = sanitize_label(&impact.summary);
        push_section(&mut out, "Will break", &impact.breaks);
        push_section(&mut out, "Review", &impact.review);
        if impact.breaks.is_empty() && impact.review.is_empty() {
            out.push_str("\nNo dependents affected.");
        }
        out
    }

    /// `list_prs` — open PRs targeting the base, as text.
    pub fn tool_list_prs(&self, base: Option<&str>, repo: Option<&str>) -> String {
        let resolved = self.resolve_base(base, repo);
        match fetch_prs(&*self.runner, repo, Some(&resolved), 50) {
            Ok(prs) => format_prs_text(&prs, &resolved, today_epoch_days()),
            Err(e) => format!("Error: {e}"),
        }
    }

    /// `get_pr_impact` — one PR's detail + graph blast radius.
    pub fn tool_get_pr_impact(&self, number: u64, repo: Option<&str>) -> String {
        let resolved = self.resolve_base(None, repo);
        let Some(mut pr) = fetch_pr(&*self.runner, number, repo, &resolved) else {
            return format!("PR #{number} not found (gh unavailable, unauthenticated, or no such PR). Graph audit continues offline.");
        };
        pr.files_changed = fetch_pr_files(&*self.runner, number, repo);
        if pr.files_changed.is_empty() {
            return format!("PR #{number}: no changed files found (may require gh auth). Graph audit continues offline.");
        }
        let (comms, nodes) = self.graph_impact(&pr.files_changed, false);
        pr.communities_touched = comms;
        pr.nodes_affected = nodes;
        format_pr_detail(&pr, today_epoch_days(), 20)
    }

    /// `triage_prs` — actionable PRs ranked by status, with blast radius. Returns
    /// structured data + an instruction for the calling model to rank (the MCP
    /// host is itself the LLM; no LLM call here, unlike the CLI `prs --triage`).
    pub fn tool_triage_prs(&self, base: Option<&str>, repo: Option<&str>) -> String {
        let resolved = self.resolve_base(base, repo);
        let now = today_epoch_days();
        let prs = match fetch_prs(&*self.runner, repo, Some(&resolved), 50) {
            Ok(p) => p,
            Err(e) => return format!("Error: {e}"),
        };
        let worktrees = fetch_worktrees(&*self.runner);
        let mut actionable: Vec<PrInfo> = prs
            .into_iter()
            .filter(|p| {
                p.base_branch == resolved
                    && !matches!(p.classify(now), Status::WrongBase | Status::Stale)
            })
            .collect();
        if actionable.is_empty() {
            return format!("No actionable PRs targeting {resolved}.");
        }
        // Fetch each PR's changed files concurrently: the `gh pr diff`
        // subprocess is the dominant latency; the graph-impact step is cheap CPU
        // done afterwards. Bounded so a 50-PR triage can't spawn 50 processes at
        // once; order is preserved so it zips back onto `actionable`.
        let files = fetch_pr_files_concurrent(&*self.runner, &actionable, repo);
        // Build the source_file -> impact index once, then reuse it per PR (H5).
        let impact = ImpactIndex::build(
            self.kg
                .nodes()
                .map(|n| (n.source_file.as_str(), n.community)),
        );
        for (p, fc) in actionable.iter_mut().zip(files) {
            p.worktree_path = worktrees.get(&p.branch).cloned();
            p.files_changed = fc;
            let (comms, nodes) = impact.impact_for_files(&p.files_changed);
            p.communities_touched = comms;
            p.nodes_affected = nodes;
        }
        actionable.sort_by_key(|p| (p.classify(now).sort_rank(), p.days_old(now)));
        let mut out = format!(
            "Actionable PRs targeting {resolved}: {}\n\
             Rank these by review priority. Higher blast_radius = more graph communities \
             affected = higher merge risk.",
            actionable.len()
        );
        for p in &actionable {
            let br = p.blast_radius();
            let impact = if br.is_empty() {
                String::new()
            } else {
                format!("  blast_radius={br}")
            };
            let wt = match &p.worktree_path {
                Some(path) if !path.is_empty() => format!("  worktree={path}"),
                _ => String::new(),
            };
            out.push_str(&format!(
                "\n\nPR #{} [{}] CI={} review={} age={}d author={}{}{}\n  title: {}",
                p.number,
                p.classify(now).as_str(),
                p.ci_status.as_str(),
                if p.review_decision.is_empty() {
                    "none"
                } else {
                    &p.review_decision
                },
                p.days_old(now),
                sanitize_label(&p.author),
                impact,
                wt,
                sanitize_label(&p.title)
            ));
        }
        out
    }

    /// Fail-silent JSONL query log. Opt-in via `SYNAPTIC_QUERY_LOG`.
    fn log_query(&self, question: &str, nodes_returned: usize) {
        let Some(path) = &self.log_path else {
            return;
        };
        let line =
            json!({ "kind": "mcp_query", "question": question, "nodes_returned": nodes_returned });
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    // structured (typed) tool output, mirroring the text for outputSchema tools

    fn stats_json(&self) -> Value {
        let s = &self.stats;
        let (total, opaque, linked) = self.dynamic_stats();
        json!({
            "nodes": s.nodes, "edges": s.edges, "communities": s.communities,
            "extracted": s.extracted, "inferred": s.inferred, "ambiguous": s.ambiguous,
            "cross_repo": s.cross_repo, "cross_language": s.cross_language,
            "dynamic_sites": total, "dynamic_sites_opaque": opaque, "dynamic_refs_linked": linked
        })
    }

    fn affected_json(
        &self,
        label: &str,
        depth: usize,
        relations: &[String],
        limit: usize,
        verbose: bool,
    ) -> Value {
        // Resolve through the SAME unified resolver as the text path, so the
        // structured channel never silently picks one of several candidates and
        // returns a misleading empty/total:0 (which reads as "nothing depends on
        // it"). Ambiguity / not-found are reported explicitly instead.
        let id = match resolve_detailed(&self.kg, label) {
            Resolution::Unique(id) => id,
            Resolution::Ambiguous(ids) => {
                return json!({
                    "seed": sanitize_label(label),
                    "resolved": false,
                    "ambiguous": true,
                    "candidates": self.candidates_json(&ids),
                    "affected": [],
                    "total": 0,
                    "truncated": false
                });
            }
            Resolution::NotFound => {
                return json!({
                    "seed": sanitize_label(label),
                    "resolved": false,
                    "found": false,
                    "affected": [],
                    "total": 0,
                    "truncated": false
                });
            }
        };
        let rels: Vec<&str> = if relations.is_empty() {
            DEFAULT_AFFECTED_RELATIONS.to_vec()
        } else {
            relations.iter().map(String::as_str).collect()
        };
        // Fold a type's members in (mirrors the text path) so the structured blast
        // radius for a class is not a misleading zero.
        let (hits, member_count) = self.affected_for(&id, &rels, depth.clamp(1, 16));
        let total = hits.len();
        let cap = if verbose { usize::MAX } else { limit.max(1) };
        let mut by_depth: serde_json::Map<String, Value> = serde_json::Map::new();
        for h in &hits {
            let k = h.depth.to_string();
            let n = by_depth.get(&k).and_then(Value::as_u64).unwrap_or(0) + 1;
            by_depth.insert(k, json!(n));
        }
        let arr: Vec<Value> = hits
            .iter()
            .take(cap)
            .map(|h| {
                json!({
                    "label": sanitize_label(&self.label_of(&h.node_id)),
                    "depth": h.depth,
                    "via_relation": sanitize_label(&h.via_relation)
                })
            })
            .collect();
        let mut obj = serde_json::Map::new();
        obj.insert("seed".into(), json!(sanitize_label(&self.label_of(&id))));
        obj.insert("resolved".into(), json!(true));
        obj.insert("affected".into(), Value::Array(arr));
        obj.insert("total".into(), json!(total));
        obj.insert("truncated".into(), json!(total > cap));
        obj.insert("by_depth".into(), Value::Object(by_depth));
        if member_count > 0 {
            obj.insert("aggregated_over_members".into(), json!(member_count));
        }
        // When nothing statically depends on the seed, attach the honest
        // dynamic-dispatch caveat (if any) so a structured-only reader does not
        // treat total:0 as proof the symbol is unused.
        if total == 0 {
            if let Some(c) = self.dynamic_caveat_for(&id) {
                obj.insert(
                    "dynamic_caveat".into(),
                    serde_json::to_value(&c).unwrap_or(Value::Null),
                );
            }
        }
        Value::Object(obj)
    }

    /// Candidate list for an ambiguous structured resolution: `[{id, file,
    /// degree}]`, capped like the text path. Lets an agent reading only the
    /// structured channel disambiguate without a `get_node` round-trip.
    fn candidates_json(&self, ids: &[NodeId]) -> Value {
        let shown = ids.len().min(10);
        let arr: Vec<Value> = candidate_details(&self.kg, &ids[..shown])
            .iter()
            .map(|c| json!({ "id": c.id.0, "file": c.file, "degree": c.degree }))
            .collect();
        Value::Array(arr)
    }

    /// Typed mirror of [`tool_structural_search`](Server::tool_structural_search):
    /// runs the same SYNQL query / pattern and returns structured rows of resolved
    /// node views (label, kind, visibility, file, and the captured signature) so
    /// an agent can route on a function's shape without reading source. Aggregate
    /// queries return `groups` of scalar cells instead.
    fn structural_search_json(
        &self,
        query: Option<&str>,
        pattern: Option<&str>,
        file: Option<&str>,
        limit: usize,
    ) -> Value {
        let r = match self.structural_search_result(query, pattern, file) {
            Ok(r) => r,
            Err(e) => return json!({ "error": e, "results": [] }),
        };
        if let Some(agg) = &r.aggregates {
            let groups: Vec<Value> = agg
                .iter()
                .take(limit)
                .map(|row| Value::Array(row.iter().map(|c| json!(sanitize_label(c))).collect()))
                .collect();
            return json!({ "columns": r.columns, "groups": groups });
        }
        let results: Vec<Value> = r
            .node_views(&self.kg)
            .iter()
            .take(limit)
            .map(|row| Value::Array(row.iter().map(node_view_to_json).collect()))
            .collect();
        json!({ "columns": r.columns, "results": results })
    }

    /// Typed mirror of [`render_query_text`](Server::render_query_text) over the
    /// same filtered retrieval, so structuredContent stays consistent with the
    /// rendered text without re-querying.
    fn render_query_json(
        &self,
        r: &synaptic_query::QueryResult,
        keep: &[usize],
        recency: Option<&ResolvedRecency>,
        edge_cap: usize,
    ) -> Value {
        let in_set: std::collections::HashSet<&NodeId> =
            keep.iter().map(|&i| &r.nodes[i]).collect();
        let nodes: Vec<Value> = keep
            .iter()
            .filter_map(|&i| self.kg.node(&r.nodes[i]).map(|n| (i, n)))
            .map(|(i, n)| {
                let mut obj = json!({
                    "label": sanitize_label(&n.label),
                    "file_type": file_type_str(&n.file_type),
                    "source_file": sanitize_label(&n.source_file),
                    // Relevance score (higher = more relevant); nodes are already
                    // ordered by it so a caller can triage signal from noise.
                    "score": round2(r.scores.get(i).copied().unwrap_or(0.0)),
                    // True when `since` was given and this node's file changed on the
                    // current branch (its score was boosted accordingly).
                    "changed": recency.is_some_and(|rr| rr.changed.contains(&r.nodes[i]))
                });
                // An external stub (unresolved import target / third-party package)
                // has no source file and cannot be opened with get_source; flag it so
                // it is not mistaken for a navigable symbol. Emitted only when true to
                // keep the structured channel terse.
                if n.is_external_stub() {
                    obj["external_stub"] = json!(true);
                }
                obj
            })
            .collect();
        // Mirror the text path's edge cap so the structured channel stays bounded
        // (edge_cap == 0 => no edges, the terse default).
        let edges: Vec<Value> = r
            .edges
            .iter()
            .filter(|e| in_set.contains(&e.source) && in_set.contains(&e.target))
            .take(edge_cap)
            .map(|e| {
                json!({
                    "source": sanitize_label(&self.label_of(&e.source)),
                    "relation": sanitize_label(&e.relation),
                    "target": sanitize_label(&self.label_of(&e.target))
                })
            })
            .collect();
        json!({ "nodes": nodes, "edges": edges })
    }

    // resources

    fn resource_report(&self) -> String {
        self.graph_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|dir| dir.join("GRAPH_REPORT.md"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_else(|| "No GRAPH_REPORT.md found.".to_string())
    }

    fn resource_surprises(&self) -> String {
        let s = surprising_connections(&self.kg, &self.communities, 10);
        if s.is_empty() {
            return "No surprising connections.".to_string();
        }
        let mut out = String::from("Surprising connections:");
        for c in &s {
            out.push_str(&format!(
                "\n  {} <-> {} [{}]",
                sanitize_label(&c.source),
                sanitize_label(&c.target),
                sanitize_label(&c.relation)
            ));
        }
        out
    }

    fn resource_audit(&self) -> String {
        let s = &self.stats;
        format!(
            "Confidence audit:\n  EXTRACTED: {}\n  INFERRED: {}\n  AMBIGUOUS: {}",
            s.extracted, s.inferred, s.ambiguous
        )
    }

    fn resource_questions(&self) -> String {
        let qs = suggest_questions(&self.kg, &self.communities, &BTreeMap::new(), 7);
        if qs.is_empty() {
            return "No suggested questions.".to_string();
        }
        let mut out = String::from("Suggested questions:");
        for q in &qs {
            let text = q.question.as_deref().unwrap_or(&q.why);
            out.push_str(&format!("\n  - {}", sanitize_label(text)));
        }
        out
    }

    // JSON-RPC dispatch

    /// Handle one JSON-RPC request over the **single-threaded stdio** transport:
    /// pick up a rebuilt graph.json inline (if a data request), then dispatch.
    /// Returns the response value, or `None` for a notification (no `id`).
    ///
    /// The HTTP transport shares the server behind an `RwLock` and instead
    /// reloads under a write lock only when [`is_stale`](Server::is_stale), so
    /// read requests run concurrently via [`dispatch_request`](Server::dispatch_request).
    pub fn handle_request(&mut self, req: &Value) -> Option<Value> {
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        if request_needs_reload(method) {
            self.maybe_reload();
            if let Some(report) = self.needs_freshen() {
                self.apply_freshen(report);
            }
        }
        self.dispatch_request(req)
    }

    /// Dispatch one JSON-RPC request **without** reloading — read-only (`&self`),
    /// so it can run under a shared read lock. The caller handles any hot-reload
    /// first (see [`is_stale`](Server::is_stale)). Returns the response value, or
    /// `None` for a notification (no `id`) that takes no reply.
    pub fn dispatch_request(&self, req: &Value) -> Option<Value> {
        // Notifications carry no `id` and get no response.
        let id = req.get("id").cloned()?;
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        let result = match method {
            "initialize" => {
                let requested = params.get("protocolVersion").and_then(Value::as_str);
                Ok(json!({
                    "protocolVersion": negotiate_protocol(requested),
                    "capabilities": {
                        "tools": {},
                        "resources": { "subscribe": true },
                        "prompts": {},
                        "completions": {},
                        "logging": {}
                    },
                    "serverInfo": {
                        "name": "synaptic",
                        "version": env!("CARGO_PKG_VERSION"),
                        "description": "Read-only code knowledge graph: query, impact, and structural search."
                    },
                    "instructions": SERVER_INSTRUCTIONS,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools_list(self.allow_exec) })),
            "prompts/list" => Ok(json!({ "prompts": prompts::prompts_list() })),
            "prompts/get" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let pargs = params.get("arguments").cloned().unwrap_or(Value::Null);
                match prompts::prompts_get(name, &pargs) {
                    Some(v) => Ok(v),
                    None => Err((-32602, format!("Unknown prompt: {name}"))),
                }
            }
            "resources/list" => Ok(json!({ "resources": resources_list() })),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": resource_templates() })),
            // Subscriptions are acknowledged here; the HTTP transport does the
            // actual push over SSE when the graph reloads (see http::handle_sse).
            "resources/subscribe" | "resources/unsubscribe" => Ok(json!({})),
            // Accept the client's minimum log level; we advertise `logging` so a
            // host can set it, and never emit below it.
            "logging/setLevel" => Ok(json!({})),
            "completion/complete" => self.dispatch_completion(&params),
            "tools/call" => self.dispatch_tool(&params),
            "resources/read" => self.dispatch_resource(&params),
            other => Err((-32601, format!("Method not found: {other}"))),
        };

        Some(match result {
            Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
        })
    }

    /// Whether the loaded graph.json changed on disk since load (cheap, read-only).
    /// `false` when there's no path or the file vanished (serve-stale-on-error),
    /// matching `maybe_reload`'s own decision.
    pub fn is_stale(&self) -> bool {
        let Some(path) = &self.graph_path else {
            return false;
        };
        match reload_key_for(path) {
            Some(key) => self.reload_key != Some(key),
            None => false,
        }
    }

    /// Repo root for tools that shell out (diff) or read source (plan_rename):
    /// the configured source root, else the current directory.
    fn repo_root(&self) -> std::path::PathBuf {
        self.source_root
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    }

    /// Structural search via SYNQL (or a named pattern) over the loaded graph.
    /// Resolve the structural_search inputs into a query result. Precedence:
    /// `pattern`, then `query`, then `file` (the file-outline shorthand, which
    /// synthesizes `WHERE n.file =~ "<file>"` and orders the rows by line so the
    /// result reads like a file outline). The file string is regex-escaped so a
    /// path matches literally.
    fn structural_search_result(
        &self,
        query: Option<&str>,
        pattern: Option<&str>,
        file: Option<&str>,
    ) -> Result<synaptic_synql::QueryResult, String> {
        let raw = if let Some(p) = pattern {
            synaptic_synql::patterns::run_pattern(&self.kg, p)
        } else if let Some(q) = query {
            synaptic_synql::run(&self.kg, q)
        } else if let Some(f) = file {
            // File-outline shorthand (escaped + line-ordered), shared with the CLI.
            synaptic_synql::file_outline(&self.kg, f)
        } else {
            return Err("Provide a SYNQL query, a pattern name, or a file.".to_string());
        };
        raw.map_err(|e| format!("search error: {e}"))
    }

    pub fn tool_structural_search(
        &self,
        query: Option<&str>,
        pattern: Option<&str>,
        file: Option<&str>,
        limit: usize,
    ) -> String {
        let r = match self.structural_search_result(query, pattern, file) {
            Ok(r) => r,
            Err(e) => return e,
        };
        if let Some(agg) = &r.aggregates {
            let mut out = format!("{} group(s) [{}]", agg.len(), r.columns.join(", "));
            for row in agg.iter().take(limit) {
                out.push_str(&format!("\n  {}", row.join("  |  ")));
            }
            return out;
        }
        if r.rows.is_empty() {
            return "0 results.".to_string();
        }
        let mut out = format!("{} result(s) [{}]", r.rows.len(), r.columns.join(", "));
        for row in r.rows.iter().take(limit) {
            let cells: Vec<String> = row
                .iter()
                .map(|id| sanitize_label(&self.label_of(id)))
                .collect();
            out.push_str(&format!("\n  {}", cells.join("  |  ")));
        }
        out
    }

    /// Time-travel diff between two git revisions (builds each in a worktree).
    pub fn tool_time_travel_diff(&self, rev1: &str, rev2: Option<&str>, top: usize) -> String {
        let opts = synaptic_history::DiffOptions {
            top,
            ..Default::default()
        };
        let r = match synaptic_history::diff(&self.repo_root(), rev1, rev2, &opts) {
            Ok(r) => r,
            Err(e) => return format!("diff error: {e}"),
        };
        let mut o = format!("Diff {} -> {}\n{}\n", r.rev1, r.rev2, r.summary);
        o.push_str(&format!(
            "Added deps: {}; removed deps: {}; removed APIs: {}; new cycles: {}\n",
            r.added_dependencies.len(),
            r.removed_dependencies.len(),
            r.removed_apis.len(),
            r.new_cycles.len()
        ));
        o.push_str(&format!(
            "Drift: coupling {:.3} -> {:.3}, communities {} -> {}\n",
            r.drift.coupling_before,
            r.drift.coupling_after,
            r.drift.communities_before,
            r.drift.communities_after
        ));
        for h in r.hotspots.iter().take(top) {
            // Include graph-node churn: a file can be a hotspot purely from node
            // adds/removes with no line delta, which rendered as a meaningless
            // "+0/-0 lines" row. Matches the CLI `diff` output.
            o.push_str(&format!(
                "  hotspot {} (+{}/-{} lines, +{}/-{} nodes)\n",
                h.file, h.lines_added, h.lines_removed, h.nodes_added, h.nodes_removed
            ));
        }
        o
    }

    /// Plan a rename (plan-only; never edits). Returns a human-readable summary.
    pub fn tool_plan_rename(
        &self,
        name: &str,
        to: &str,
        id: Option<&str>,
        file: Option<&str>,
        limit: usize,
        verbose: bool,
    ) -> String {
        // `name` may be a node id; pin it only when --id is not given.
        let (old, opt_id) = match (id, self.kg.node(&synaptic_core::NodeId(name.to_string()))) {
            (Some(_), _) => (name.to_string(), id.map(str::to_string)),
            (None, Some(n)) => (n.label.clone(), Some(n.id.0.clone())),
            (None, None) => (name.to_string(), None),
        };
        let opts = synaptic_refactor::RenameOptions {
            id: opt_id,
            file: file.map(str::to_string),
            // Reading every indexed file is too heavy for an MCP call; the CLI does it.
            scan_text: false,
            ..Default::default()
        };
        let plan =
            match synaptic_refactor::plan_rename(&self.kg, &old, to, &self.repo_root(), &opts) {
                Ok(p) => p,
                Err(e) => return format!("rename error: {e}"),
            };
        let mut o = format!(
            "Rename {} -> {} [{:?}], {} edit(s) across {} file(s), {} to review, {} affected",
            plan.old_name,
            plan.new_name,
            plan.overall_confidence,
            plan.blast_radius.edit_count,
            plan.blast_radius.file_count,
            plan.review.len(),
            plan.blast_radius.affected_node_count
        );
        if plan.ambiguous_target {
            o.push_str(&format!(
                "\n  note: {} definitions share `{}`",
                plan.candidates.len(),
                plan.old_name
            ));
        }
        if plan.collision.exists {
            o.push_str(&format!(
                "\n  WARNING ({}): `{}` already exists",
                plan.collision.severity, plan.new_name
            ));
        }
        // Emit the actual edit sites (file:line:col, old -> new, reason,
        // confidence) so an agent can apply the rename without a second
        // round-trip to the CLI's plan.md. Capped like `affected`; verbose dumps
        // all. The site renderer is shared with the CLI so the two never drift.
        let cap = if verbose { usize::MAX } else { limit.max(1) };
        append_capped_sites(&mut o, "Edits", &plan.edits, cap);
        append_capped_sites(&mut o, "Review", &plan.review, cap);
        o.push_str("\n  (plan-only; Synaptic did not edit source)");
        o
    }

    /// Audit the loaded graph's SQL for perf + security findings (read-only).
    /// Graph-only here (no trusted source root for the N+1 source-read rule;
    /// the CLI `sql audit --root` covers that).
    fn audit_sql_report(&self, severity: Option<&str>) -> synaptic_sqlaudit::AuditReport {
        let opts = synaptic_sqlaudit::AuditOptions {
            root: None,
            min_severity: severity.and_then(synaptic_sqlaudit::Severity::parse),
        };
        synaptic_sqlaudit::audit(&self.kg, &opts)
    }

    fn advise_sql_report(
        &self,
        query: &str,
        dialect: Option<&str>,
    ) -> synaptic_sqlaudit::AuditReport {
        synaptic_sqlaudit::advise(&self.kg, query, dialect)
    }

    /// Compact text rendering of an audit report for the MCP text channel.
    /// Shows at most `cap` findings (the report is severity-sorted) before a
    /// "+N more" note. Terse by default: one line per finding (severity, rule,
    /// location, confidence, title); `verbose` adds the detail + fix per finding.
    fn render_audit_text(
        &self,
        r: &synaptic_sqlaudit::AuditReport,
        cap: usize,
        verbose: bool,
    ) -> String {
        let mut out = sanitize_label(&r.summary);
        for f in r.findings.iter().take(cap) {
            let loc = f.location.as_deref().unwrap_or("-");
            if verbose {
                out.push_str(&format!(
                    "\n[{}] {} ({}) @ {} conf {:.2}\n  {}\n  fix: {}",
                    f.severity.as_str(),
                    sanitize_label(&f.title),
                    sanitize_label(&f.rule_id),
                    sanitize_label(loc),
                    f.confidence,
                    sanitize_label(&f.detail),
                    sanitize_label(&f.remediation),
                ));
            } else {
                out.push_str(&format!(
                    "\n[{}] {} @ {} (conf {:.2}) {}",
                    f.severity.as_str(),
                    sanitize_label(&f.rule_id),
                    sanitize_label(loc),
                    f.confidence,
                    sanitize_label(&f.title),
                ));
            }
        }
        if r.findings.len() > cap {
            out.push_str(&format!(
                "\n... (+{} more finding(s); pass verbose=true or raise limit for the full list)",
                r.findings.len() - cap
            ));
        } else if !verbose && !r.findings.is_empty() {
            out.push_str("\n(pass verbose=true for each finding's detail and fix)");
        }
        out
    }

    fn dispatch_tool(&self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        let s = |k: &str| {
            args.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let u = |k: &str, d: u64| args.get(k).and_then(Value::as_u64).unwrap_or(d);
        let opt = |k: &str| args.get(k).and_then(Value::as_str);
        let b = |k: &str| args.get(k).and_then(Value::as_bool).unwrap_or(false);
        // Concise mode lowers a knob's DEFAULT only; an explicit argument always
        // wins because `u(k, d)` reads the argument before falling back to `d`.
        let cdef = |normal: u64, concise: u64| if self.concise { concise } else { normal };

        // query_graph renders both text and structuredContent from a SINGLE
        // retrieval. The index query is O(graph); rendering both shapes from one
        // result avoids paying it twice per request.
        if name == "query_graph" {
            let mode = match args.get("mode").and_then(Value::as_str) {
                Some("dfs") => TraversalMode::Dfs,
                _ => TraversalMode::Bfs,
            };
            let ctx: Vec<String> = args
                .get("context_filter")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let question = s("question");
            let budget = u("token_budget", cdef(1200, 800)) as usize;
            let since = opt("since");
            let full = b("full");
            let recency_mode = match opt("recency_mode") {
                Some("seed") => RecencyMode::Seed,
                _ => RecencyMode::Boost,
            };
            let (r, keep, recency) =
                self.query_filtered(&question, mode, budget, &ctx, since, recency_mode);
            // Terse by default: a prefix of the score-sorted nodes and NO edges, so
            // a "where is X" question returns the key symbols cheaply. full=true
            // returns the whole budget-bounded subgraph with its edges (capped to
            // ~2x the node count so a dense neighbourhood cannot dominate).
            let (view, edge_cap): (&[usize], usize) = if full {
                (&keep, keep.len().saturating_mul(2).max(1))
            } else {
                let top_k = cdef(15, 10) as usize;
                (&keep[..keep.len().min(top_k)], 0)
            };
            let mut text =
                self.render_query_text(&r, view, mode, budget, recency.as_ref(), edge_cap);
            if !full && keep.len() > view.len() {
                text.push_str(&format!(
                    "\n(terse: top {} of {} matched nodes, edges omitted; pass full=true for the whole subgraph)",
                    view.len(),
                    keep.len()
                ));
            }
            // Log the "<n> nodes found" count from the header.
            self.log_query(&question, nodes_found(&text));
            let structured = self.render_query_json(&r, view, recency.as_ref(), edge_cap);
            return Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": structured,
                "isError": false
            }));
        }

        // search_text renders text + structuredContent from a SINGLE content
        // walk (the walk is the cost), so both shapes share one search.
        if name == "search_text" {
            let (text, structured) = self.search_text_dual(
                &s("pattern"),
                b("literal"),
                args.get("case_sensitive").and_then(Value::as_bool),
                opt("repo"),
                opt("path_glob"),
                u("max_results", cdef(100, 40)) as usize,
            );
            return Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": structured,
                "isError": false
            }));
        }

        if name == "dynamic_hazards" {
            let (text, structured) = self.dynamic_hazards_dual(
                opt("repo"),
                opt("path_glob"),
                opt("kind"),
                opt("target"),
                u("max_results", cdef(30, 20)) as usize,
            );
            return Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": structured,
                "isError": false
            }));
        }

        // The only command-running tool. Gated: it is advertised in tools/list and
        // runnable ONLY when the operator started the server with --allow-exec.
        if name == "speculate" {
            if !self.allow_exec {
                return Ok(json!({
                    "content": [{ "type": "text", "text": "Speculative execution is disabled. Restart the server with --allow-exec to enable the speculate tool." }],
                    "isError": true
                }));
            }
            let files: Vec<String> = args
                .get("files")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let text = self.tool_speculate(
                &files,
                opt("base"),
                opt("test_cmd"),
                opt("check_cmd"),
                u("depth", 3) as usize,
                u("timeout", 300),
                u("max_tests", 20) as usize,
            );
            return Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false
            }));
        }

        // god_nodes: compute the page (one reverse-impact walk per hub) ONCE and
        // render both channels, instead of recomputing for the text path and again
        // for the structured mirror. Mirrors the audit_sql compute-once idiom.
        if name == "god_nodes" {
            let (rows, start) =
                self.god_nodes_page(u("top_n", cdef(10, 6)) as usize, u("offset", 0) as usize);
            let text = Self::render_god_nodes_text(&rows, start);
            let structured = Self::render_god_nodes_json(&rows);
            return Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": structured,
                "isError": false
            }));
        }

        // SQL audit/advise: compute the report once and render to both channels
        // (mirrors the query_graph compute-once idiom). Both are read-only.
        if name == "audit_sql" || name == "advise_sql" {
            let report = if name == "advise_sql" {
                self.advise_sql_report(&s("query"), opt("dialect"))
            } else {
                self.audit_sql_report(opt("severity"))
            };
            // Summary + top-N by default; verbose (or a larger limit) returns the
            // full dump. advise_sql is a single query, so it is never truncated.
            let verbose = b("verbose") || name == "advise_sql";
            let cap = if verbose {
                usize::MAX
            } else {
                (u("limit", cdef(20, 12)) as usize).max(1)
            };
            let text = self.render_audit_text(&report, cap, verbose);
            // The structured channel mirrors the text cap so the response payload
            // stays bounded; the summary still reflects the true total.
            let structured = if report.findings.len() > cap {
                let mut trimmed = report.clone();
                trimmed.findings.truncate(cap);
                serde_json::to_value(&trimmed).unwrap_or(Value::Null)
            } else {
                serde_json::to_value(&report).unwrap_or(Value::Null)
            };
            return Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": structured,
                "isError": false
            }));
        }

        // predict_impact / affected_tests: build the forecast ONCE (it runs git
        // and a blast-radius walk) and render both channels, instead of computing
        // it for the text path and again for a structured mirror.
        if name == "predict_impact" || name == "affected_tests" {
            let files: Vec<String> = args
                .get("files")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let depth = u("depth", 3) as usize;
            let forecast = self.build_forecast(&files, opt("base"), depth);
            let (text, structured) = match (name, forecast) {
                ("predict_impact", Some(f)) => {
                    let cap = if b("verbose") {
                        usize::MAX
                    } else {
                        (u("limit", cdef(20, 12)) as usize).max(1)
                    };
                    let text = Self::render_predict_text(&f, cap);
                    let structured = serde_json::to_value(&f).unwrap_or(Value::Null);
                    (text, structured)
                }
                ("predict_impact", None) => (
                    "No changed files to forecast (pass `files`, or run on a branch with a diff vs the base).".to_string(),
                    json!({ "changed_files": [] }),
                ),
                (_, Some(f)) => {
                    let text = Self::render_affected_tests_text(&f);
                    let structured =
                        json!({ "tests": f.at_risk_tests, "total": f.at_risk_tests.len() });
                    (text, structured)
                }
                (_, None) => (
                    "No changed files (pass `files`, or run on a branch with a diff vs the base)."
                        .to_string(),
                    json!({ "tests": [], "total": 0 }),
                ),
            };
            return Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": structured,
                "isError": false
            }));
        }

        let text = match name {
            "get_node" => self.tool_get_node(&s("label")),
            "get_source" => self.tool_get_source(
                &s("label"),
                opt("file"),
                opt("lines"),
                u("context_lines", cdef(40, 25)) as usize,
            ),
            "get_neighbors" => {
                let rf = args.get("relation_filter").and_then(Value::as_str);
                self.tool_get_neighbors(
                    &s("label"),
                    rf,
                    b("show_sites"),
                    u("limit", cdef(50, 20)) as usize,
                    b("verbose"),
                )
            }
            "get_community" => self.tool_get_community(
                u("community_id", 0) as u32,
                u("offset", 0) as usize,
                u("limit", cdef(100, 40)) as usize,
            ),
            "graph_stats" => self.tool_graph_stats(),
            "list_repos" => self.tool_list_repos(),
            "repo_stats" => self.tool_repo_stats(&s("repo")),
            "shortest_path" => {
                self.tool_shortest_path(&s("source"), &s("target"), u("max_hops", 8) as usize)
            }
            "affected" => {
                let rels: Vec<String> = args
                    .get("relations")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                self.tool_affected(
                    &s("label"),
                    u("depth", 3) as usize,
                    &rels,
                    u("limit", cdef(50, 20)) as usize,
                    b("verbose"),
                )
            }
            "find_callers" => self.tool_find_callers(
                &s("label"),
                u("limit", cdef(50, 20)) as usize,
                b("verbose"),
                b("show_sites"),
            ),
            "find_callees" => self.tool_find_callees(
                &s("label"),
                u("limit", cdef(50, 20)) as usize,
                b("verbose"),
                b("show_sites"),
            ),
            "find_references" => self.tool_find_references(
                &s("label"),
                u("limit", cdef(50, 20)) as usize,
                b("verbose"),
                b("show_sites"),
            ),
            "list_prs" => self.tool_list_prs(opt("base"), opt("repo")),
            "get_pr_impact" => self.tool_get_pr_impact(u("pr_number", 0), opt("repo")),
            "triage_prs" => self.tool_triage_prs(opt("base"), opt("repo")),
            "working_changes_impact" => self.tool_working_changes_impact(
                opt("base"),
                u("limit", cdef(20, 12)) as usize,
                b("verbose"),
                b("code_only"),
            ),
            // predict_impact / affected_tests handled above (compute-once dual render).
            "predict_edit" => self.tool_predict_edit(
                &s("symbol"),
                &s("kind"),
                u("depth", 3) as usize,
                u("limit", cdef(20, 12)) as usize,
                b("verbose"),
            ),
            "structural_search" => self.tool_structural_search(
                opt("query"),
                opt("pattern"),
                opt("file"),
                u("limit", cdef(25, 15)) as usize,
            ),
            "describe_node" => self.tool_describe_node(&s("label")),
            "time_travel_diff" => {
                self.tool_time_travel_diff(&s("rev1"), opt("rev2"), u("top", 20) as usize)
            }
            "plan_rename" => self.tool_plan_rename(
                &s("name"),
                &s("to"),
                opt("id"),
                opt("file"),
                u("limit", cdef(50, 20)) as usize,
                b("verbose"),
            ),
            // An unknown tool is a tool-result with isError, NOT a JSON-RPC
            // protocol error (return text content).
            other => {
                return Ok(json!({
                    "content": [{ "type": "text", "text": format!("Unknown tool: {other}") }],
                    "isError": true
                }))
            }
        };

        // Typed mirror of the text, for the tools that declare an outputSchema.
        let structured: Option<Value> = match name {
            "graph_stats" => Some(self.stats_json()),
            "affected" => {
                let rels: Vec<String> = args
                    .get("relations")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                Some(self.affected_json(
                    &s("label"),
                    u("depth", 3) as usize,
                    &rels,
                    u("limit", cdef(50, 20)) as usize,
                    b("verbose"),
                ))
            }
            "structural_search" => Some(self.structural_search_json(
                opt("query"),
                opt("pattern"),
                opt("file"),
                u("limit", cdef(25, 15)) as usize,
            )),
            "describe_node" => Some(self.describe_node_json(&s("label"))),
            "get_node" => Some(self.get_node_json(&s("label"))),
            "get_neighbors" => Some(self.get_neighbors_json(
                &s("label"),
                args.get("relation_filter").and_then(Value::as_str),
                u("limit", cdef(50, 20)) as usize,
                b("verbose"),
            )),
            "list_repos" => Some(self.list_repos_json()),
            _ => None,
        };

        let mut result = json!({ "content": [{ "type": "text", "text": text }], "isError": false });
        // Skip a null mirror: a tool whose `*_json` could not resolve its node
        // (e.g. get_neighbors on an ambiguous label) returns Null rather than
        // attaching an empty `structuredContent: null` to the result.
        if let Some(sc) = structured.filter(|v| !v.is_null()) {
            result["structuredContent"] = sc;
        }
        Ok(result)
    }

    fn dispatch_resource(&self, params: &Value) -> Result<Value, (i64, String)> {
        let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
        // Templated resources (resources/templates/list): any node or community
        // is addressable by URI. Checked before the static table; the static
        // URIs (synaptic://god-nodes etc.) do not share these prefixes.
        if let Some(label) = uri.strip_prefix("synaptic://node/") {
            let text = self.tool_get_node(label);
            return Ok(
                json!({ "contents": [{ "uri": uri, "mimeType": "text/plain", "text": text }] }),
            );
        }
        if let Some(id) = uri.strip_prefix("synaptic://community/") {
            let cid: u32 = id.parse().unwrap_or(u32::MAX);
            let text = self.tool_get_community(cid, 0, 1000);
            return Ok(
                json!({ "contents": [{ "uri": uri, "mimeType": "text/plain", "text": text }] }),
            );
        }
        let (mime, text) = match uri {
            "synaptic://report" => ("text/markdown", self.resource_report()),
            "synaptic://stats" => ("text/plain", self.tool_graph_stats()),
            "synaptic://god-nodes" => ("text/plain", self.tool_god_nodes(10, 0)),
            "synaptic://surprises" => ("text/plain", self.resource_surprises()),
            "synaptic://audit" => ("text/plain", self.resource_audit()),
            "synaptic://questions" => ("text/plain", self.resource_questions()),
            other => return Err((-32602, format!("Unknown resource: {other}"))),
        };
        Ok(json!({ "contents": [{ "uri": uri, "mimeType": mime, "text": text }] }))
    }

    /// `completion/complete` — argument autocomplete for the common tool/prompt
    /// arguments: node labels (label/source/target), repo tags, and community
    /// ids. Prefix match, sorted, capped at the protocol's 100 values.
    fn dispatch_completion(&self, params: &Value) -> Result<Value, (i64, String)> {
        let arg = params.get("argument");
        let arg_name = arg
            .and_then(|a| a.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let prefix = arg
            .and_then(|a| a.get("value"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let plow = prefix.to_lowercase();
        let mut values: Vec<String> = match arg_name {
            "label" | "source" | "target" => self
                .kg
                .nodes()
                // Match the bare name too: method nodes are labeled ".name()", so
                // a prefix like "tool_get" must see past the leading punctuation.
                .filter(|n| {
                    let l = n.label.to_lowercase();
                    l.starts_with(&plow)
                        || l.trim_start_matches(|c: char| !c.is_alphanumeric())
                            .starts_with(&plow)
                })
                .map(|n| sanitize_label(&n.label))
                .collect(),
            "repo" => self
                .kg
                .nodes()
                .filter_map(|n| n.repo.clone())
                .filter(|r| r.to_lowercase().starts_with(&plow))
                .map(|r| sanitize_label(&r))
                .collect(),
            "community_id" => self
                .communities
                .keys()
                .map(|c| c.to_string())
                .filter(|c| c.starts_with(prefix))
                .collect(),
            _ => Vec::new(),
        };
        values.sort();
        values.dedup();
        let total = values.len();
        values.truncate(100);
        Ok(json!({
            "completion": { "values": values, "total": total, "hasMore": total > 100 }
        }))
    }

    /// Serve over stdio: newline-delimited JSON-RPC on stdin/stdout.
    pub fn serve_stdio(&mut self) -> std::io::Result<()> {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let Ok(req) = serde_json::from_str::<Value>(&line) else {
                continue; // ignore unparseable lines (client quirk tolerance)
            };
            if let Some(resp) = self.handle_request(&req) {
                writeln!(stdout, "{resp}")?;
                stdout.flush()?;
            }
        }
        Ok(())
    }
}

fn communities_of(kg: &KnowledgeGraph) -> BTreeMap<u32, Vec<NodeId>> {
    let mut communities: BTreeMap<u32, Vec<NodeId>> = BTreeMap::new();
    for n in kg.nodes() {
        // A community lists the real code symbols that belong to a subsystem. Skip
        // nodes that carry a community label from clustering but are not subsystem
        // members: external stubs (import targets / third-party packages with no
        // source file) and non-code-symbol nodes (rationale TODO/NOTE comments,
        // markdown headings, config keys). Listing those is noise.
        if n.is_external_stub() || !n.is_code_symbol() {
            continue;
        }
        if let Some(c) = n.community {
            communities.entry(c).or_default().push(n.id.clone());
        }
    }
    for v in communities.values_mut() {
        v.sort();
    }
    communities
}

/// Data requests that should pick up a rebuilt graph.json before answering.
fn request_needs_reload(method: &str) -> bool {
    matches!(method, "tools/call" | "resources/read")
}

/// Fetch each PR's changed-file list concurrently, bounded to `MAX_CONCURRENT`
/// in-flight `gh pr diff` subprocesses. Output order matches `prs`, so callers
/// can `zip` it back on. `CommandRunner: Send + Sync`, so the scoped borrow is
/// sound; a panicking fetch propagates (same as the previous sequential call).
fn fetch_pr_files_concurrent(
    runner: &dyn CommandRunner,
    prs: &[PrInfo],
    repo: Option<&str>,
) -> Vec<Vec<String>> {
    const MAX_CONCURRENT: usize = 8;
    let mut out: Vec<Vec<String>> = Vec::with_capacity(prs.len());
    for chunk in prs.chunks(MAX_CONCURRENT) {
        std::thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|p| scope.spawn(move || fetch_pr_files(runner, p.number, repo)))
                .collect();
            for h in handles {
                out.push(h.join().expect("fetch_pr_files thread panicked"));
            }
        });
    }
    out
}

/// Round a relevance score to 2 decimals for compact tool output.
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn file_type_str(ft: &synaptic_core::FileType) -> &'static str {
    use synaptic_core::FileType::*;
    match ft {
        Code => "code",
        Document => "document",
        Paper => "paper",
        Image => "image",
        Rationale => "rationale",
        Concept => "concept",
    }
}

/// Parse a `get_source` `lines` value into a 1-based inclusive `(start, end)`.
/// `"108-140"` -> `(108, 140)`; a single `"108"` -> `(108, 108 + window - 1)` so
/// a bare line number reads a `context_lines` window. `None` for malformed input
/// or an end before the start.
fn parse_line_range(spec: &str, window: usize) -> Option<(usize, usize)> {
    let spec = spec.trim();
    if let Some((a, b)) = spec.split_once('-') {
        let start = a.trim().parse::<usize>().ok()?;
        let end = b.trim().parse::<usize>().ok()?;
        if start == 0 || end < start {
            return None;
        }
        Some((start, end))
    } else {
        let start = spec.parse::<usize>().ok()?;
        if start == 0 {
            return None;
        }
        Some((start, start + window.saturating_sub(1)))
    }
}

/// Serialize a resolved [`NodeView`](synaptic_synql::NodeView) for structured
/// tool output. Free-text fields (label/id/file, and the signature, which is
/// source-derived) are sanitized; `kind`/`visibility` come from fixed enums.
fn node_view_to_json(v: &synaptic_synql::NodeView) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("id".into(), json!(sanitize_label(&v.id)));
    obj.insert("label".into(), json!(sanitize_label(&v.label)));
    obj.insert("file".into(), json!(sanitize_label(&v.file)));
    if let Some(k) = &v.kind {
        obj.insert("kind".into(), json!(k));
    }
    if let Some(vis) = &v.visibility {
        obj.insert("visibility".into(), json!(vis));
    }
    if let Some(line) = &v.line {
        obj.insert("line".into(), json!(sanitize_label(line)));
    }
    if let Some(loc) = v.loc {
        obj.insert("loc".into(), json!(loc));
    }
    if let Some(sig) = &v.signature {
        obj.insert("signature".into(), signature_json(sig));
    }
    Value::Object(obj)
}

/// JSON for a function signature, sanitized with `sanitize_label` (control-strip +
/// length cap) rather than `sanitize_metadata_value`. The latter HTML-escapes
/// `<`/`>`, which mangles generics like `Record<string, unknown>` in the JSON
/// channels that feed tool/function-description generation. Shape mirrors the
/// serde form (`type_ref`/`return_type` omitted when absent).
fn signature_json(sig: &synaptic_core::Signature) -> Value {
    let params: Vec<Value> = sig
        .params
        .iter()
        .map(|p| {
            let mut po = serde_json::Map::new();
            po.insert("name".into(), json!(sanitize_label(&p.name)));
            if let Some(t) = &p.type_ref {
                po.insert("type_ref".into(), json!(sanitize_label(t)));
            }
            Value::Object(po)
        })
        .collect();
    let mut o = serde_json::Map::new();
    o.insert("params".into(), Value::Array(params));
    if let Some(rt) = &sig.return_type {
        o.insert("return_type".into(), json!(sanitize_label(rt)));
    }
    o.insert("raw".into(), json!(sanitize_label(&sig.raw)));
    Value::Object(o)
}

/// Parse the `<n> nodes found` count from a `query_graph` result header (the
/// count of nodes found, independent of any later truncation).
fn nodes_found(text: &str) -> usize {
    text.lines()
        .next()
        .and_then(|first| {
            let idx = first.find(" nodes found")?;
            first[..idx].rsplit(' ').next()?.parse().ok()
        })
        .unwrap_or(0)
}

/// Process-wide cl100k_base tokenizer, built once on first use. `None` if it
/// could not be loaded (then [`truncate_to_tokens`] falls back to a heuristic).
fn bpe() -> Option<&'static tiktoken_rs::CoreBPE> {
    use std::sync::OnceLock;
    static BPE: OnceLock<Option<tiktoken_rs::CoreBPE>> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().ok()).as_ref()
}

/// Truncate rendered text to about `token_budget` tokens. Cheap gate first: at
/// roughly 4 bytes per token, text within `token_budget * 4` bytes is already at
/// or under budget, so it returns unchanged without tokenizing. query_graph caps
/// its node count at `budget / 40`, so its output stays well under this gate and
/// the hot path never tokenizes. Only genuinely oversized text pays the real
/// cl100k tokenizer, and only then for an exact cut (falling back to a byte cut
/// if the tokenizer is unavailable).
fn truncate_to_tokens(text: String, token_budget: usize) -> String {
    let cap = token_budget.saturating_mul(4);
    if text.len() <= cap {
        return text;
    }
    let Some(bpe) = bpe() else {
        let mut end = cap;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        return format!(
            "{}\n... (truncated to ~{token_budget} tokens)",
            &text[..end]
        );
    };
    let toks = bpe.encode_with_special_tokens(&text);
    if toks.len() <= token_budget {
        return text;
    }
    let kept = bpe.decode(&toks[..token_budget]).unwrap_or_default();
    format!("{kept}\n... (truncated to ~{token_budget} tokens)")
}

/// Server-level orientation returned in the MCP `initialize` result. It frames
/// the whole toolset (these tools all query THIS repo's Synaptic), gives the
/// recommended flow, and defines the jargon, so an agent picks the right tool.
const SERVER_INSTRUCTIONS: &str = "\
This server exposes a Synaptic knowledge graph of THIS repo's code: symbols (functions, \
classes, files) as nodes and relationships (calls, imports, inheritance) as edges, \
clustered into communities. All tools are read-only. Query the graph before grepping or \
reading files broadly.\n\
\n\
Flow: graph_stats or god_nodes to orient; query_graph for a question (terse ranked nodes \
by default, full=true for the subgraph + edges); get_source to read a symbol's code (or a \
`file` + `lines` range, e.g. around a search_text hit); get_neighbors / find_callers / \
find_callees / find_references / shortest_path to navigate (find_callers = calls, \
find_references = all uses incl. imports/inheritance; show_sites=true prints the call-site line); \
get_node / describe_node for detail.\n\
\n\
Change impact -- pick by input: affected = one SYMBOL now; working_changes_impact = your \
git diff now; predict_impact = forecast a set of changed FILES (blast radius + public-API \
breaks + at-risk tests + checklist); affected_tests = same input, tests only; predict_edit \
= ONE symbol under a named edit (delete/signature/visibility). On a class/type these fold \
in its members, so a class is never a false 'safe leaf'. A '0 dependents' result is NOT \
proof of 'safe to change': a symbol reached via reflection or dynamic dispatch has no \
static dependents, so affected attaches a dynamic_caveat and dynamic_hazards lists the \
sites (event buses and string-literal reflection are linked into the graph; computed names \
are the residual risk). speculate runs the at-risk tests for real but is gated: start the \
server with --allow-exec to expose it (it executes commands).\n\
\n\
Also: structural_search (SYNQL query or named pattern; matches kind/loc/fan-in-out, not \
text); search_text (regex/literal content search, each hit attributed to its enclosing \
symbol); time_travel_diff; plan_rename (plan-only rename); audit_sql / advise_sql (review \
SQL). Multi-repo: call list_repos, then pass repo to scope a tool. The PR tools (list_prs / \
get_pr_impact / triage_prs) need the `gh` CLI and skip gracefully without it.\n\
\n\
Names resolve leniently (id, label, bare name); when a name is shared by several files pin \
it with a 'name@file-substring' qualifier (e.g. announce@core/foo.ts). An ambiguous name \
returns its candidates (id, file, degree).\n\
\n\
Coverage: the graph is static. Electron IPC and WebSocket/socket.io channels ARE modelled; \
inline tests beside the code may not be linked. Treat a surprising 0-caller as 'no STATIC \
caller' (see dynamic_hazards), not dead code.\n\
\n\
Terms: a 'community' is a densely-connected cluster (roughly a module); edge confidence on \
a relationship is EXTRACTED, INFERRED, or AMBIGUOUS.";

/// The MCP `tools/list` payload. Descriptions and per-parameter docs make the
/// implicit domain knowledge explicit so an agent uses each tool correctly
/// (graph jargon, the lenient label resolution, the relation vocabulary).
fn tools_list(allow_exec: bool) -> Value {
    let mut tools = json!([
        {
            "name": "query_graph",
            "description": "Primary entry point: find the symbols relevant to a natural-language question about this codebase, instead of grepping or reading files. Good for 'where is X handled', 'how does auth work', 'what is related to Y'. Returns a terse, ranked list of the top matching nodes by default; pass full=true for the whole subgraph with its edges. Optional `since`/`recency_mode` bias ranking toward branch-changed code.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "Natural-language question, e.g. 'how does login work' or 'what handles payments'." },
                    "mode": { "type": "string", "enum": ["bfs", "dfs"], "description": "Traversal from the seed nodes: 'bfs' (default) expands a broad neighbourhood; 'dfs' follows deep call chains. Use dfs to trace one flow end to end." },
                    "full": { "type": "boolean", "description": "Return the whole subgraph (all budget-bounded nodes plus their edges) instead of the terse top-N node list (default false). Set true when you need the relationships, not just which symbols match." },
                    "token_budget": { "type": "integer", "description": "Approximate token budget for the full subgraph (default 1200). Controls how many nodes are in scope (about budget/40, capped 10-400); raise it for broader context. The terse default shows only the top ~15 of those." },
                    "context_filter": { "type": "array", "items": { "type": "string" }, "description": "Optional source-file path substrings; keeps only nodes whose file matches one (e.g. ['src/auth','login']). Use to scope a question to a subsystem." },
                    "since": { "type": "string", "description": "Optional. Boost nodes whose file changed since this baseline: a git ref ('main', 'HEAD~10'), a date ('2 weeks ago'), or 'auto' (detect the default branch). Includes uncommitted edits; silently ignored outside a git repo." },
                    "recency_mode": { "type": "string", "enum": ["boost", "seed"], "description": "Only with `since`. 'boost' (default) re-ranks query matches by recency. 'seed' also injects changed-file nodes as seeds, so the changed surface appears even when the question matches little (use to answer 'what did this branch change')." }
                },
                "required": ["question"]
            },
            "outputSchema": { "type": "object", "properties": {
                "nodes": { "type": "array", "description": "Ordered most- to least-relevant.", "items": { "type": "object", "properties": {
                    "label": {"type":"string"}, "file_type": {"type":"string"}, "source_file": {"type":"string"},
                    "score": {"type":"number", "description":"Relevance score (higher = more relevant); nodes are sorted by it. Use it to focus on the top results and ignore the low-scored tail."},
                    "changed": {"type":"boolean", "description":"True when `since` was given and this node's file changed on the current branch (its score was boosted)."} } } },
                "edges": { "type": "array", "description": "Ordered by endpoint relevance.", "items": { "type": "object", "properties": {
                    "source": {"type":"string"}, "relation": {"type":"string"}, "target": {"type":"string"} } } }
            }, "required": ["nodes", "edges"] }
        },
        { "name": "get_node", "description": "Show one node's metadata: type, source file, community, degree, plus kind (class/function/method/etc.), visibility, and LOC when available. Use after query_graph to inspect a specific symbol.",
          "inputSchema": { "type": "object", "properties": { "label": { "type": "string", "description": "Node label, id, or bare name (e.g. 'login_user', 'AuthService'); qualify a shared name as 'name@file'." } }, "required": ["label"] },
          "outputSchema": { "type": "object", "properties": {
              "found": {"type":"boolean"},
              "id": {"type":"string"}, "label": {"type":"string"}, "source_file": {"type":"string"},
              "file_type": {"type":"string"}, "degree": {"type":"integer"}, "community": {"type":"integer"},
              "kind": {"type":"string"}, "visibility": {"type":"string"}, "loc": {"type":"integer"},
              "dynamic_sites": { "type": "object", "description": "Reflection/dynamic-dispatch sites in this node's body (present only when non-empty).", "properties": { "count": {"type":"integer"}, "kinds": {"type":"array","items":{"type":"string"}} } },
              "dynamically_referenced": {"type":"boolean", "description": "True when a reflection site names this symbol, so it may be reachable only at runtime."},
              "dynamic_caveat": { "type": "object", "description": "Present when this symbol has 0 static dependents but a dynamic site could reach it -- 0 is not proof it is unused.", "properties": { "opaque_sites_in_scope": {"type":"integer"}, "kinds": {"type":"array","items":{"type":"string"}}, "dynamically_referenced": {"type":"boolean"}, "message": {"type":"string"} } },
              "ambiguous": {"type":"boolean"}, "query": {"type":"string"},
              "candidates": { "type": "array", "items": { "type": "object", "properties": {
                  "id": {"type":"string"}, "file": {"type":"string"}, "degree": {"type":"integer"} } }, "description":"Disambiguation candidates when the name is ambiguous (found=false)." }
          }, "required": ["found"] } },
        { "name": "get_source", "description": "Return the actual source code for a symbol (the lines at its location), so you do not have to open the file. Use after query_graph or get_node to read a function or class body directly. Alternatively pass `file` (with an optional `lines` range) to read an ARBITRARY region that is not a single symbol -- a config block, or the lines around a search_text hit. In a federated graph a leading 'tag/' on `file` selects the member repo.",
          "inputSchema": { "type": "object", "properties": {
              "label": { "type": "string", "description": "Node label, id, or bare name; qualify a shared name as 'name@file'. Omit when using `file`." },
              "file": { "type": "string", "description": "Read this file directly instead of resolving a symbol. Repo-relative, or 'tag/path' in a federated graph (the tag from list_repos). Pair with `lines` for a range." },
              "lines": { "type": "string", "description": "With `file`: the range to read, 'start-end' (e.g. '108-140') or a single 'start' (reads context_lines from there). Ignored without `file`." },
              "context_lines": { "type": "integer", "description": "Lines to return from the symbol/line start (default 40, max 400)." }
          }, "required": [] } },
        { "name": "get_neighbors", "description": "List a node's directly connected nodes and the relation on each edge. Answers 'what does X call/use' and 'what calls X'.",
          "inputSchema": { "type": "object", "properties": { "label": { "type": "string", "description": "Node label, id, or bare name; qualify a shared name as 'name@file'." }, "relation_filter": { "type": "string", "description": "Keep only this edge relation (substring); e.g. calls, imports, inherits, implements, references, contains, depends_on. A non-match returns the node's actual relations." }, "show_sites": { "type": "boolean", "description": "Also show each edge's call/reference source line ('at file:line: <code>'). Default false." }, "limit": { "type": "integer", "description": "Max neighbors listed before a '+N more' summary (default 50). Ignored when verbose=true." }, "verbose": { "type": "boolean", "description": "List every neighbor instead of the capped top-N (default false). Use after a relation_filter on a hub." } }, "required": ["label"] },
          "outputSchema": { "type": "object", "properties": {
              "seed": {"type":"string"},
              "neighbors": { "type": "array", "items": { "type": "object", "properties": {
                  "label": {"type":"string"}, "relation": {"type":"string"}, "direction": {"type":"string", "description": "'out' (seed -> neighbor) or 'in' (neighbor -> seed)."} } } },
              "by_relation": { "type": "object", "description": "Count of all edges on the seed by relation, before any filter." },
              "total": {"type":"integer", "description": "Total neighbors matching the filter; may exceed the capped `neighbors` list."},
              "truncated": {"type":"boolean"}
          }, "required": ["seed","neighbors"] } },
        { "name": "get_community", "description": "List the members of a community: a cluster of densely-connected nodes, roughly a module or subsystem. Use to see what belongs together. Paginates: a large community returns one page at a time.",
          "inputSchema": { "type": "object", "properties": {
              "community_id": { "type": "integer", "description": "Community id, as reported by graph_stats, god_nodes, or a node's 'Community' field." },
              "offset": { "type": "integer", "description": "Members to skip before the page (default 0). Raise it to page through a large community." },
              "limit": { "type": "integer", "description": "Max members to return in this page (default 100)." }
          }, "required": ["community_id"] } },
        { "name": "god_nodes", "description": "The most-connected nodes (high-degree hubs); use to orient in an unfamiliar codebase. degree = structural centrality, not a dependence count (use `affected` for blast radius). Also shows each hub's transitive test count; 0 flags an untested hub.",
          "inputSchema": { "type": "object", "properties": {
              "top_n": { "type": "integer", "description": "How many hubs to return (default 10)." },
              "offset": { "type": "integer", "description": "Hubs to skip before the page (default 0), for paging past the top ranks." }
          } },
          "outputSchema": { "type": "object", "properties": {
              "god_nodes": { "type": "array", "items": { "type": "object", "properties": {
                  "label": {"type":"string"}, "degree": {"type":"integer", "description": "Total connections (all edge kinds, incl. class members): structural centrality/size, not an incoming-dependence count."}, "id": {"type":"string"},
                  "test_count": {"type":"integer", "description": "How many tests transitively exercise this hub; 0 flags an untested high-blast-radius symbol."},
                  "dynamically_referenced": {"type":"boolean", "description": "Present and true when a reflection site names this hub: it is reachable via dynamic dispatch, so its static dependence count understates real coupling."} } } }
          }, "required": ["god_nodes"] } },
        { "name": "graph_stats", "description": "Graph size and health: node/edge/community counts and the EXTRACTED/INFERRED/AMBIGUOUS edge-confidence breakdown. On a federated (multi-repo) graph it also reports how many edges span repositories (`cross_repo`) and how many of those are cross-language coupling (`cross_language`: HTTP/RPC/FFI/WebSocket boundaries). Good first call to confirm a graph is loaded and how large it is.",
          "inputSchema": { "type": "object", "properties": {} },
          "outputSchema": { "type": "object", "properties": {
              "nodes": {"type":"integer"}, "edges": {"type":"integer"}, "communities": {"type":"integer"},
              "extracted": {"type":"integer"}, "inferred": {"type":"integer"}, "ambiguous": {"type":"integer"},
              "cross_repo": {"type":"integer"}, "cross_language": {"type":"integer"},
              "dynamic_sites": {"type":"integer", "description": "Total reflection/dynamic-dispatch sites recorded across the graph."},
              "dynamic_sites_opaque": {"type":"integer", "description": "Of those, sites whose dispatched name is computed (not a string literal) and so could not be evidence-linked."},
              "dynamic_refs_linked": {"type":"integer", "description": "Evidence-linked dynamic_ref edges added from literal-key sites to their unique target."}
          }, "required": ["nodes","edges","communities"] } },
        { "name": "dynamic_hazards", "description": "List the reflection / dynamic-dispatch sites (by-name lookups, dispatch tables, eval, dynamic import, .NET/Python/JVM reflection) in the graph. Use it to judge a '0 dependents' answer: a symbol reached only by dynamic dispatch has no static dependents. A literal-key site is evidence-linked to its target; an opaque (computed-name) site cannot be and is cataloged here as residual risk. Filter by `repo`/`path_glob`/`kind`, or pass `target` for the sites that could reach one symbol.",
          "inputSchema": { "type": "object", "properties": {
              "repo": { "type": "string", "description": "Restrict to one federated member tag (as listed by list_repos)." },
              "path_glob": { "type": "string", "description": "Only sites in files matching this glob, e.g. '**/*.ts' or 'src/**'." },
              "kind": { "type": "string", "enum": ["reflection","dynamic_import","eval"], "description": "Restrict to one site kind." },
              "target": { "type": "string", "description": "Show only sites that could reach this symbol: sites whose literal key names it, plus opaque sites in a file that defines it." },
              "max_results": { "type": "integer", "description": "Max sites to return (default 30, capped at 1000). It is a scan-and-narrow tool: filter by repo/path_glob/kind/target rather than raising this." }
          } },
          "outputSchema": { "type": "object", "properties": {
              "total": {"type":"integer"}, "truncated": {"type":"boolean"},
              "sites": { "type": "array", "items": { "type": "object", "properties": {
                  "repo": {"type":["string","null"]}, "file": {"type":"string"}, "line": {"type":"integer"},
                  "kind": {"type":"string"}, "key": {"type":["string","null"], "description": "The dispatched name when it is a string literal; null when computed/opaque."},
                  "host": {"type":"string", "description": "The enclosing symbol that performs the dynamic dispatch."} } } }
          }, "required": ["total","sites"] } },
        { "name": "list_repos", "description": "For a federated (multi-repo) graph, list member repos (tags) with node/edge counts; empty for a single repo. Use before scoping a query to one repo. Each repo also carries a `source_hash` (a content fingerprint of that member's sources from the last extraction) when available, so per-repo drift is visible: a member whose code changed since this graph was built keeps its old hash until re-extraction.",
          "inputSchema": { "type": "object", "properties": {} },
          "outputSchema": { "type": "object", "properties": {
              "repos": { "type": "array", "items": { "type": "object", "properties": {
                  "repo": {"type":"string"}, "nodes": {"type":"integer"}, "edges": {"type":"integer"},
                  "source_hash": {"type":"string", "description": "Per-repo source fingerprint from workspace-state.json; present only for a federated graph with that state file."} } } }
          }, "required": ["repos"] } },
        { "name": "repo_stats", "description": "Node/edge counts for one federated member repo.",
          "inputSchema": { "type": "object", "properties": { "repo": { "type": "string", "description": "Repo tag, as listed by list_repos." } }, "required": ["repo"] } },
        { "name": "shortest_path", "description": "Shortest path between two nodes, showing the chain of relations. Answers 'how does A reach B' or 'is X connected to Y'.",
          "inputSchema": { "type": "object", "properties": { "source": { "type": "string", "description": "Start node: label, id, or bare name. Qualify a shared name as 'name@file'." }, "target": { "type": "string", "description": "End node: label, id, or bare name. Qualify a shared name as 'name@file'." }, "max_hops": { "type": "integer", "description": "Optional cap on path length in hops (default 8)." } }, "required": ["source", "target"] } },
        { "name": "affected", "description": "Reverse-impact of one SYMBOL: the nodes that transitively depend on it -- what could break if you change it. Walks the dependency edges backward including cross-language coupling, so the blast radius spans languages. A class/type folds in its members (labelled aggregated), so a class is never a false 'safe leaf'. A 0-dependent result may carry a `dynamic_caveat` (reflection/IPC/event-bus); see the server instructions / `dynamic_hazards`.",
          "inputSchema": { "type": "object", "properties": {
              "label": { "type": "string", "description": "Node label, id, or bare name; qualify a shared name as 'name@file'." },
              "depth": { "type": "integer", "description": "Max hops to walk backward (default 3, max 16)." },
              "relations": { "type": "array", "items": { "type": "string" }, "description": "Optional edge relations to follow; defaults to the structural-impact set (calls/imports/inheritance/uses/depends_on) plus the cross-language relations invokes, binds_native, calls_service, handled_by, and the evidence-linked dynamic_ref." },
              "limit": { "type": "integer", "description": "Max dependents listed before a '+N more' summary (default 50; a per-depth breakdown and true total are always shown). Ignored when verbose=true." },
              "verbose": { "type": "boolean", "description": "List all dependents instead of the top-N summary (default false); useful after narrowing depth/relations on a hub." }
          }, "required": ["label"] },
          "outputSchema": { "type": "object", "properties": {
              "seed": {"type":"string"},
              "affected": { "type": "array", "items": { "type": "object", "properties": {
                  "label": {"type":"string"}, "depth": {"type":"integer"}, "via_relation": {"type":"string"} } } },
              "total": {"type":"integer"}, "truncated": {"type":"boolean"},
              "by_depth": { "type": "object", "additionalProperties": {"type":"integer"} },
              "resolved": {"type":"boolean", "description":"false when the name did not resolve to a single node; see ambiguous/candidates."},
              "ambiguous": {"type":"boolean"},
              "candidates": { "type": "array", "items": { "type": "object", "properties": {
                  "id": {"type":"string"}, "file": {"type":"string"}, "degree": {"type":"integer"} } }, "description":"Disambiguation candidates when ambiguous." },
              "aggregated_over_members": {"type":"integer", "description":"When the seed is a class/type, the number of members folded into the reverse-impact (impact attaches to a class's methods, not the bare symbol)."},
              "dynamic_caveat": { "type": "object", "description": "Present only when total=0 AND the symbol may be reached by dynamic dispatch -- so 0 dependents is not proof it is safe to change. Inspect the sites with dynamic_hazards.", "properties": { "opaque_sites_in_scope": {"type":"integer"}, "kinds": {"type":"array","items":{"type":"string"}}, "dynamically_referenced": {"type":"boolean"}, "message": {"type":"string"} } }
          }, "required": ["seed","affected"] } },
        { "name": "find_callers", "description": "Incoming callers of one SYMBOL ('who calls X'; incoming edges only). Capped with a '+N more' summary; per-relation counts in the header. For a class/type, its methods' callers fold in (labelled). For a type's import/inheritance/type usages (not just calls), use find_references. STATIC callers only: a dynamically-dispatched handler can show 0 yet still run (see server instructions).",
          "inputSchema": { "type": "object", "properties": {
              "label": { "type": "string", "description": "Node label, id, or bare name; qualify a shared name as 'name@file'." },
              "limit": { "type": "integer", "description": "Max callers listed before a '+N more' summary (default 50). Ignored when verbose=true." },
              "verbose": { "type": "boolean", "description": "List all callers instead of the top-N summary (default false)." },
              "show_sites": { "type": "boolean", "description": "Under each caller, show the actual source line where the call happens ('at file:line: <code>'), read from the jail. Turns 'who calls X' into 'who calls X, and the exact line' without a second get_source. Default false." }
          }, "required": ["label"] } },
        { "name": "find_callees", "description": "Outgoing calls of one SYMBOL ('what does X call'; outgoing edges only). Capped with a '+N more' summary; per-relation counts in the header. For a class/type, its methods' callees fold in (labelled).",
          "inputSchema": { "type": "object", "properties": {
              "label": { "type": "string", "description": "Node label, id, or bare name; qualify a shared name as 'name@file'." },
              "limit": { "type": "integer", "description": "Max callees listed before a '+N more' summary (default 50). Ignored when verbose=true." },
              "verbose": { "type": "boolean", "description": "List all callees instead of the top-N summary (default false)." },
              "show_sites": { "type": "boolean", "description": "Under each callee, show the actual source line where this symbol calls it ('at file:line: <code>'), read from the jail -- so 'what does X call' also shows HOW it calls it. Default false." }
          }, "required": ["label"] } },
        { "name": "find_references", "description": "Find-all-references: EVERY place a symbol is used -- calls plus imports, inheritance/implements, type uses, cross-language coupling, and reflection refs -- with a per-relation breakdown. Use for a type/interface/enum/constant, where find_callers (calls only) misses the structural usages. Incoming edges to the symbol itself (no class-member folding). Capped with a '+N more' summary.",
          "inputSchema": { "type": "object", "properties": {
              "label": { "type": "string", "description": "Node label, id, or bare name; qualify a shared name as 'name@file'." },
              "limit": { "type": "integer", "description": "Max references listed before a '+N more' summary (default 50). Ignored when verbose=true." },
              "verbose": { "type": "boolean", "description": "List all references instead of the top-N summary (default false)." },
              "show_sites": { "type": "boolean", "description": "Under each reference, show the actual source line where the use happens ('at file:line: <code>'). Default false." }
          }, "required": ["label"] } },
        { "name": "list_prs", "description": "Open pull requests targeting the base branch with their CI/review state. Requires the `gh` CLI authenticated for the repo.",
          "inputSchema": { "type": "object", "properties": { "base": { "type": "string", "description": "Base branch to filter to (default: the repo's default branch)." }, "repo": { "type": "string", "description": "Target repo 'owner/name' (default: the current repo)." } } } },
        { "name": "get_pr_impact", "description": "One PR's detail plus its graph blast radius: which graph nodes and communities its changed files touch.",
          "inputSchema": { "type": "object", "properties": { "pr_number": { "type": "integer", "description": "PR number." }, "repo": { "type": "string", "description": "Target repo 'owner/name' (default: the current repo)." } }, "required": ["pr_number"] } },
        { "name": "triage_prs", "description": "Open PRs ranked by actionability (status plus graph blast radius) so the model can prioritize review and merge order.",
          "inputSchema": { "type": "object", "properties": { "base": { "type": "string", "description": "Base branch (default: the repo's default branch)." }, "repo": { "type": "string", "description": "Target repo 'owner/name' (default: the current repo)." } } } },
        { "name": "working_changes_impact", "description": "Graph blast radius of your branch's changes against a base branch (committed plus uncommitted, the same set a PR would have): which graph nodes and communities they touch, before opening a PR. Uses git, no gh needed. Default output lists the changed files plus counts; pass verbose=true to also list the top touched nodes (ranked by connectivity) and the touched communities with labels.",
          "inputSchema": { "type": "object", "properties": {
              "base": { "type": "string", "description": "Base branch to diff against (default: the repo's default branch)." },
              "verbose": { "type": "boolean", "description": "Also list the top touched nodes and labeled communities, not just files (default false)." },
              "limit": { "type": "integer", "description": "Max touched nodes listed when verbose (default 20)." },
              "code_only": { "type": "boolean", "description": "Count and list only code nodes, excluding non-code files (package.json, lockfiles, .md docs) to sharpen the blast radius (default false)." }
          } } },
        { "name": "structural_search", "description": "Structural (not text) search over the graph via SYNQL, or a named pattern, or a file outline. Matches kind/visibility/loc/fan-in-out. `.name` is the bare symbol (no parentheses); use `=~` for a regex/substring match. Example: 'MATCH (c:class) WHERE c.loc > 500 RETURN c'. Patterns: singleton, factory, observer, service-locator, god-class. Pass `file` to list every symbol defined in a file (an outline, ordered by line) -- no query needed.",
          "inputSchema": { "type": "object", "properties": {
              "query": { "type": "string", "description": "A SYNQL query. Omit when using `pattern` or `file`." },
              "pattern": { "type": "string", "description": "A built-in pattern name instead of a query." },
              "file": { "type": "string", "description": "List every symbol defined in this file (path substring), ordered by line -- a file outline. Used only when `query` and `pattern` are omitted." },
              "limit": { "type": "integer", "description": "Max rows to return (default 25)." }
          } },
          "outputSchema": { "type": "object", "properties": {
              "columns": { "type": "array", "items": { "type": "string" }, "description": "RETURN headers." },
              "results": { "type": "array", "description": "One array of node cells per matched row.",
                "items": { "type": "array", "items": { "type": "object", "properties": {
                  "id": { "type": "string" },
                  "label": { "type": "string" },
                  "kind": { "type": "string" },
                  "visibility": { "type": "string" },
                  "file": { "type": "string" },
                  "line": { "type": "string" },
                  "loc": { "type": "integer" },
                  "signature": { "type": "object", "description": "Captured signature: params (name + optional type_ref), optional return_type, and the raw header.", "properties": {
                    "params": { "type": "array", "items": { "type": "object", "properties": {
                      "name": { "type": "string" }, "type_ref": { "type": "string" } }, "required": ["name"] } },
                    "return_type": { "type": "string" },
                    "raw": { "type": "string" }
                  } }
                }, "required": ["id", "label"] } } },
              "groups": { "type": "array", "description": "Scalar cells per group, for aggregation queries (count/projection).",
                "items": { "type": "array", "items": { "type": "string" } } }
          } } },
        { "name": "describe_node", "description": "Compact 'takes X, returns Y, calls Z' description of a symbol, composed from its captured signature and outgoing call edges (graph-only, no source read). Useful for generating tool/function descriptions or quickly understanding a function's shape. For a class/type it lists the members instead (a class has no calls of its own). Resolve `label` by bare name, full label, id, or file.",
          "inputSchema": { "type": "object", "properties": {
              "label": { "type": "string", "description": "Symbol to describe (bare name, label, node id, or source file). Qualify a shared name as 'name@file'." }
          }, "required": ["label"] },
          "outputSchema": { "type": "object", "properties": {
              "found": { "type": "boolean" },
              "id": { "type": "string" },
              "label": { "type": "string" },
              "kind": { "type": "string" },
              "summary": { "type": "string", "description": "The one-line 'takes X, returns Y, calls Z' description." },
              "callees": { "type": "array", "items": { "type": "string" }, "description": "Distinct outgoing call-target labels." },
              "signature": { "type": "object", "description": "Captured signature: params (name + optional type_ref), optional return_type, raw header." },
              "members": { "type": "array", "items": { "type": "string" }, "description": "For a class/type only: its member symbol labels (a type has no calls of its own; capped at 40)." },
              "member_count": { "type": "integer", "description": "For a class/type: total members folded in (may exceed the capped `members` list)." },
              "dynamically_referenced": { "type": "boolean", "description": "True when a reflection site names this symbol, so it may be reachable only at runtime." },
              "dynamic_caveat": { "type": "object", "description": "Present when this symbol has 0 static dependents but a dynamic site could reach it.", "properties": { "opaque_sites_in_scope": {"type":"integer"}, "kinds": {"type":"array","items":{"type":"string"}}, "dynamically_referenced": {"type":"boolean"}, "message": {"type":"string"} } },
              "query": { "type": "string", "description": "Echo of the input label when found=false." },
              "ambiguous": { "type": "boolean", "description": "true when the name resolved to several nodes (found=false); see candidates." },
              "candidates": { "type": "array", "items": { "type": "object", "properties": {
                  "id": {"type":"string"}, "file": {"type":"string"}, "degree": {"type":"integer"} } }, "description": "Disambiguation candidates when the name is ambiguous (found=false), matching get_node/affected." }
          }, "required": ["found"] } },
        { "name": "time_travel_diff", "description": "How the code graph changed between two git revisions: added/removed module dependencies, removed APIs, architectural drift, new cycles, and hotspots. Builds each revision in a throwaway git worktree (slow on a cold repo).",
          "inputSchema": { "type": "object", "properties": {
              "rev1": { "type": "string", "description": "Base revision (e.g. HEAD~10, a branch, or a SHA)." },
              "rev2": { "type": "string", "description": "Target revision (default: the current working tree)." },
              "top": { "type": "integer", "description": "Max rows per ranked section (default 20)." }
          }, "required": ["rev1"] } },
        { "name": "plan_rename", "description": "Plan-only: a confidence-scored rename plan for an agent to apply. Returns the actual edit sites (file:line:col, old -> new, reason, confidence) plus the review-needed sites, so you can apply the rename without a second round-trip. Never edits source. Use `synaptic refactor verify` on the CLI after applying.",
          "inputSchema": { "type": "object", "properties": {
              "name": { "type": "string", "description": "The symbol to rename (its name, or a node id)." },
              "to": { "type": "string", "description": "The new name." },
              "id": { "type": "string", "description": "Disambiguate by node id when several definitions share the name." },
              "file": { "type": "string", "description": "Disambiguate by file-path substring." },
              "limit": { "type": "integer", "description": "Max sites listed per section (Edits, Review) before a '+N more' summary (default 50). Ignored when verbose=true." },
              "verbose": { "type": "boolean", "description": "List every edit/review site instead of the summarized top-N (default false)." }
          }, "required": ["name", "to"] } },
        { "name": "predict_impact", "description": "Full forecast for a multi-file change BEFORE editing (superset of affected_tests): changed nodes, the reverse-impact blast radius, public-API breaks (callers outside the module), and a verify checklist. Pure-graph; for new-cycle / removed-API detection use time_travel_diff.",
          "inputSchema": { "type": "object", "properties": {
              "files": { "type": "array", "items": { "type": "string" }, "description": "Repo-relative changed files to forecast. Omit to use the working-tree diff vs `base`." },
              "base": { "type": "string", "description": "Base branch to diff against when `files` is omitted (default: the repo's default branch)." },
              "depth": { "type": "integer", "description": "Reverse-impact hop bound (default 3, max 16)." },
              "limit": { "type": "integer", "description": "Max entries shown per section before a '+N more' summary (default 20). Ignored when verbose=true." },
              "verbose": { "type": "boolean", "description": "List all instead of the top-N summary (default false)." }
          } },
          "outputSchema": { "type": "object", "description": "The full ChangeForecast. The structured channel is not truncated by `limit` (that caps only the text); blast_radius is bounded by the forecast's internal hit cap, with blast_radius_total carrying the true count.", "properties": {
              "summary": {"type":"string"},
              "changed_files": { "type": "array", "items": {"type":"string"} },
              "changed_nodes": { "type": "array", "items": {"type":"object"} },
              "public_api_breaks": { "type": "array", "items": {"type":"object"} },
              "blast_radius": { "type": "array", "items": {"type":"object"} },
              "blast_radius_total": {"type":"integer"},
              "at_risk_tests": { "type": "array", "items": {"type":"object"} },
              "verify_checklist": { "type": "array", "items": {"type":"object"} }
          }, "required": ["summary","changed_files","blast_radius_total"] } },
        { "name": "affected_tests", "description": "Predictive test selection: the tests that exercise the changed code, found by walking the reverse-impact set from the changed files and keeping the test nodes (detected by path convention). The focused 'which tests should I run for this change' view.",
          "inputSchema": { "type": "object", "properties": {
              "files": { "type": "array", "items": { "type": "string" }, "description": "Repo-relative changed files. Omit to use the working-tree diff vs `base`." },
              "base": { "type": "string", "description": "Base branch to diff against when `files` is omitted (default: the repo's default branch)." },
              "depth": { "type": "integer", "description": "Reverse-impact hop bound (default 3, max 16)." }
          } },
          "outputSchema": { "type": "object", "properties": {
              "tests": { "type": "array", "items": { "type": "object", "properties": {
                  "id": {"type":"string"}, "label": {"type":"string"}, "file": {"type":"string"},
                  "depth": {"type":"integer"}, "via_relation": {"type":"string"} } } },
              "total": {"type":"integer"}
          }, "required": ["tests","total"] } },
        { "name": "predict_edit", "description": "What breaks if you make a specific kind of edit to a symbol, classified into 'will break' vs 'to review'. kind=delete (every dependent breaks), signature (callers/type-users break, bare imports go to review), or visibility (references from other files break when narrowing to private). Pure-graph; complements plan_rename (which is rename-only).",
          "inputSchema": { "type": "object", "properties": {
              "symbol": { "type": "string", "description": "The symbol to edit: its name, bare name, or a node id. Qualify a shared name as 'name@file'." },
              "kind": { "type": "string", "enum": ["delete", "signature", "visibility"], "description": "The edit kind (see above for what each breaks)." },
              "depth": { "type": "integer", "description": "Reverse-impact hop bound (default 3, max 16)." },
              "limit": { "type": "integer", "description": "Max entries shown per section (will break / review) before a '+N more' summary (default 20). Each section also prints a by-depth rollup. Ignored when verbose=true." },
              "verbose": { "type": "boolean", "description": "List all instead of the top-N summary (default false)." }
          }, "required": ["symbol", "kind"] } },
        { "name": "audit_sql", "description": "Audit the codebase's SQL for performance and security problems over the SQL-aware graph: RLS gaps, over-broad grants, likely SQL injection, missing indexes on filter/FK columns, SELECT *, non-sargable predicates, missing primary keys. Findings are ranked by severity then confidence; each carries a severity, confidence, location, and fix. To vet a single query you are drafting, use advise_sql.",
          "inputSchema": { "type": "object", "properties": {
              "severity": { "type": "string", "enum": ["critical","high","medium","low","info"], "description": "Only return findings at least this severe (default: all)." },
              "limit": { "type": "integer", "description": "Max findings returned before a '+N more' summary (default 20). Ignored when verbose=true." },
              "verbose": { "type": "boolean", "description": "Return all findings AND each finding's full detail + fix, instead of the terse one-line-per-finding summary (default false)." }
          } },
          "outputSchema": { "type": "object", "properties": {
              "version": {"type":"integer"}, "summary": {"type":"string"},
              "findings": { "type": "array", "items": { "type": "object", "properties": {
                  "rule_id": {"type":"string"}, "severity": {"type":"string"}, "category": {"type":"string"},
                  "title": {"type":"string"}, "detail": {"type":"string"}, "location": {"type":"string"},
                  "remediation": {"type":"string"}, "confidence": {"type":"number"} } } }
          }, "required": ["version","summary","findings"] } },
        { "name": "advise_sql", "description": "Critique a single candidate SQL query BEFORE writing it. Runs the same performance + security checks on the query text and cross-references the graph: whether the referenced tables exist, are behind row-level security, and have indexes on the columns you filter on. Use this while drafting SQL to write fast, safe queries.",
          "inputSchema": { "type": "object", "properties": {
              "query": { "type": "string", "description": "The SQL query to critique." },
              "dialect": { "type": "string", "enum": ["postgres","mysql","mssql","sqlite"], "description": "Optional dialect hint." }
          }, "required": ["query"] },
          "outputSchema": { "type": "object", "properties": {
              "version": {"type":"integer"}, "summary": {"type":"string"},
              "findings": { "type": "array", "items": { "type": "object", "properties": {
                  "rule_id": {"type":"string"}, "severity": {"type":"string"}, "category": {"type":"string"},
                  "title": {"type":"string"}, "detail": {"type":"string"}, "location": {"type":"string"},
                  "remediation": {"type":"string"}, "confidence": {"type":"number"} } } }
          }, "required": ["version","summary","findings"] } },
        { "name": "search_text", "description": "Regex/literal content search over source files (complements structural_search, which matches the graph, not file text). Use for string literals, config values, log/error messages, TODO wording. Each hit is attributed to its enclosing symbol, so pivot to affected/find_callers. Searches all federated members (scope with `repo`); skips Synaptic's own output dirs. Regex by default (literal=true for a fixed string); smart-case.",
          "inputSchema": { "type": "object", "properties": {
              "pattern": { "type": "string", "description": "Regex (default) or, with literal=true, a fixed string to find in file content." },
              "literal": { "type": "boolean", "description": "Treat `pattern` as a literal string rather than a regex (default false)." },
              "case_sensitive": { "type": "boolean", "description": "Force case sensitivity. Omit for smart case: case-insensitive unless `pattern` has an uppercase letter (true = always sensitive, false = always insensitive)." },
              "repo": { "type": "string", "description": "Restrict to one federated member repo (tag from list_repos). Works even when the graph is served over a single parent source root. Omit to search every member / the single repo." },
              "path_glob": { "type": "string", "description": "Only search files matching this glob, e.g. '**/*.ts' or 'src/**'. Applied relative to each repo root." },
              "max_results": { "type": "integer", "description": "Max hits to return before truncation is flagged (default 100, max 1000)." }
          }, "required": ["pattern"] },
          "outputSchema": { "type": "object", "properties": {
              "pattern": {"type":"string"}, "total": {"type":"integer"}, "truncated": {"type":"boolean"},
              "files_scanned": {"type":"integer"},
              "hits": { "type": "array", "items": { "type": "object", "properties": {
                  "repo": {"type":["string","null"]}, "file": {"type":"string"}, "line": {"type":"integer"},
                  "col": {"type":"integer"}, "match": {"type":"string"}, "line_text": {"type":"string"},
                  "node": { "type": ["object","null"], "description": "The enclosing graph symbol (null if the hit is outside any captured span). Pivot from here to affected/find_callers.", "properties": {
                      "id": {"type":"string"}, "label": {"type":"string"}, "kind": {"type":"string"}, "community": {"type":"integer"} } } } } }
          }, "required": ["pattern","total","hits"] } }
    ]);
    // The single command-running tool, exposed only when the operator opted in
    // with --allow-exec. It is NOT read-only (it executes the project's tests /
    // build in a throwaway worktree), so it is annotated honestly below and kept
    // out of the default, strictly-read-only tool surface.
    if allow_exec {
        tools.as_array_mut().unwrap().push(json!({
            "name": "speculate",
            "description": "Run a proposed change for real: apply it in a throwaway git worktree and run the forecast's at-risk tests plus a build/type-check, reporting actual pass/fail. NOT read-only (it executes commands); available only because the server was started with --allow-exec. Use predict_impact/affected_tests first to forecast; use this to confirm.",
            "inputSchema": { "type": "object", "properties": {
                "files": { "type": "array", "items": { "type": "string" }, "description": "Repo-relative changed files. Omit to use the working-tree diff vs `base`. Explicit files also scope the applied diff." },
                "base": { "type": "string", "description": "Base branch to apply onto and diff against (default: the repo's default branch)." },
                "test_cmd": { "type": "string", "description": "Test command template; `{files}` expands to the at-risk test files. Omit to auto-detect (cargo/go/pytest/npm)." },
                "check_cmd": { "type": "string", "description": "Build / type-check command, run before the tests. Omit to auto-detect." },
                "depth": { "type": "integer", "description": "Reverse-impact hop bound for selecting at-risk tests (default 3, max 16)." },
                "timeout": { "type": "integer", "description": "Per-command wall-clock budget in seconds (default 300, max 3600)." },
                "max_tests": { "type": "integer", "description": "Cap on the number of at-risk test files run (default 20)." }
            } }
        }));
    }
    // Every read tool is a pure read; the PR tools and time_travel_diff
    // additionally reach the environment (gh / git worktrees), so they carry
    // openWorldHint. `speculate` is the lone non-read-only, open-world exception.
    let open_world = [
        "list_prs",
        "get_pr_impact",
        "triage_prs",
        "working_changes_impact",
        "predict_impact",
        "affected_tests",
        "time_travel_diff",
    ];
    for t in tools.as_array_mut().unwrap() {
        let name = t["name"].as_str().unwrap_or("").to_string();
        if name == "speculate" {
            t["annotations"] = json!({
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": true,
            });
        } else {
            t["annotations"] = json!({
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": open_world.contains(&name.as_str()),
            });
        }
    }
    tools
}

/// The MCP `resources/list` payload.
fn resources_list() -> Value {
    json!([
        { "uri": "synaptic://report", "name": "Graph report", "mimeType": "text/markdown" },
        { "uri": "synaptic://stats", "name": "Graph stats", "mimeType": "text/plain" },
        { "uri": "synaptic://god-nodes", "name": "God nodes", "mimeType": "text/plain" },
        { "uri": "synaptic://surprises", "name": "Surprising connections", "mimeType": "text/plain" },
        { "uri": "synaptic://audit", "name": "Confidence audit", "mimeType": "text/plain" },
        { "uri": "synaptic://questions", "name": "Suggested questions", "mimeType": "text/plain" }
    ])
}

/// The MCP `resources/templates/list` payload: any node or community is
/// addressable as a resource by URI.
fn resource_templates() -> Value {
    json!([
        { "uriTemplate": "synaptic://node/{label}", "name": "Node", "mimeType": "text/plain",
          "description": "Metadata for one node by label, id, or bare name." },
        { "uriTemplate": "synaptic://community/{id}", "name": "Community", "mimeType": "text/plain",
          "description": "Members of one community by id." }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::NodeKind;
    use synaptic_core::{Confidence, Edge, FileType};

    fn sql_graph() -> GraphData {
        serde_json::from_value(serde_json::json!({
            "nodes": [
                {"id":"sql:orders","label":"orders","file_type":"code","source_file":"s.sql","kind":"table","rls_enabled":false},
                {"id":"sql:orders:col:tenant_id","label":"tenant_id","file_type":"code","source_file":"s.sql","kind":"column"}
            ],
            "links": [{"source":"sql:orders","target":"sql:orders:col:tenant_id","relation":"has_column","confidence":"EXTRACTED","source_file":"s.sql"}]
        }))
        .unwrap()
    }

    #[test]
    fn audit_sql_tool_returns_structured_findings() {
        let srv = Server::from_graph_data(sql_graph(), None);
        let res = srv
            .dispatch_tool(&serde_json::json!({"name":"audit_sql","arguments":{}}))
            .unwrap();
        let findings = res["structuredContent"]["findings"].as_array().unwrap();
        assert!(
            findings.iter().any(|f| f["rule_id"] == "SEC-RLS-001"),
            "{res}"
        );
    }

    #[test]
    fn advise_sql_tool_critiques_a_candidate() {
        let srv = Server::from_graph_data(sql_graph(), None);
        let res = srv
            .dispatch_tool(&serde_json::json!({
                "name":"advise_sql",
                "arguments":{"query":"SELECT * FROM orders WHERE tenant_id = 1"}
            }))
            .unwrap();
        let findings = res["structuredContent"]["findings"].as_array().unwrap();
        assert!(
            findings.iter().any(|f| f["rule_id"] == "PERF-SEL-001"),
            "{res}"
        );
    }

    fn node(id: &str, label: &str, community: Option<u32>) -> synaptic_core::Node {
        synaptic_core::Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: format!("{id}.py"),
            source_location: Some("L1".into()),
            community,
            repo: None,
            extra: Map::new(),
        }
    }

    fn edge(s: &str, t: &str, rel: &str) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: rel.into(),
            confidence: Confidence::Extracted,
            source_file: "x.py".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    fn server() -> Server {
        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                node("auth", "AuthService", Some(0)),
                node("login", "login_user", Some(0)),
                node("db", "Database", Some(1)),
            ],
            links: vec![edge("auth", "login", "calls"), edge("auth", "db", "uses")],
            hyperedges: vec![],
            built_at_commit: None,
        };
        Server::from_graph_data(gd, None)
    }

    /// A graph with two dynamic-dispatch sites: an opaque reflection call in
    /// `dispatcher.py` and an `eval` in `evil.py`.
    fn hazard_server() -> Server {
        let mut a = node("dispatcher", "dispatch", Some(0));
        a.push_dynamic_site(synaptic_core::DynamicSite {
            kind: synaptic_core::DynamicKind::Reflection,
            line: 5,
            key: None,
            snippet: "h[k]()".into(),
        });
        let mut b = node("evil", "run_eval", Some(0));
        b.push_dynamic_site(synaptic_core::DynamicSite {
            kind: synaptic_core::DynamicKind::Eval,
            line: 2,
            key: None,
            snippet: "eval(x)".into(),
        });
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![a, b],
            links: vec![],
            hyperedges: vec![],
            built_at_commit: None,
        };
        Server::from_graph_data(gd, None)
    }

    #[test]
    fn dynamic_hazards_lists_sites_with_kind_and_location() {
        let mut s = hazard_server();
        let resp = call_tool_full(&mut s, "dynamic_hazards", json!({}));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("reflection"), "{text}");
        let structured = &resp["result"]["structuredContent"];
        assert!(structured["total"].as_u64().unwrap() >= 2, "{structured}");
        assert!(
            structured["sites"][0]["line"].as_u64().is_some(),
            "{structured}"
        );
    }

    #[test]
    fn dynamic_hazards_filters_by_kind() {
        let mut s = hazard_server();
        let resp = call_tool_full(&mut s, "dynamic_hazards", json!({"kind": "eval"}));
        let structured = &resp["result"]["structuredContent"];
        let sites = structured["sites"].as_array().unwrap();
        assert!(!sites.is_empty(), "{structured}");
        assert!(sites.iter().all(|x| x["kind"] == "eval"), "{structured}");
    }

    #[test]
    fn affected_appends_dynamic_caveat_for_zero_dep_node_with_reflection() {
        let mut s = hazard_server();
        // 'dispatch' has no static dependents but its file holds an opaque reflection
        // site, so a bare "0 dependents" must carry the honesty caveat.
        let text = call_tool(&mut s, "affected", json!({"label": "dispatch"}));
        assert!(text.contains("not provably unused"), "text caveat: {text}");
        let full = call_tool_full(&mut s, "affected", json!({"label": "dispatch"}));
        let sc = &full["result"]["structuredContent"];
        assert!(sc["dynamic_caveat"].is_object(), "structured caveat: {sc}");
    }

    /// A handler reached only by an evidence-linked `dynamic_ref` edge: flagged
    /// `dynamically_referenced`, surfaced by get_node and god_nodes.
    fn dynamic_ref_server() -> Server {
        let mut tgt = node("handler", "on_event", Some(0));
        tgt.set_dynamically_referenced(true);
        let caller = node("c", "caller", Some(0));
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![tgt, caller],
            links: vec![edge("c", "handler", "dynamic_ref")],
            hyperedges: vec![],
            built_at_commit: None,
        };
        Server::from_graph_data(gd, None)
    }

    #[test]
    fn get_node_surfaces_dynamically_referenced_flag() {
        let mut s = dynamic_ref_server();
        let full = call_tool_full(&mut s, "get_node", json!({"label": "on_event"}));
        let sc = &full["result"]["structuredContent"];
        assert_eq!(sc["dynamically_referenced"], json!(true), "{sc}");
    }

    #[test]
    fn graph_stats_reports_dynamic_dispatch_counts() {
        let mut s = hazard_server();
        let text = call_tool(&mut s, "graph_stats", json!({}));
        assert!(text.contains("Dynamic-dispatch sites:"), "{text}");
        let full = call_tool_full(&mut s, "graph_stats", json!({}));
        let sc = &full["result"]["structuredContent"];
        assert_eq!(sc["dynamic_sites"], json!(2), "{sc}");
        assert_eq!(sc["dynamic_sites_opaque"], json!(2), "{sc}");
    }

    #[test]
    fn god_nodes_annotates_dynamically_referenced_hub() {
        let mut s = dynamic_ref_server();
        let full = call_tool_full(&mut s, "god_nodes", json!({"top_n": 10}));
        let gods = full["result"]["structuredContent"]["god_nodes"]
            .as_array()
            .unwrap();
        assert!(
            gods.iter()
                .any(|g| g["label"] == "on_event" && g["dynamically_referenced"] == json!(true)),
            "{gods:?}"
        );
    }

    fn call_tool(s: &mut Server, name: &str, args: Value) -> String {
        let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}});
        let resp = s.handle_request(&req).unwrap();
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn call_tool_full(s: &mut Server, name: &str, args: Value) -> Value {
        let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}});
        s.handle_request(&req).unwrap()
    }

    /// A hub node with 25 outgoing 'calls' neighbors, for the cap / verbose /
    /// concise-default tests.
    fn hub_server() -> Server {
        let mut nodes = vec![node("hub", "Hub", Some(0))];
        let mut links = Vec::new();
        for i in 0..25 {
            nodes.push(node(&format!("n{i}"), &format!("dep{i}"), Some(0)));
            links.push(edge("hub", &format!("n{i}"), "calls"));
        }
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes,
            links,
            hyperedges: vec![],
            built_at_commit: None,
        };
        Server::from_graph_data(gd, None)
    }

    #[test]
    fn get_neighbors_caps_with_limit_and_verbose_lists_all() {
        let mut s = hub_server();
        let capped = call_tool(&mut s, "get_neighbors", json!({"label":"hub","limit":5}));
        assert!(
            capped.contains("Neighbors of Hub (25)"),
            "header carries the total: {capped}"
        );
        assert!(capped.contains("+20 more"), "cap note present: {capped}");
        assert_eq!(
            capped.lines().filter(|l| l.contains("-->")).count(),
            5,
            "limit caps the rendered neighbors: {capped}"
        );
        let full = call_tool(
            &mut s,
            "get_neighbors",
            json!({"label":"hub","verbose":true}),
        );
        assert!(
            !full.contains("more"),
            "verbose lists all, no cap note: {full}"
        );
        assert_eq!(full.lines().filter(|l| l.contains("-->")).count(), 25);
    }

    #[test]
    fn get_neighbors_structured_mirror_caps_and_reports_total() {
        let mut s = hub_server();
        let full = call_tool_full(&mut s, "get_neighbors", json!({"label":"hub","limit":5}));
        let sc = &full["result"]["structuredContent"];
        assert_eq!(sc["neighbors"].as_array().unwrap().len(), 5, "{sc}");
        assert_eq!(sc["total"], json!(25));
        assert_eq!(sc["truncated"], json!(true));
    }

    #[test]
    fn concise_mode_lowers_get_neighbors_default() {
        // The default neighbor cap is 50 (does not trim 25); concise lowers it to 20.
        let mut normal = hub_server();
        let n = call_tool(&mut normal, "get_neighbors", json!({"label":"hub"}));
        assert!(
            !n.contains("more"),
            "non-concise default does not cap 25: {n}"
        );
        let mut lean = hub_server().with_concise(true);
        let c = call_tool(&mut lean, "get_neighbors", json!({"label":"hub"}));
        assert!(c.contains("+5 more"), "concise default caps at 20: {c}");
    }

    #[test]
    fn query_graph_terse_by_default_then_full_includes_edges() {
        let mut s = server();
        let terse = query_graph_structured(&mut s, json!({"question":"auth login database"}));
        assert!(
            terse["structuredContent"]["edges"]
                .as_array()
                .unwrap()
                .is_empty(),
            "terse default omits edges: {}",
            terse["structuredContent"]
        );
        assert!(
            !terse["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("EDGE "),
            "terse text has no EDGE lines: {}",
            terse["content"][0]["text"]
        );
        let full = query_graph_structured(
            &mut s,
            json!({"question":"auth login database","full":true}),
        );
        assert!(
            !full["structuredContent"]["edges"]
                .as_array()
                .unwrap()
                .is_empty(),
            "full=true includes the subgraph edges: {}",
            full["structuredContent"]
        );
    }

    #[test]
    fn audit_sql_terse_one_line_default_verbose_adds_fix() {
        let mut s = Server::from_graph_data(sql_graph(), None);
        let terse = call_tool(&mut s, "audit_sql", json!({}));
        assert!(
            !terse.contains("fix:"),
            "terse default omits the fix line: {terse}"
        );
        assert!(
            terse.contains("pass verbose=true"),
            "terse default hints at verbose: {terse}"
        );
        let full = call_tool(&mut s, "audit_sql", json!({"verbose":true}));
        assert!(full.contains("fix:"), "verbose adds the fix line: {full}");
    }

    /// A class `MyClass` owning methods `doThing`/`helper`; an external function
    /// `caller` calls doThing; doThing calls helper (internal coupling). The
    /// class's only incoming edge is the `method` ownership, so the bare class
    /// node has ~0 reverse-impact — impact lives on its members.
    fn class_server() -> Server {
        let mut cls = node("c", "MyClass", Some(0));
        cls.set_kind(NodeKind::Class);
        let mut m1 = node("m1", "doThing", Some(0));
        m1.set_kind(NodeKind::Method);
        let mut m2 = node("m2", "helper", Some(0));
        m2.set_kind(NodeKind::Method);
        let caller = node("caller", "caller", Some(0));
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![cls, m1, m2, caller],
            links: vec![
                edge("c", "m1", "method"),
                edge("c", "m2", "method"),
                edge("caller", "m1", "calls"),
                edge("m1", "m2", "calls"),
            ],
            hyperedges: vec![],
            built_at_commit: None,
        };
        Server::from_graph_data(gd, None)
    }

    #[test]
    fn affected_on_a_class_folds_member_impact_and_labels_it() {
        let s = class_server();
        let out = s.tool_affected("MyClass", 5, &[], 50, false);
        // The class is labelled and the member-callers are surfaced (not "Nothing
        // depends on MyClass"): caller (calls doThing) and doThing (calls helper).
        assert!(
            out.contains("MyClass is a class with 2 members"),
            "note: {out}"
        );
        assert!(out.contains("caller"), "external caller folded in: {out}");
        assert!(
            out.contains("doThing"),
            "internal member-caller folded in: {out}"
        );
    }

    #[test]
    fn affected_structured_on_a_class_is_not_a_misleading_zero() {
        let mut s = class_server();
        let resp = call_tool_full(&mut s, "affected", json!({"label": "MyClass", "depth": 5}));
        let sc = &resp["result"]["structuredContent"];
        assert!(
            sc["total"].as_u64().unwrap() >= 2,
            "class total must reflect folded members, got {sc}"
        );
        assert_eq!(sc["aggregated_over_members"], json!(2), "{sc}");
    }

    #[test]
    fn find_callers_on_a_class_folds_members_external_only() {
        let s = class_server();
        let out = s.tool_find_callers("MyClass", 50, false, false);
        assert!(
            out.contains("MyClass is a class with 2 members"),
            "note: {out}"
        );
        // External caller of a method is surfaced; the class's own members are not
        // listed as callers of themselves.
        assert!(out.contains("caller"), "external caller folded in: {out}");
        assert!(
            !out.contains("\n  helper "),
            "members not listed as callers: {out}"
        );
    }

    #[test]
    fn describe_node_on_a_class_lists_members() {
        let s = class_server();
        let out = s.tool_describe_node("MyClass");
        assert!(out.contains("Members (2):"), "members listed: {out}");
        assert!(out.contains("doThing") && out.contains("helper"), "{out}");
    }

    #[test]
    fn get_community_excludes_external_stubs() {
        // A community holding a real symbol plus an import stub (empty source_file,
        // e.g. `@acme/router`). The stub is noise and must not be listed; the
        // total reflects only real members.
        let real = node("real", "RealThing", Some(0));
        let mut stub = node("pkg", "@acme/router", Some(0));
        stub.source_file = String::new();
        // A rationale (captured TODO comment) node also carries a community label
        // but is not a code symbol.
        let mut todo = node("todo", "// TODO: handle the edge case", Some(0));
        todo.file_type = FileType::Rationale;
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![real, stub, todo],
            links: vec![],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let mut s = Server::from_graph_data(gd, None);
        let out = call_tool(&mut s, "get_community", json!({"community_id": 0}));
        assert!(out.contains("RealThing"), "real member shown: {out}");
        assert!(!out.contains("@acme/router"), "stub excluded: {out}");
        assert!(!out.contains("TODO"), "rationale comment excluded: {out}");
        assert!(
            out.contains("showing 1 of 1"),
            "total excludes noise: {out}"
        );
    }

    #[test]
    fn list_repos_surfaces_per_repo_source_hash() {
        // A federated graph file with a workspace-state.json sibling: list_repos
        // must surface each member's source fingerprint so per-repo drift is
        // visible.
        let dir = tempfile::tempdir().unwrap();
        let mut n = node("alpha::x", "X", Some(0));
        n.repo = Some("alpha".into());
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![n],
            links: vec![],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let gpath = dir.path().join("graph.json");
        std::fs::write(&gpath, serde_json::to_vec(&gd).unwrap()).unwrap();
        std::fs::write(
            dir.path().join("workspace-state.json"),
            r#"{"members":{"alpha":{"source_hash":"abcdef0123456789deadbeef","surface_hash":"s"}}}"#,
        )
        .unwrap();
        let mut s = Server::load(gpath).unwrap();
        let resp = call_tool_full(&mut s, "list_repos", json!({}));
        let repos = resp["result"]["structuredContent"]["repos"]
            .as_array()
            .unwrap();
        assert_eq!(repos[0]["repo"], "alpha");
        assert_eq!(
            repos[0]["source_hash"], "abcdef012345",
            "12-char fingerprint: {repos:?}"
        );
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("src abcdef012345"),
            "text fingerprint: {text}"
        );
    }

    #[test]
    fn affected_structured_surfaces_ambiguity_instead_of_zero() {
        // Two nodes share the bare name "Dup"; the text path refuses with a
        // candidate list, so the structured path must NOT silently pick one and
        // report total:0 (which reads as "nothing depends on it").
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                node("a_dup", "Dup", Some(0)),
                node("b_dup", "Dup", Some(0)),
                node("x", "x", Some(0)),
            ],
            links: vec![edge("x", "a_dup", "calls")],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let mut s = Server::from_graph_data(gd, None);
        let resp = call_tool_full(&mut s, "affected", json!({"label": "Dup"}));
        let sc = &resp["result"]["structuredContent"];
        assert_eq!(sc["resolved"], json!(false), "must flag unresolved: {sc}");
        assert_eq!(sc["ambiguous"], json!(true), "{sc}");
        assert!(
            sc["candidates"]
                .as_array()
                .map(|a| a.len() >= 2)
                .unwrap_or(false),
            "candidates listed: {sc}"
        );
    }

    #[test]
    fn get_node_structured_mirrors_metadata_and_ambiguity() {
        // Unique name: structured metadata with found:true.
        let mut s = server();
        let resp = call_tool_full(&mut s, "get_node", json!({"label": "AuthService"}));
        let sc = &resp["result"]["structuredContent"];
        assert_eq!(sc["found"], json!(true), "{sc}");
        assert_eq!(sc["label"], "AuthService");
        assert!(sc["degree"].as_u64().is_some(), "degree present: {sc}");

        // Ambiguous name: structured channel surfaces it like affected/describe_node
        // (was text-only before), instead of silently picking one.
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![node("a_dup", "Dup", Some(0)), node("b_dup", "Dup", Some(0))],
            links: vec![],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let mut s2 = Server::from_graph_data(gd, None);
        let resp = call_tool_full(&mut s2, "get_node", json!({"label": "Dup"}));
        let sc = &resp["result"]["structuredContent"];
        assert_eq!(sc["found"], json!(false), "{sc}");
        assert_eq!(sc["ambiguous"], json!(true), "{sc}");
        assert!(
            sc["candidates"]
                .as_array()
                .map(|a| a.len() >= 2)
                .unwrap_or(false),
            "candidates listed: {sc}"
        );
    }

    #[test]
    fn autofresh_picks_up_a_new_file_on_query() {
        use std::fs;
        // A graph built from alpha.py only. After serving, an agent writes a new
        // beta.py and queries it: the on-query catch-up must extract beta.py so
        // the new symbol is queryable without any external watch/update.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let out = root.join("synaptic-out");
        fs::create_dir_all(&out).unwrap();
        fs::write(root.join("alpha.py"), "def alpha_func():\n    return 1\n").unwrap();

        let opts = synaptic_incremental::RebuildOptions {
            root: root.clone(),
            directed: false,
            force: false,
        };
        let outcome =
            synaptic_incremental::rebuild(&opts, &synaptic_incremental::ChangeSet::Full, None)
                .unwrap();
        let graph_path = out.join("graph.json");
        fs::write(
            &graph_path,
            serde_json::to_vec(&outcome.kg.to_graph_data()).unwrap(),
        )
        .unwrap();
        synaptic_incremental::persist_manifest(&out, &root).unwrap();

        let mut server = Server::load(graph_path)
            .unwrap()
            .with_source_root(root.clone());
        assert!(
            !server.kg.nodes().any(|n| n.label.contains("beta_func")),
            "beta_func absent before the file is written"
        );

        fs::write(root.join("beta.py"), "def beta_func():\n    return 2\n").unwrap();
        let text = call_tool(
            &mut server,
            "query_graph",
            json!({ "question": "beta_func" }),
        );
        assert!(
            text.contains("beta_func"),
            "new file's symbol must be queryable after auto-freshen: {text}"
        );
    }

    #[test]
    fn autofresh_applies_a_symbol_removal_on_query() {
        use std::fs;
        // Removing a method from a still-existing file is a bounded shrink; the
        // on-query catch-up must apply it so the deleted symbol leaves the graph.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let out = root.join("synaptic-out");
        fs::create_dir_all(&out).unwrap();
        fs::write(
            root.join("m.py"),
            "def keep_func():\n    return 1\n\n\ndef gone_func():\n    return 2\n",
        )
        .unwrap();

        let opts = synaptic_incremental::RebuildOptions {
            root: root.clone(),
            directed: false,
            force: false,
        };
        let outcome =
            synaptic_incremental::rebuild(&opts, &synaptic_incremental::ChangeSet::Full, None)
                .unwrap();
        let graph_path = out.join("graph.json");
        fs::write(
            &graph_path,
            serde_json::to_vec(&outcome.kg.to_graph_data()).unwrap(),
        )
        .unwrap();
        synaptic_incremental::persist_manifest(&out, &root).unwrap();

        let mut server = Server::load(graph_path)
            .unwrap()
            .with_source_root(root.clone());
        assert!(
            server.kg.nodes().any(|n| n.label.contains("gone_func")),
            "symbol present before the edit"
        );

        // Delete gone_func() from the file, then query.
        fs::write(root.join("m.py"), "def keep_func():\n    return 1\n").unwrap();
        let _ = call_tool(
            &mut server,
            "query_graph",
            json!({ "question": "keep_func" }),
        );
        assert!(
            !server.kg.nodes().any(|n| n.label.contains("gone_func")),
            "removed symbol must leave the graph after auto-freshen"
        );
        assert!(
            server.kg.nodes().any(|n| n.label.contains("keep_func")),
            "kept symbol remains"
        );
    }

    #[test]
    fn every_tool_and_param_is_documented() {
        // Findings #1/#3: each tool needs a substantive description, and every
        // input-schema property needs its own description so agents use it right.
        // Use the full surface (incl. the opt-in speculate tool) so its schema is
        // documented too.
        let tools = tools_list(true);
        for t in tools.as_array().unwrap() {
            let name = t["name"].as_str().unwrap();
            assert!(
                t["description"]
                    .as_str()
                    .map(|d| d.len() > 20)
                    .unwrap_or(false),
                "tool {name} needs a substantive description"
            );
            if let Some(props) = t["inputSchema"]["properties"].as_object() {
                for (pname, schema) in props {
                    assert!(
                        schema
                            .get("description")
                            .and_then(Value::as_str)
                            .map(|d| !d.is_empty())
                            .unwrap_or(false),
                        "tool {name} param '{pname}' needs a description"
                    );
                }
            }
        }
    }

    #[test]
    fn tool_surface_is_plain_ascii() {
        // The instructions + tool descriptions are agent-facing output; keep them
        // free of em-dashes / smart quotes / arrows (AI tells).
        let mut text = SERVER_INSTRUCTIONS.to_string();
        text.push_str(&tools_list(true).to_string());
        for t in [
            '\u{2014}', '\u{2013}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2192}',
        ] {
            assert!(!text.contains(t), "AI tell {t:?} in tool surface");
        }
    }

    #[test]
    fn tool_surface_documents_at_file_disambiguation() {
        // The `name@file` qualifier works on every name-taking tool; the schema and
        // instructions must advertise it (not just predict_edit) so an agent reading
        // tools/list discovers it. Guards the discoverability fix.
        let tools = tools_list(true);
        let arr = tools.as_array().unwrap();
        let find = |name: &str| {
            arr.iter()
                .find(|t| t["name"] == name)
                .unwrap_or_else(|| panic!("tool {name} missing"))
        };
        for (name, param) in [
            ("get_node", "label"),
            ("get_source", "label"),
            ("get_neighbors", "label"),
            ("describe_node", "label"),
            ("affected", "label"),
            ("find_callers", "label"),
            ("find_callees", "label"),
            ("find_references", "label"),
            ("shortest_path", "source"),
            ("shortest_path", "target"),
            ("predict_edit", "symbol"),
        ] {
            let desc = find(name)["inputSchema"]["properties"][param]["description"]
                .as_str()
                .unwrap_or_else(|| panic!("{name}.{param} has no description"));
            assert!(
                desc.contains("@file"),
                "{name}.{param} should document the @file qualifier: {desc}"
            );
        }
        // The onboarding instructions explain it cross-cuttingly.
        assert!(
            SERVER_INSTRUCTIONS.contains("name@file-substring"),
            "instructions should explain @file disambiguation"
        );
        // god_nodes structured output advertises the per-hub test count.
        let props =
            &find("god_nodes")["outputSchema"]["properties"]["god_nodes"]["items"]["properties"];
        assert!(
            props.get("test_count").is_some(),
            "god_nodes outputSchema should declare test_count"
        );
    }

    #[test]
    fn structural_search_returns_structured_signature() {
        use synaptic_core::{Param, Signature};
        let mut greet = node("greet", "greet()", None);
        greet.set_kind(NodeKind::Function);
        greet.set_signature(Signature {
            params: vec![Param {
                name: "name".into(),
                type_ref: Some("str".into()),
            }],
            return_type: Some("str".into()),
            raw: "def greet(name: str) -> str".into(),
        });
        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![greet],
            links: vec![],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let mut s = Server::from_graph_data(gd, None);
        let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"structural_search","arguments":{"query":"MATCH (f:function) RETURN f"}}});
        let resp = s.handle_request(&req).unwrap();
        let sc = &resp["result"]["structuredContent"];
        let cell = &sc["results"][0][0];
        assert_eq!(cell["label"], "greet()", "structured row carries label");
        assert_eq!(cell["kind"], "function");
        assert_eq!(cell["signature"]["return_type"], "str");
        assert_eq!(cell["signature"]["params"][0]["name"], "name");
        assert_eq!(cell["signature"]["params"][0]["type_ref"], "str");
    }

    #[test]
    fn describe_node_tool_returns_summary_and_structured() {
        use synaptic_core::{Param, Signature};
        let mut greet = node("greet", "greet()", None);
        greet.set_kind(NodeKind::Function);
        greet.set_signature(Signature {
            params: vec![Param {
                name: "name".into(),
                type_ref: Some("str".into()),
            }],
            return_type: Some("str".into()),
            raw: "def greet(name: str) -> str".into(),
        });
        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![greet, node("parse", "parse()", None)],
            links: vec![edge("greet", "parse", "calls")],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let mut s = Server::from_graph_data(gd, None);

        let txt = call_tool(&mut s, "describe_node", json!({ "label": "greet" }));
        assert!(txt.contains("takes (name: str)"), "{txt}");
        assert!(txt.contains("returns str"), "{txt}");
        assert!(txt.contains("calls [parse()]"), "{txt}");

        let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"describe_node","arguments":{"label":"greet"}}});
        let resp = s.handle_request(&req).unwrap();
        let sc = &resp["result"]["structuredContent"];
        assert_eq!(sc["found"], true);
        assert_eq!(sc["signature"]["return_type"], "str");
        assert_eq!(sc["callees"][0], "parse()");
        assert!(sc["summary"]
            .as_str()
            .unwrap_or("")
            .contains("takes (name: str)"));
    }

    #[test]
    fn describe_node_structured_signature_preserves_generics() {
        // The structured signature must NOT HTML-escape generics (`Record<...>`),
        // since it feeds tool/function-description generation.
        use synaptic_core::{Param, Signature};
        let mut f = node("load", "loadWidget()", None);
        f.set_kind(NodeKind::Function);
        f.set_signature(Signature {
            params: vec![Param {
                name: "opts".into(),
                type_ref: Some("Record<string, unknown>".into()),
            }],
            return_type: Some("Promise<void>".into()),
            raw: "loadWidget(opts: Record<string, unknown>): Promise<void>".into(),
        });
        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![f],
            links: vec![],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let mut s = Server::from_graph_data(gd, None);
        let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"describe_node","arguments":{"label":"loadWidget"}}});
        let resp = s.handle_request(&req).unwrap();
        let sig = &resp["result"]["structuredContent"]["signature"];
        let raw = sig["raw"].as_str().unwrap_or("");
        assert!(
            raw.contains("Record<string, unknown>") && raw.contains("Promise<void>"),
            "generics preserved verbatim: {raw}"
        );
        assert!(
            !raw.contains("&lt;") && !raw.contains("&gt;"),
            "signature must not be HTML-escaped: {raw}"
        );
        assert_eq!(sig["return_type"], "Promise<void>");
        assert_eq!(sig["params"][0]["type_ref"], "Record<string, unknown>");
    }

    #[test]
    fn affected_truncates_with_per_depth_breakdown_and_verbose() {
        // A hub with many dependents must summarize (per-depth counts + "+N more")
        // by default, and dump everything under verbose=true.
        let mut nodes = vec![node("h", "hub", Some(0))];
        let mut links = Vec::new();
        for i in 0..6 {
            nodes.push(node(&format!("d{i}"), &format!("dep{i}"), Some(0)));
            links.push(edge(&format!("d{i}"), "h", "calls"));
        }
        // A depth-2 dependent (g -> d0 -> h).
        nodes.push(node("g", "grand", Some(0)));
        links.push(edge("g", "d0", "calls"));
        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes,
            links,
            hyperedges: vec![],
            built_at_commit: None,
        };
        let mut s = Server::from_graph_data(gd, None);

        let out = call_tool(
            &mut s,
            "affected",
            json!({"label":"hub","depth":3,"limit":2}),
        );
        assert!(
            out.contains("depth 1:") && out.contains("depth 2:"),
            "per-depth breakdown present: {out}"
        );
        assert!(
            out.contains("more; pass verbose=true"),
            "truncation note present: {out}"
        );
        // The body is capped at the limit (2 entry lines).
        let entry_lines = out.lines().filter(|l| l.contains("h via ")).count();
        assert_eq!(entry_lines, 2, "limit caps entries: {out}");

        let full = call_tool(&mut s, "affected", json!({"label":"hub","verbose":true}));
        assert!(
            !full.contains("more; pass verbose=true"),
            "verbose must not truncate: {full}"
        );
        assert_eq!(
            full.lines().filter(|l| l.contains("h via ")).count(),
            7,
            "verbose lists all 7 dependents: {full}"
        );
    }

    #[test]
    fn initialize_returns_orienting_instructions() {
        // Finding #2: the MCP initialize result should carry server `instructions`
        // that orient the agent to the whole toolset.
        let mut s = server();
        let req = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}});
        let resp = s.handle_request(&req).unwrap();
        let instr = resp["result"]["instructions"].as_str().unwrap_or("");
        assert!(instr.len() > 100, "instructions should orient: {instr}");
        assert!(instr.contains("query_graph"), "should name the entry tool");
        assert!(
            instr.to_lowercase().contains("graph"),
            "should frame the toolset"
        );
        // The gated speculate tool is invisible without --allow-exec, so the
        // instructions point at how to enable it (discoverability).
        assert!(
            instr.contains("--allow-exec") && instr.contains("speculate"),
            "instructions should explain how to enable speculate: {instr}"
        );
    }

    #[test]
    fn list_repos_and_repo_stats() {
        let mut a1 = node("a::x", "x", None);
        a1.repo = Some("a".into());
        let mut a2 = node("a::y", "y", None);
        a2.repo = Some("a".into());
        let mut b1 = node("b::z", "z", None);
        b1.repo = Some("b".into());
        let gd = GraphData {
            nodes: vec![a1, a2, b1],
            links: vec![
                edge("a::x", "a::y", "calls"),
                edge("a::y", "b::z", "imports"),
            ],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None);
        let repos = call_tool(&mut s, "list_repos", json!({}));
        assert!(repos.contains("Repos (2)"), "{repos}");
        assert!(repos.contains("a - 2 nodes, 2 edges"), "{repos}");
        assert!(repos.contains("b - 1 nodes, 0 edges"), "{repos}");
        let stats = call_tool(&mut s, "repo_stats", json!({"repo": "a"}));
        assert!(stats.contains("Repo a: 2 nodes, 2 edges"), "{stats}");
    }

    #[test]
    fn predict_impact_reports_blast_radius_for_changed_files() {
        // auth calls login, so changing login.py puts AuthService in the blast
        // radius. login_user is the changed node.
        let mut s = server();
        let out = call_tool(&mut s, "predict_impact", json!({"files": ["login.py"]}));
        assert!(out.contains("login_user"), "changed node listed: {out}");
        assert!(
            out.contains("AuthService"),
            "dependent in blast radius: {out}"
        );
    }

    #[test]
    fn predict_impact_flags_public_api_and_sanitizes_output() {
        let mut svc = node("svc", "Service", Some(0));
        svc.set_visibility(synaptic_core::Visibility::Public);
        // A label carrying a control char must be stripped before it reaches the LLM.
        let evil = node("evil", "ev\u{0}il", Some(0));
        let gd = GraphData {
            nodes: vec![svc, evil],
            links: vec![edge("evil", "svc", "calls")],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None);
        let out = call_tool(&mut s, "predict_impact", json!({"files": ["svc.py"]}));
        assert!(
            out.contains("Public API at risk"),
            "public-api section: {out}"
        );
        assert!(out.contains("Service"), "{out}");
        // `evil` depends on svc -> blast radius, with its control char stripped.
        assert!(
            out.contains("evil") && !out.contains('\u{0}'),
            "output sanitized: {out:?}"
        );
    }

    #[test]
    fn affected_tests_selects_only_test_dependents() {
        // prod_caller and test_login both call login; only the test is selected.
        let login = node("login", "login", Some(0));
        let mut test_node = node("t", "test_login", Some(0));
        test_node.source_file = "tests/test_login.py".into();
        let prod = node("prod", "prod_caller", Some(0));
        let gd = GraphData {
            nodes: vec![login, test_node, prod],
            links: vec![edge("t", "login", "calls"), edge("prod", "login", "calls")],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None);
        let out = call_tool(&mut s, "affected_tests", json!({"files": ["login.py"]}));
        assert!(out.contains("test_login"), "test selected: {out}");
        assert!(!out.contains("prod_caller"), "prod caller excluded: {out}");
    }

    #[test]
    fn predict_edit_classifies_by_kind() {
        // auth calls login_user, so deleting login_user breaks AuthService.
        let mut s = server();
        let del = call_tool(
            &mut s,
            "predict_edit",
            json!({"symbol": "login_user", "kind": "delete"}),
        );
        assert!(
            del.contains("Will break"),
            "delete breaks dependents: {del}"
        );
        assert!(del.contains("AuthService"), "the caller is named: {del}");
        // An unknown kind is reported, not silently accepted.
        let bad = call_tool(
            &mut s,
            "predict_edit",
            json!({"symbol": "login_user", "kind": "frobnicate"}),
        );
        assert!(bad.contains("Unknown edit kind"), "{bad}");
        // An unknown symbol is reported.
        let miss = call_tool(
            &mut s,
            "predict_edit",
            json!({"symbol": "Nope", "kind": "delete"}),
        );
        assert!(miss.contains("No node matches"), "{miss}");
    }

    #[test]
    fn predict_edit_summarizes_large_break_sets() {
        // Five callers of `hub`; deleting it breaks all five. Like its siblings
        // (affected/predict_impact), predict_edit must cap the list and roll up
        // a "+N more" note unless verbose=true.
        let mut nodes = vec![node("hub", "hub_fn", Some(0))];
        let mut links = Vec::new();
        for i in 0..5 {
            let id = format!("c{i}");
            nodes.push(node(&id, &format!("Caller{i}"), Some(0)));
            links.push(edge(&id, "hub", "calls"));
        }
        let gd = GraphData {
            nodes,
            links,
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None);
        // Capped at 2 with a "+3 more" rollup.
        let capped = call_tool(
            &mut s,
            "predict_edit",
            json!({"symbol": "hub_fn", "kind": "delete", "limit": 2}),
        );
        assert!(capped.contains("Will break (5)"), "total shown: {capped}");
        assert!(capped.contains("+3 more"), "remaining rolled up: {capped}");
        // A by-depth rollup of the break set (all five are 1 hop away).
        assert!(
            capped.contains("by depth") && capped.contains("1h: 5"),
            "by-depth rollup present: {capped}"
        );
        // verbose shows everything, no truncation note.
        let full = call_tool(
            &mut s,
            "predict_edit",
            json!({"symbol": "hub_fn", "kind": "delete", "verbose": true}),
        );
        assert!(full.contains("Caller4"), "all dependents shown: {full}");
        assert!(!full.contains("more"), "no truncation note: {full}");
    }

    #[test]
    fn predict_impact_clamps_depth_and_handles_no_changes() {
        let mut s = server();
        // depth 0 is clamped to 1 (still returns the direct dependent).
        let out = call_tool(
            &mut s,
            "predict_impact",
            json!({"files": ["login.py"], "depth": 0}),
        );
        assert!(out.contains("AuthService"), "depth clamped to >=1: {out}");
        // A file with no graph nodes yields an empty forecast, not an error.
        let none = call_tool(&mut s, "predict_impact", json!({"files": ["nope.py"]}));
        assert!(none.contains("0 changed node(s)"), "{none}");
    }

    #[test]
    fn initialize_and_tools_list() {
        let mut s = server();
        let init = s
            .handle_request(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
            .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "synaptic");
        assert_eq!(
            init["result"]["serverInfo"]["description"],
            "Read-only code knowledge graph: query, impact, and structural search."
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-11-25");

        let tl = s
            .handle_request(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .unwrap();
        let names: Vec<&str> = tl["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), 29);
        for expected in [
            "query_graph",
            "get_node",
            "get_source",
            "search_text",
            "dynamic_hazards",
            "get_neighbors",
            "get_community",
            "god_nodes",
            "graph_stats",
            "list_repos",
            "repo_stats",
            "shortest_path",
            "affected",
            "find_callers",
            "find_callees",
            "find_references",
            "list_prs",
            "get_pr_impact",
            "triage_prs",
            "working_changes_impact",
            "structural_search",
            "describe_node",
            "time_travel_diff",
            "plan_rename",
            "predict_impact",
            "affected_tests",
            "predict_edit",
            "audit_sql",
            "advise_sql",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
    }

    #[test]
    fn structural_search_tool_returns_rows() {
        let mut s = server();
        let out = call_tool(
            &mut s,
            "structural_search",
            json!({"query": "MATCH (n) RETURN n", "limit": 5}),
        );
        // The default server() graph has nodes; a bare match returns some.
        assert!(
            out.contains("result(s)") || out.contains("0 results"),
            "search output: {out}"
        );
        // A plan_rename on a missing symbol reports an error string, never panics.
        let pr = call_tool(
            &mut s,
            "plan_rename",
            json!({"name": "DefinitelyMissingSymbol", "to": "X"}),
        );
        assert!(
            pr.contains("rename error") || pr.contains("not found"),
            "{pr}"
        );
    }

    #[test]
    fn notifications_get_no_response() {
        let mut s = server();
        let n = s.handle_request(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        assert!(n.is_none(), "a notification (no id) must not get a reply");
    }

    #[test]
    fn unknown_method_is_jsonrpc_error() {
        let mut s = server();
        let r = s
            .handle_request(&json!({"jsonrpc":"2.0","id":9,"method":"does/not/exist"}))
            .unwrap();
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn tools_return_expected_text() {
        let mut s = server();
        assert!(call_tool(&mut s, "graph_stats", json!({})).contains("3 nodes"));
        assert!(call_tool(&mut s, "god_nodes", json!({"top_n": 3})).contains("God nodes:"));
        assert!(
            call_tool(&mut s, "get_node", json!({"label": "AuthService"})).contains("ID: auth")
        );
        let neigh = call_tool(&mut s, "get_neighbors", json!({"label": "AuthService"}));
        assert!(neigh.contains("login_user") && neigh.contains("[calls]"));
        assert!(
            call_tool(&mut s, "get_community", json!({"community_id": 0})).contains("Community 0")
        );
        let path = call_tool(
            &mut s,
            "shortest_path",
            json!({"source": "login_user", "target": "Database"}),
        );
        assert!(path.contains("Shortest path"), "{path}");
        let q = call_tool(
            &mut s,
            "query_graph",
            json!({"question": "authentication", "mode": "dfs"}),
        );
        assert!(q.contains("Traversal: dfs"), "{q}");
    }

    #[test]
    fn god_nodes_page_is_capped() {
        // A huge top_n must not trigger a reverse-impact walk per node across the
        // whole graph: the page is capped (page further with offset).
        let nodes: Vec<_> = (0..250)
            .map(|i| node(&format!("n{i}"), &format!("Fn{i}"), Some(0)))
            .collect();
        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes,
            links: vec![],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let mut s = Server::from_graph_data(gd, None);
        let out = call_tool(&mut s, "god_nodes", json!({"top_n": 10000}));
        let count = out.matches("test(s)").count();
        assert_eq!(count, 200, "page should cap at 200 rows, got {count}");
    }

    #[test]
    fn god_nodes_annotate_test_coverage() {
        // `hub` is exercised by a test (tests/ path) and a plain caller; `orphan`
        // is a hub with only non-test callers. god_nodes must surface the count so
        // an untested hub (the prime risk) is flagged without a second tool call.
        let mut nodes = vec![
            node("hub", "hub_fn", Some(0)),
            node("orphan", "orphan_fn", Some(0)),
        ];
        // Test caller of hub (path makes it a test node).
        let mut t = node("t1", "test_hub", Some(0));
        t.source_file = "tests/test_hub.py".into();
        nodes.push(t);
        nodes.push(node("c1", "caller_one", Some(0)));
        nodes.push(node("c2", "caller_two", Some(0)));
        nodes.push(node("c3", "caller_three", Some(0)));
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes,
            links: vec![
                edge("t1", "hub", "calls"),
                edge("c1", "hub", "calls"),
                edge("c2", "orphan", "calls"),
                edge("c3", "orphan", "calls"),
            ],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let mut s = Server::from_graph_data(gd, None);
        let out = call_tool(&mut s, "god_nodes", json!({"top_n": 10}));
        assert!(out.contains("hub_fn"), "{out}");
        // hub has one test; orphan has none.
        assert!(
            out.contains("1 test"),
            "hub should show a test count: {out}"
        );
        assert!(
            out.contains("0 test"),
            "untested hub must be flagged with 0 tests: {out}"
        );
        // Structured JSON carries the same signal.
        let js = s
            .dispatch_tool(&json!({"name":"god_nodes","arguments":{"top_n":10}}))
            .unwrap();
        let arr = js["structuredContent"]["god_nodes"].as_array().unwrap();
        let orphan = arr
            .iter()
            .find(|g| g["label"] == "orphan_fn")
            .unwrap_or_else(|| panic!("{js}"));
        assert_eq!(orphan["test_count"], 0);
    }

    #[test]
    fn ambiguous_resolution_lists_file_and_degree_inline() {
        // Two `announce()` nodes in different files with different degrees. The
        // ambiguity message must carry each candidate's file + degree so an agent
        // can pick without a second get_node call.
        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                node("hub", "announce()", Some(0)),
                node("leaf", "announce()", Some(0)),
                node("x", "X", Some(0)),
            ],
            links: vec![edge("hub", "leaf", "calls"), edge("hub", "x", "uses")],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let mut s = Server::from_graph_data(gd, None);
        let out = call_tool(&mut s, "get_node", json!({"label": "announce"}));
        assert!(out.contains("ambiguous"), "{out}");
        // File of at least one candidate and its degree appear inline.
        assert!(out.contains("hub.py"), "candidate file missing: {out}");
        assert!(out.contains("degree"), "candidate degree missing: {out}");
    }

    #[test]
    fn get_neighbors_empty_filter_names_available_relations() {
        let mut s = server();
        // AuthService has `calls` and `uses` edges, but nothing matches `contains`.
        // The result must distinguish "0 matching edges" from "no such node" by
        // naming the relations this node DOES have.
        let out = call_tool(
            &mut s,
            "get_neighbors",
            json!({"label": "AuthService", "relation_filter": "contains"}),
        );
        assert!(out.contains("none"), "{out}");
        assert!(
            out.contains("calls") && out.contains("uses"),
            "should list available relations, got: {out}"
        );
    }

    #[test]
    fn get_source_returns_lines_under_jail() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/auth.py"),
            "def login_user(u):\n    return check(u)\n\n\ndef check(u):\n    return True\n",
        )
        .unwrap();

        let mut n = node("login", "login_user", Some(0));
        n.source_file = "src/auth.py".into();
        n.source_location = Some("L1".into());
        let gd = GraphData {
            nodes: vec![n],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None).with_source_root(root.to_path_buf());

        let out = call_tool(
            &mut s,
            "get_source",
            json!({"label": "login_user", "context_lines": 2}),
        );
        assert!(
            out.contains("def login_user(u):"),
            "should include the body: {out}"
        );
        assert!(
            out.contains("src/auth.py:L1"),
            "header names the file+line: {out}"
        );
    }

    #[test]
    fn get_source_without_root_is_graceful() {
        let mut s = server(); // no source root
        let out = call_tool(&mut s, "get_source", json!({"label": "AuthService"}));
        assert!(out.contains("Source not available"), "{out}");
    }

    #[test]
    fn get_source_reads_federated_node_from_its_repo_root() {
        // A federated node: `repo` set and `source_file` prefixed with the tag,
        // its file living under a sibling repo root outside the single source root.
        let dir = tempfile::tempdir().unwrap();
        let billing_root = dir.path().join("billing");
        std::fs::create_dir_all(billing_root.join("src")).unwrap();
        std::fs::write(billing_root.join("src/pay.py"), "def charge():\n    pass\n").unwrap();
        let other_root = dir.path().join("other");
        std::fs::create_dir_all(&other_root).unwrap();

        let mut n = node("billing::charge", "charge", Some(0));
        n.repo = Some("billing".into());
        n.source_file = "billing/src/pay.py".into();
        n.source_location = Some("L1".into());
        let gd = GraphData {
            nodes: vec![n],
            ..Default::default()
        };

        // With the repo root registered, the federated file resolves.
        let mut roots = HashMap::new();
        roots.insert("billing".to_string(), billing_root.clone());
        let mut s = Server::from_graph_data(gd.clone(), None)
            .with_source_root(other_root.clone())
            .with_repo_roots(roots);
        let out = call_tool(&mut s, "get_source", json!({"label": "charge"}));
        assert!(
            out.contains("def charge():"),
            "reads federated source: {out}"
        );

        // Without the repo root, it falls back to the single source root, misses,
        // and the message names the root it actually tried.
        let mut s2 = Server::from_graph_data(gd, None).with_source_root(other_root.clone());
        let out2 = call_tool(&mut s2, "get_source", json!({"label": "charge"}));
        assert!(out2.contains("not found under source-root"), "{out2}");
        assert!(
            out2.contains(&other_root.display().to_string()),
            "message names the configured root: {out2}"
        );
    }

    /// Build a single-repo server over `dir` with one node spanning `lines` in
    /// `rel`. The node is the enclosing symbol every in-range hit attributes to.
    fn text_search_server(
        dir: &std::path::Path,
        rel: &str,
        contents: &str,
        node_label: &str,
        span: (u32, u32),
    ) -> Server {
        let full = dir.join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, contents).unwrap();
        let mut n = node("sym", node_label, Some(0));
        n.set_kind(NodeKind::Method);
        n.source_file = rel.replace('\\', "/");
        n.source_location = Some(format!("L{}", span.0));
        n.set_span(synaptic_core::Span {
            start_line: span.0,
            start_col: 1,
            end_line: span.1,
            end_col: 1,
        });
        let gd = GraphData {
            nodes: vec![n],
            ..Default::default()
        };
        Server::from_graph_data(gd, None).with_source_root(dir.to_path_buf())
    }

    #[test]
    fn search_text_finds_content_and_attributes_enclosing_node() {
        let dir = tempfile::tempdir().unwrap();
        let src = "function getStatus() {\n  const ALLOW_LIST = ['a'];\n  // TODO: tighten this\n  return ALLOW_LIST;\n}\n";
        let mut s = text_search_server(dir.path(), "src/fw.js", src, "getStatus", (1, 5));

        let out = call_tool(&mut s, "search_text", json!({"pattern": "ALLOW_LIST"}));
        assert!(out.contains("src/fw.js"), "names the file: {out}");
        assert!(
            out.contains("getStatus"),
            "attributes the enclosing node: {out}"
        );
        assert!(out.contains("ALLOW_LIST"), "shows the matched line: {out}");
    }

    #[test]
    fn search_text_structured_mirror_carries_node_and_location() {
        let dir = tempfile::tempdir().unwrap();
        let src = "function getStatus() {\n  const ALLOW_LIST = ['a'];\n}\n";
        let mut s = text_search_server(dir.path(), "src/fw.js", src, "getStatus", (1, 3));

        let resp = call_tool_full(&mut s, "search_text", json!({"pattern": "ALLOW_LIST"}));
        let sc = &resp["result"]["structuredContent"];
        assert_eq!(sc["total"], json!(1), "{sc}");
        let hit = &sc["hits"][0];
        assert_eq!(hit["file"], "src/fw.js");
        assert_eq!(hit["line"], json!(2), "matched on line 2: {hit}");
        assert_eq!(hit["node"]["label"], "getStatus");
        assert_eq!(hit["node"]["kind"], "method");
    }

    #[test]
    fn search_text_regex_vs_literal() {
        let dir = tempfile::tempdir().unwrap();
        // `a.b` literal should match only "a.b", not "axb"; as a regex `.` is any.
        let src = "x = a.b\ny = axb\n";
        let mut s = text_search_server(dir.path(), "src/m.py", src, "fn", (1, 2));

        let lit = call_tool_full(
            &mut s,
            "search_text",
            json!({"pattern": "a.b", "literal": true}),
        );
        assert_eq!(
            lit["result"]["structuredContent"]["total"],
            json!(1),
            "literal a.b matches one line: {}",
            lit["result"]["structuredContent"]
        );

        let rx = call_tool_full(&mut s, "search_text", json!({"pattern": "a.b"}));
        assert_eq!(
            rx["result"]["structuredContent"]["total"],
            json!(2),
            "regex a.b matches both lines"
        );
    }

    #[test]
    fn search_text_case_insensitive_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = text_search_server(dir.path(), "src/m.py", "TODO fix\n", "fn", (1, 1));
        let out = call_tool_full(&mut s, "search_text", json!({"pattern": "todo"}));
        assert_eq!(out["result"]["structuredContent"]["total"], json!(1));
        let sens = call_tool_full(
            &mut s,
            "search_text",
            json!({"pattern": "todo", "case_sensitive": true}),
        );
        assert_eq!(sens["result"]["structuredContent"]["total"], json!(0));
    }

    #[test]
    fn search_text_repo_filter_scopes_to_one_member() {
        let dir = tempfile::tempdir().unwrap();
        let billing = dir.path().join("billing");
        let web = dir.path().join("web");
        std::fs::create_dir_all(billing.join("src")).unwrap();
        std::fs::create_dir_all(web.join("src")).unwrap();
        std::fs::write(billing.join("src/pay.py"), "SECRET = 1\n").unwrap();
        std::fs::write(web.join("src/app.js"), "const SECRET = 2\n").unwrap();

        let mut bn = node("b", "pay", Some(0));
        bn.repo = Some("billing".into());
        bn.source_file = "billing/src/pay.py".into();
        bn.set_span(synaptic_core::Span {
            start_line: 1,
            start_col: 1,
            end_line: 1,
            end_col: 1,
        });
        let mut wn = node("w", "app", Some(0));
        wn.repo = Some("web".into());
        wn.source_file = "web/src/app.js".into();
        wn.set_span(synaptic_core::Span {
            start_line: 1,
            start_col: 1,
            end_line: 1,
            end_col: 1,
        });
        let gd = GraphData {
            nodes: vec![bn, wn],
            ..Default::default()
        };
        let mut roots = HashMap::new();
        roots.insert("billing".to_string(), billing.clone());
        roots.insert("web".to_string(), web.clone());
        let mut s = Server::from_graph_data(gd, None)
            .with_source_root(dir.path().to_path_buf())
            .with_repo_roots(roots);

        let all = call_tool_full(&mut s, "search_text", json!({"pattern": "SECRET"}));
        assert_eq!(
            all["result"]["structuredContent"]["total"],
            json!(2),
            "both members hit without a filter"
        );

        let scoped = call_tool_full(
            &mut s,
            "search_text",
            json!({"pattern": "SECRET", "repo": "billing"}),
        );
        let sc = &scoped["result"]["structuredContent"];
        assert_eq!(
            sc["total"],
            json!(1),
            "repo filter scopes to one member: {sc}"
        );
        assert_eq!(sc["hits"][0]["file"], "billing/src/pay.py");
        assert_eq!(sc["hits"][0]["repo"], "billing");
    }

    #[test]
    fn search_text_path_glob_filters_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.js"), "MARK\n").unwrap();
        std::fs::write(dir.path().join("src/b.py"), "MARK\n").unwrap();
        let gd = GraphData {
            nodes: vec![],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None).with_source_root(dir.path().to_path_buf());

        let out = call_tool_full(
            &mut s,
            "search_text",
            json!({"pattern": "MARK", "path_glob": "**/*.js"}),
        );
        let sc = &out["result"]["structuredContent"];
        assert_eq!(sc["total"], json!(1), "glob keeps only the .js file: {sc}");
        assert_eq!(sc["hits"][0]["file"], "src/a.js");
    }

    #[test]
    fn search_text_caps_results_and_flags_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let many = "HIT\nHIT\nHIT\nHIT\nHIT\n";
        let mut s = text_search_server(dir.path(), "src/m.py", many, "fn", (1, 5));
        let out = call_tool_full(
            &mut s,
            "search_text",
            json!({"pattern": "HIT", "max_results": 2}),
        );
        let sc = &out["result"]["structuredContent"];
        assert_eq!(sc["total"], json!(2), "capped to max_results: {sc}");
        assert_eq!(sc["truncated"], json!(true), "flags more were available");
    }

    #[test]
    fn search_text_without_source_root_is_graceful() {
        let mut s = server();
        let out = call_tool(&mut s, "search_text", json!({"pattern": "anything"}));
        assert!(
            out.contains("source root"),
            "explains the missing root: {out}"
        );
    }

    #[test]
    fn search_text_excludes_synaptic_output_dir() {
        // Synaptic's own generated output must never surface as content hits, even
        // when it is not gitignored: the canonical `synaptic-out/` (matched by
        // name, with its exports/backups), a custom --out dir (matched by its
        // graph.json + .manifest.json marker pair), and a cache-only / predecessor
        // dir matched by its `cache/ast/` AST cache (no graph.json beside it). A
        // stray source file merely *named* graph.json (no sibling manifest) stays
        // searchable.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/app.js"), "const TOKEN = 1\n").unwrap();
        // Canonical output dir, matched by name.
        std::fs::create_dir_all(dir.path().join("synaptic-out")).unwrap();
        std::fs::write(
            dir.path().join("synaptic-out/graph.json"),
            "{\"TOKEN\":1}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("synaptic-out/graph.json.bak-007"),
            "TOKEN backup\n",
        )
        .unwrap();
        // A predecessor / cache-only output dir: differently named, NO graph.json,
        // just the hash-keyed AST cache full of extracted source text. Matched by
        // the `cache/ast/` signature alone.
        std::fs::create_dir_all(dir.path().join("codegraph-out/cache/ast/v0.1.0")).unwrap();
        std::fs::write(
            dir.path()
                .join("codegraph-out/cache/ast/v0.1.0/deadbeef.json"),
            "{\"label\":\"TOKEN from cached ast\"}\n",
        )
        .unwrap();
        // Custom --out dir, matched by the generated-file marker pair.
        std::fs::create_dir_all(dir.path().join("nested/build-out")).unwrap();
        std::fs::write(
            dir.path().join("nested/build-out/graph.json"),
            "TOKEN out\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("nested/build-out/.manifest.json"), "{}\n").unwrap();
        // A genuine source/fixture file that happens to be named graph.json (no
        // manifest beside it): this is the user's content and must be searched.
        std::fs::create_dir_all(dir.path().join("fixtures")).unwrap();
        std::fs::write(dir.path().join("fixtures/graph.json"), "TOKEN sample\n").unwrap();

        let gd = GraphData {
            nodes: vec![],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None).with_source_root(dir.path().to_path_buf());

        let out = call_tool_full(&mut s, "search_text", json!({"pattern": "TOKEN"}));
        let sc = &out["result"]["structuredContent"];
        let files: std::collections::BTreeSet<&str> = sc["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|h| h["file"].as_str())
            .collect();
        assert_eq!(
            files,
            ["fixtures/graph.json", "src/app.js"].into_iter().collect(),
            "output dirs pruned, real source (incl. a stray graph.json) kept: {sc}"
        );
    }

    #[test]
    fn search_text_smart_case_uppercase_pattern_is_case_sensitive() {
        // Smart case: an uppercase-bearing pattern defaults to case-sensitive (so
        // `TODO` does not drag in a lowercase "todos"); case_sensitive=false
        // restores insensitive matching, case_sensitive=true is unchanged.
        let dir = tempfile::tempdir().unwrap();
        let src = "// TODO real\nconst todos = []\n";
        let mut s = text_search_server(dir.path(), "src/m.js", src, "fn", (1, 2));

        let smart = call_tool_full(&mut s, "search_text", json!({"pattern": "TODO"}));
        assert_eq!(
            smart["result"]["structuredContent"]["total"],
            json!(1),
            "uppercase pattern is case-sensitive by default (TODO only): {}",
            smart["result"]["structuredContent"]
        );

        let forced = call_tool_full(
            &mut s,
            "search_text",
            json!({"pattern": "TODO", "case_sensitive": false}),
        );
        assert_eq!(
            forced["result"]["structuredContent"]["total"],
            json!(2),
            "case_sensitive=false forces insensitive: TODO + todos"
        );
    }

    #[test]
    fn search_text_repo_filter_works_over_single_parent_root() {
        // A multi-repo graph served with one parent --source-root and NO registered
        // repo_roots (no global-manifest): the repo filter still scopes to a member
        // whose files live under <root>/<tag>, and every hit carries its repo from
        // the enclosing node.
        let dir = tempfile::tempdir().unwrap();
        let billing = dir.path().join("billing");
        let web = dir.path().join("web");
        std::fs::create_dir_all(billing.join("src")).unwrap();
        std::fs::create_dir_all(web.join("src")).unwrap();
        std::fs::write(billing.join("src/pay.py"), "SECRET = 1\n").unwrap();
        std::fs::write(web.join("src/app.js"), "const SECRET = 2\n").unwrap();

        let mut bn = node("b", "pay", Some(0));
        bn.repo = Some("billing".into());
        bn.source_file = "billing/src/pay.py".into();
        bn.set_span(synaptic_core::Span {
            start_line: 1,
            start_col: 1,
            end_line: 1,
            end_col: 1,
        });
        let mut wn = node("w", "app", Some(0));
        wn.repo = Some("web".into());
        wn.source_file = "web/src/app.js".into();
        wn.set_span(synaptic_core::Span {
            start_line: 1,
            start_col: 1,
            end_line: 1,
            end_col: 1,
        });
        let gd = GraphData {
            nodes: vec![bn, wn],
            ..Default::default()
        };
        // No with_repo_roots: only the single parent source root is registered.
        let mut s = Server::from_graph_data(gd, None).with_source_root(dir.path().to_path_buf());

        let all = call_tool_full(&mut s, "search_text", json!({"pattern": "SECRET"}));
        let sc = &all["result"]["structuredContent"];
        assert_eq!(sc["total"], json!(2), "both members hit: {sc}");
        let repos: std::collections::BTreeSet<&str> = sc["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|h| h["repo"].as_str())
            .collect();
        assert!(
            repos.contains("billing") && repos.contains("web"),
            "each hit carries its repo from the enclosing node: {sc}"
        );

        let scoped = call_tool_full(
            &mut s,
            "search_text",
            json!({"pattern": "SECRET", "repo": "billing"}),
        );
        let sc = &scoped["result"]["structuredContent"];
        assert_eq!(
            sc["total"],
            json!(1),
            "repo filter scopes without registered roots: {sc}"
        );
        assert_eq!(sc["hits"][0]["file"], "billing/src/pay.py");
        assert_eq!(sc["hits"][0]["repo"], "billing");
    }

    #[test]
    fn search_text_unenclosed_hit_carries_repo_from_path_prefix() {
        // A hit outside any node span still reports its repo from the graph-path's
        // member prefix, not null.
        let dir = tempfile::tempdir().unwrap();
        let billing = dir.path().join("billing");
        std::fs::create_dir_all(billing.join("src")).unwrap();
        std::fs::write(
            billing.join("src/pay.py"),
            "# MARKER here\n\ndef pay():\n    pass\n",
        )
        .unwrap();
        let mut bn = node("b", "pay", Some(0));
        bn.repo = Some("billing".into());
        bn.source_file = "billing/src/pay.py".into();
        bn.set_span(synaptic_core::Span {
            start_line: 3,
            start_col: 1,
            end_line: 4,
            end_col: 9,
        });
        let gd = GraphData {
            nodes: vec![bn],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None).with_source_root(dir.path().to_path_buf());

        let out = call_tool_full(&mut s, "search_text", json!({"pattern": "MARKER"}));
        let hit = &out["result"]["structuredContent"]["hits"][0];
        assert!(hit["node"].is_null(), "hit is outside any node span: {hit}");
        assert_eq!(
            hit["repo"], "billing",
            "repo derived from the path prefix: {hit}"
        );
    }

    // ---- Gap 1: reading logic (get_source file+range, show_sites call lines) ----

    #[test]
    fn get_source_reads_an_arbitrary_file_range() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/conf.js"),
            "line1\nline2\nline3\nline4\n",
        )
        .unwrap();
        let gd = GraphData {
            nodes: vec![],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None).with_source_root(dir.path().to_path_buf());

        let out = call_tool(
            &mut s,
            "get_source",
            json!({"file": "src/conf.js", "lines": "2-3"}),
        );
        assert!(
            out.contains("line2") && out.contains("line3"),
            "range body: {out}"
        );
        assert!(
            !out.contains("line1") && !out.contains("line4"),
            "range is bounded: {out}"
        );
        assert!(
            out.contains("src/conf.js:L2-L3"),
            "header names range: {out}"
        );
    }

    #[test]
    fn get_source_file_range_refuses_paths_outside_the_jail() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(dir.path().join("secret.txt"), "top secret\n").unwrap();
        let gd = GraphData {
            nodes: vec![],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None).with_source_root(root.clone());
        let out = call_tool(
            &mut s,
            "get_source",
            json!({"file": "../secret.txt", "lines": "1"}),
        );
        assert!(
            !out.contains("top secret"),
            "must not escape the jail: {out}"
        );
        assert!(
            out.to_lowercase().contains("outside") || out.to_lowercase().contains("not found"),
            "refuses with an explanation: {out}"
        );
    }

    #[test]
    fn get_source_file_range_reads_a_federated_member() {
        let dir = tempfile::tempdir().unwrap();
        let billing = dir.path().join("billing");
        std::fs::create_dir_all(billing.join("src")).unwrap();
        std::fs::write(billing.join("src/pay.py"), "def charge():\n    pass\n").unwrap();
        let gd = GraphData {
            nodes: vec![],
            ..Default::default()
        };
        let mut roots = HashMap::new();
        roots.insert("billing".to_string(), billing.clone());
        let mut s = Server::from_graph_data(gd, None)
            .with_source_root(dir.path().to_path_buf())
            .with_repo_roots(roots);
        let out = call_tool(
            &mut s,
            "get_source",
            json!({"file": "billing/src/pay.py", "lines": "1"}),
        );
        assert!(out.contains("def charge"), "reads via tag/ path: {out}");
    }

    /// Build a 2-node graph where `a` calls `b` at `src/a.js:2`, where line 2 is
    /// the actual call. Returns a server with the source root set.
    fn call_site_server() -> (tempfile::TempDir, Server) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.js"), "function a(){\n  b();\n}\n").unwrap();
        let mut a = node("a", "a", Some(0));
        a.source_file = "src/a.js".into();
        a.set_span(synaptic_core::Span {
            start_line: 1,
            start_col: 1,
            end_line: 3,
            end_col: 1,
        });
        let mut bnode = node("b", "b", Some(0));
        bnode.source_file = "src/b.js".into();
        let mut e = edge("a", "b", "calls");
        e.source_file = "src/a.js".into();
        e.source_location = Some("L2".into());
        let gd = GraphData {
            directed: true,
            nodes: vec![a, bnode],
            links: vec![e],
            ..Default::default()
        };
        let s = Server::from_graph_data(gd, None).with_source_root(dir.path().to_path_buf());
        (dir, s)
    }

    #[test]
    fn find_callees_show_sites_shows_the_call_line() {
        let (_d, mut s) = call_site_server();
        let out = call_tool(
            &mut s,
            "find_callees",
            json!({"label": "a", "show_sites": true}),
        );
        assert!(out.contains("b [calls]"), "lists the callee: {out}");
        assert!(out.contains("src/a.js:2"), "names the call site: {out}");
        assert!(out.contains("b();"), "shows the actual call line: {out}");
    }

    #[test]
    fn find_callers_show_sites_shows_the_call_line() {
        let (_d, mut s) = call_site_server();
        let out = call_tool(
            &mut s,
            "find_callers",
            json!({"label": "b", "show_sites": true}),
        );
        assert!(out.contains("a [calls]"), "lists the caller: {out}");
        assert!(out.contains("src/a.js:2"), "names the call site: {out}");
        assert!(out.contains("b();"), "shows the actual call line: {out}");
    }

    #[test]
    fn show_sites_is_off_by_default() {
        let (_d, mut s) = call_site_server();
        let out = call_tool(&mut s, "find_callees", json!({"label": "a"}));
        assert!(out.contains("b [calls]"), "still lists the callee: {out}");
        assert!(
            !out.contains("src/a.js:2"),
            "no call site without show_sites: {out}"
        );
    }

    #[test]
    fn get_neighbors_show_sites_shows_the_edge_line() {
        let (_d, mut s) = call_site_server();
        let out = call_tool(
            &mut s,
            "get_neighbors",
            json!({"label": "a", "show_sites": true}),
        );
        assert!(out.contains("b [calls]"), "lists the neighbor: {out}");
        assert!(out.contains("src/a.js:2"), "names the edge site: {out}");
        assert!(out.contains("b();"), "shows the actual line: {out}");
    }

    #[test]
    fn get_community_paginates() {
        // 6 members in community 0.
        let nodes: Vec<_> = (0..6)
            .map(|i| {
                let id: &'static str = Box::leak(format!("n{i}").into_boxed_str());
                let lbl: &'static str = Box::leak(format!("N{i}").into_boxed_str());
                node(id, lbl, Some(0))
            })
            .collect();
        let gd = GraphData {
            nodes,
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None);
        let out = call_tool(
            &mut s,
            "get_community",
            json!({"community_id":0,"offset":2,"limit":2}),
        );
        assert!(out.contains("showing 2 of 6"), "footer: {out}");
        // Offset past the end yields an empty page, not a panic.
        let past = call_tool(
            &mut s,
            "get_community",
            json!({"community_id":0,"offset":99,"limit":2}),
        );
        assert!(past.contains("showing 0 of 6"), "{past}");
    }

    #[test]
    fn god_nodes_offset_pages_and_numbers_absolutely() {
        let mut s = server(); // AuthService(2), Database(1), login_user(1); no tests
        let top = call_tool(&mut s, "god_nodes", json!({"top_n": 1}));
        assert!(top.starts_with("God nodes:"), "{top}");
        assert!(
            top.contains("\n  1. AuthService - 2 connections, 0 test(s)"),
            "{top}"
        );
        // offset 1 skips the top hub and numbers from its absolute rank.
        let paged = call_tool(&mut s, "god_nodes", json!({"top_n": 1, "offset": 1}));
        assert!(
            paged.contains("\n  2. Database - 1 connections, 0 test(s)"),
            "{paged}"
        );
    }

    #[test]
    fn logging_set_level_acknowledged() {
        let mut s = server();
        let r = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":1,"method":"logging/setLevel","params":{"level":"info"}
            }))
            .unwrap();
        assert!(r.get("error").is_none(), "setLevel should succeed: {r}");
        assert_eq!(r["result"], json!({}));

        // The capability is advertised so a host knows it can set a level.
        let init = s
            .handle_request(&json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}))
            .unwrap();
        assert!(init["result"]["capabilities"]["logging"].is_object());
    }

    #[test]
    fn completion_completes_node_labels() {
        let mut s = server(); // AuthService, login_user, Database
        let r = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":1,"method":"completion/complete",
                "params":{ "ref": {"type":"ref/resource","uri":"synaptic://node/{label}"},
                           "argument": {"name":"label","value":"Auth"} }
            }))
            .unwrap();
        let values: Vec<&str> = r["result"]["completion"]["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(values.contains(&"AuthService"), "{values:?}");
        assert!(!values.contains(&"Database"), "prefix filtered: {values:?}");
        assert_eq!(r["result"]["completion"]["hasMore"], false);
    }

    #[test]
    fn completion_sees_past_leading_punctuation_on_methods() {
        // Method nodes are labeled ".name()"; a bare-name prefix must still match.
        let gd = GraphData {
            nodes: vec![node(".tool_get_node()", ".tool_get_node()", Some(0))],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None);
        let r = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":1,"method":"completion/complete",
                "params":{"argument":{"name":"label","value":"tool_get"}}
            }))
            .unwrap();
        let values: Vec<&str> = r["result"]["completion"]["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(values.contains(&".tool_get_node()"), "{values:?}");
    }

    #[test]
    fn subscribe_acked_and_capability_advertised() {
        let mut s = server();
        let init = s
            .handle_request(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
            .unwrap();
        assert_eq!(
            init["result"]["capabilities"]["resources"]["subscribe"],
            true
        );

        let ack = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":2,"method":"resources/subscribe",
                "params":{"uri":"synaptic://stats"}
            }))
            .unwrap();
        assert!(ack.get("error").is_none(), "subscribe should ack: {ack}");
        assert_eq!(ack["result"], json!({}));
    }

    #[test]
    fn resource_templates_listed_and_readable() {
        let mut s = server();
        let tl = s
            .handle_request(&json!({"jsonrpc":"2.0","id":1,"method":"resources/templates/list"}))
            .unwrap();
        let templates = tl["result"]["resourceTemplates"].as_array().unwrap();
        assert!(templates
            .iter()
            .any(|t| t["uriTemplate"] == "synaptic://node/{label}"));

        let read = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":2,"method":"resources/read",
                "params":{"uri":"synaptic://node/AuthService"}
            }))
            .unwrap();
        assert!(read["result"]["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("AuthService"));

        // A static resource still resolves (templates do not shadow it).
        let stats = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":3,"method":"resources/read",
                "params":{"uri":"synaptic://god-nodes"}
            }))
            .unwrap();
        assert!(stats["result"]["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("God nodes"));
    }

    #[test]
    fn prompts_list_and_get() {
        let mut s = server();
        let list = s
            .handle_request(&json!({"jsonrpc":"2.0","id":1,"method":"prompts/list"}))
            .unwrap();
        let names: Vec<&str> = list["result"]["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"onboard"));
        assert!(names.contains(&"explain_subsystem"));

        let got = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":2,"method":"prompts/get",
                "params":{"name":"explain_subsystem","arguments":{"topic":"authentication"}}
            }))
            .unwrap();
        let text = got["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("authentication"), "arg interpolated: {text}");

        // Unknown prompt -> JSON-RPC error.
        let err = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":3,"method":"prompts/get","params":{"name":"nope"}
            }))
            .unwrap();
        assert_eq!(err["error"]["code"], -32602);
    }

    #[test]
    fn graph_stats_returns_structured_content() {
        let mut s = server();
        let resp = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"graph_stats","arguments":{}}
            }))
            .unwrap();
        let sc = &resp["result"]["structuredContent"];
        assert_eq!(sc["nodes"], json!(3));
        assert_eq!(sc["edges"], json!(2));
        // Single-repo graph: no cross-repo coupling reported in text...
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("3 nodes"));
        assert!(
            !text.contains("Cross-repo"),
            "no cross-repo line single-repo"
        );
        // ...but the structured fields are present and zero.
        assert_eq!(sc["cross_repo"], json!(0));
        assert_eq!(sc["cross_language"], json!(0));
    }

    #[test]
    fn graph_stats_reports_cross_repo_and_cross_language() {
        // A federated graph: one cross-repo import link + one cross-repo
        // cross-language (handled_by) link. graph_stats reports both.
        let mut import = edge("a", "b", "imports_from");
        import.cross_repo = true;
        let mut svc = edge("c", "ws", "handled_by");
        svc.cross_repo = true;
        let gd = GraphData {
            nodes: vec![
                node("a", "a.ts", Some(0)),
                node("b", "b.ts", Some(0)),
                node("c", "client.ts", Some(0)),
                node("ws", "ws #connect", Some(0)),
            ],
            links: vec![import, svc],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None);
        let text = call_tool(&mut s, "graph_stats", json!({}));
        assert!(
            text.contains("Cross-repo: 2 edge(s) span repositories (1 cross-language"),
            "{text}"
        );
        let resp = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"graph_stats","arguments":{}}
            }))
            .unwrap();
        let sc = &resp["result"]["structuredContent"];
        assert_eq!(sc["cross_repo"], json!(2));
        assert_eq!(sc["cross_language"], json!(1));
    }

    #[test]
    fn query_graph_structured_nodes_carry_descending_scores() {
        // Each structured node exposes a relevance `score`, and nodes come back
        // sorted by it (so a caller can focus on the top results).
        let mut s = server();
        let resp = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"query_graph","arguments":{"question":"auth login"}}
            }))
            .unwrap();
        let nodes = resp["result"]["structuredContent"]["nodes"]
            .as_array()
            .unwrap();
        assert!(!nodes.is_empty());
        let scores: Vec<f64> = nodes
            .iter()
            .map(|n| n["score"].as_f64().expect("each node has a numeric score"))
            .collect();
        for w in scores.windows(2) {
            assert!(
                w[0] >= w[1],
                "structured nodes must be score-sorted: {scores:?}"
            );
        }
    }

    #[test]
    fn query_graph_structured_respects_context_filter() {
        // structuredContent must apply context_filter just like the text path,
        // or the two diverge. Filter to auth.py; login.py/db.py must drop out.
        let mut s = server();
        let resp = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"query_graph","arguments":{
                    "question":"auth login database","context_filter":["auth.py"]}}
            }))
            .unwrap();
        let files: Vec<String> = resp["result"]["structuredContent"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["source_file"].as_str().unwrap().to_string())
            .collect();
        assert!(
            !files.is_empty(),
            "expected at least the auth node: {files:?}"
        );
        assert!(
            files.iter().all(|f| f.contains("auth.py")),
            "structured nodes must honor context_filter: {files:?}"
        );
    }

    #[test]
    fn structured_tools_declare_output_schema() {
        let tools = tools_list(false);
        for name in [
            "graph_stats",
            "query_graph",
            "affected",
            "god_nodes",
            "predict_impact",
            "affected_tests",
            "get_neighbors",
            "list_repos",
            "search_text",
        ] {
            let t = tools
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == name)
                .unwrap();
            assert!(
                t.get("outputSchema").is_some(),
                "{name} needs an outputSchema"
            );
        }
    }

    #[test]
    fn get_neighbors_and_list_repos_emit_structured_content() {
        let mut s = server();
        let resp = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"get_neighbors","arguments":{"label":"AuthService"}}
            }))
            .unwrap();
        let sc = &resp["result"]["structuredContent"];
        assert_eq!(sc["seed"], "AuthService");
        assert!(
            !sc["neighbors"].as_array().unwrap().is_empty(),
            "neighbors present: {sc}"
        );
        assert!(sc["by_relation"].is_object(), "by_relation tally: {sc}");

        // Single-repo graph: list_repos is still structured, with an empty array.
        let resp2 = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":2,"method":"tools/call",
                "params":{"name":"list_repos","arguments":{}}
            }))
            .unwrap();
        assert!(resp2["result"]["structuredContent"]["repos"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn predict_tools_emit_structured_content() {
        struct GitRunner;
        impl CommandRunner for GitRunner {
            fn run(&self, program: &str, args: &[&str]) -> Option<String> {
                if program == "git" && args.first() == Some(&"diff") {
                    return Some("auth.py\n".to_string());
                }
                None
            }
        }
        let mut a = node("auth", "AuthService", Some(0));
        a.source_file = "auth.py".into();
        let gd = GraphData {
            nodes: vec![a],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None).with_runner(Box::new(GitRunner));

        let resp = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"predict_impact","arguments":{}}
            }))
            .unwrap();
        let sc = &resp["result"]["structuredContent"];
        assert!(sc["summary"].is_string(), "forecast in structured: {sc}");
        assert!(sc.get("blast_radius_total").is_some(), "{sc}");

        let resp2 = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":2,"method":"tools/call",
                "params":{"name":"affected_tests","arguments":{}}
            }))
            .unwrap();
        let sc2 = &resp2["result"]["structuredContent"];
        assert!(sc2["tests"].is_array(), "tests array: {sc2}");
        assert!(sc2["total"].is_number(), "total count: {sc2}");
    }

    #[test]
    fn initialize_echoes_supported_protocol_else_latest() {
        let mut s = server();
        // Client asks for a still-supported legacy version -> echoed back.
        let r = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"protocolVersion":"2025-06-18"}
            }))
            .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");

        // Client asks for the new revision -> echoed back.
        let r = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":2,"method":"initialize",
                "params":{"protocolVersion":"2025-11-25"}
            }))
            .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-11-25");

        // Unknown version -> server returns its latest supported.
        let r = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":3,"method":"initialize",
                "params":{"protocolVersion":"1999-01-01"}
            }))
            .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-11-25");
    }

    #[test]
    fn every_tool_is_annotated_read_only() {
        // The DEFAULT tool surface (no --allow-exec) must be strictly read-only.
        let tools = tools_list(false);
        for t in tools.as_array().unwrap() {
            let name = t["name"].as_str().unwrap();
            let ann = &t["annotations"];
            assert_eq!(
                ann["readOnlyHint"],
                json!(true),
                "tool {name} must be read-only"
            );
            // PR + working-tree tools reach outside the graph (gh/git), and
            // time_travel_diff builds revisions in a worktree -> open world.
            // predict_impact shells out to `git diff` when `files` is omitted.
            let open = matches!(
                name,
                "list_prs"
                    | "get_pr_impact"
                    | "triage_prs"
                    | "working_changes_impact"
                    | "predict_impact"
                    | "affected_tests"
                    | "time_travel_diff"
            );
            assert_eq!(
                ann["openWorldHint"],
                json!(open),
                "tool {name} openWorldHint"
            );
        }
    }

    #[test]
    fn speculate_tool_is_gated_behind_allow_exec() {
        // Hidden on the default surface; present (and honestly annotated as a
        // non-read-only, open-world tool) only when the operator opted in.
        let default = tools_list(false);
        assert!(
            !default
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "speculate"),
            "speculate must be absent from the default read-only surface"
        );
        let exec = tools_list(true);
        let spec = exec
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "speculate")
            .expect("speculate present with --allow-exec");
        assert_eq!(
            spec["annotations"]["readOnlyHint"],
            json!(false),
            "speculate is not read-only"
        );
        assert_eq!(
            spec["annotations"]["openWorldHint"],
            json!(true),
            "speculate reaches the environment"
        );
        assert_eq!(
            default.as_array().unwrap().len() + 1,
            exec.as_array().unwrap().len(),
            "--allow-exec adds exactly one tool"
        );
    }

    #[test]
    fn speculate_call_is_refused_without_allow_exec() {
        // A default server must refuse to run commands even if asked directly.
        let mut s = server();
        let r = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"speculate","arguments":{"files":["src/x.rs"]}}
            }))
            .unwrap();
        let result = &r["result"];
        assert_eq!(result["isError"], json!(true), "refused: {result}");
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("--allow-exec"),
            "explains how to enable: {text}"
        );
    }

    #[test]
    fn speculate_runs_the_at_risk_tests_with_allow_exec() {
        use std::process::Command;
        let git = |dir: &std::path::Path, args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@e")
                .output()
                .expect("git")
        };
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        if !git(root, &["init", "-q"]).status.success() {
            return; // git unavailable
        }
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("src/helper.py"), b"def helper():\n    return 1\n").unwrap();
        std::fs::write(root.join("tests/test_helper.py"), b"# exercises helper\n").unwrap();
        git(root, &["add", "-A"]);
        assert!(git(root, &["commit", "-q", "-m", "init", "--no-gpg-sign"])
            .status
            .success());
        // An uncommitted edit is the change to speculate.
        std::fs::write(root.join("src/helper.py"), b"def helper():\n    return 2\n").unwrap();

        // tests/test_helper (a test path) calls src/helper, so editing helper puts
        // the test in the at-risk set the sandbox runs.
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                node("src/helper", "helper", Some(0)),
                node("tests/test_helper", "test_helper", Some(0)),
            ],
            links: vec![edge("tests/test_helper", "src/helper", "calls")],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let mut s = Server::from_graph_data(gd, None)
            .with_source_root(root.to_path_buf())
            .with_allow_exec(true);

        let r = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"speculate","arguments":{
                    "files":["src/helper.py"],
                    "base":"HEAD",
                    "test_cmd":"git ls-files --error-unmatch {files}"
                }}
            }))
            .unwrap();
        let result = &r["result"];
        assert_eq!(result["isError"], json!(false), "ran: {result}");
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        assert!(text.contains("PASSED"), "outcome passed: {text}");
        assert!(
            text.contains("tests/test_helper.py"),
            "ran the at-risk test: {text}"
        );
    }

    #[test]
    fn find_callers_and_callees_split_by_direction() {
        let gd = GraphData {
            nodes: vec![
                node("auth", "AuthService", Some(0)),
                node("login", "login_user", Some(0)),
                node("db", "Database", Some(0)),
            ],
            // AuthService calls login_user; login_user calls Database.
            links: vec![edge("auth", "login", "calls"), edge("login", "db", "calls")],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None);

        let callers = call_tool(&mut s, "find_callers", json!({"label": "login_user"}));
        assert!(callers.contains("AuthService"), "{callers}");
        assert!(
            !callers.contains("Database"),
            "callees must not appear: {callers}"
        );

        let callees = call_tool(&mut s, "find_callees", json!({"label": "login_user"}));
        assert!(callees.contains("Database"), "{callees}");
        assert!(
            !callees.contains("AuthService"),
            "callers must not appear: {callees}"
        );
    }

    #[test]
    fn find_references_includes_type_usages_excludes_ownership_and_members() {
        // User is a class with a member save(). Several nodes reference User in
        // different ways; X only calls the member, and Pkg merely contains User.
        let mut user = node("user", "User", Some(0));
        user.set_kind(synaptic_core::NodeKind::Class);
        let mut save = node("save", "save", Some(0));
        save.set_kind(synaptic_core::NodeKind::Function);
        let gd = GraphData {
            nodes: vec![
                user,
                save,
                node("sub", "Sub", Some(0)),
                node("modfile", "ModFile", Some(0)),
                node("consumer", "Consumer", Some(0)),
                node("caller", "Caller", Some(0)),
                node("pkg", "Pkg", Some(0)),
                node("xc", "XCaller", Some(0)),
            ],
            links: vec![
                edge("pkg", "user", "contains"),    // ownership -> excluded
                edge("user", "save", "contains"),   // User's member (outgoing) -> not a reference
                edge("sub", "user", "implements"),  // structural usage find_callers misses
                edge("modfile", "user", "imports"), // structural usage find_callers misses
                edge("consumer", "user", "uses"),   // a use
                edge("caller", "user", "calls"),    // a call
                edge("xc", "save", "calls"),        // caller of the member, NOT of User
            ],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None);

        let refs = call_tool(&mut s, "find_references", json!({"label": "User"}));
        // Every usage kind, including the structural ones find_callers omits.
        assert!(
            refs.contains("Sub") && refs.contains("implements"),
            "{refs}"
        );
        assert!(
            refs.contains("ModFile") && refs.contains("imports"),
            "{refs}"
        );
        assert!(
            refs.contains("Consumer") && refs.contains("Caller"),
            "{refs}"
        );
        // Ownership (contains) is not a usage; the container is excluded.
        assert!(
            !refs.contains("Pkg"),
            "ownership container excluded: {refs}"
        );
        assert!(
            !refs.contains("contains"),
            "no contains relation listed: {refs}"
        );
        // No member folding: a caller of the member is not a reference to the type.
        assert!(!refs.contains("XCaller"), "members not folded in: {refs}");

        // find_callers, by contrast, misses the structural (non-call) usages.
        let callers = call_tool(&mut s, "find_callers", json!({"label": "User"}));
        assert!(
            !callers.contains("Sub"),
            "find_callers omits implements: {callers}"
        );
        assert!(
            !callers.contains("ModFile"),
            "find_callers omits imports: {callers}"
        );
    }

    #[test]
    fn find_references_reports_none_when_unreferenced() {
        let gd = GraphData {
            // Leaf references User; nobody references Leaf.
            nodes: vec![node("leaf", "Leaf", Some(0)), node("user", "User", Some(0))],
            links: vec![edge("leaf", "user", "calls")],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None);
        let refs = call_tool(&mut s, "find_references", json!({"label": "Leaf"}));
        assert!(refs.contains("No references to Leaf"), "{refs}");
    }

    #[test]
    fn structural_search_file_lists_symbols_ordered_by_line() {
        let mut a = node("a", "alpha", Some(0));
        a.source_file = "mod.rs".into();
        a.set_span(synaptic_core::Span {
            start_line: 30,
            start_col: 1,
            end_line: 32,
            end_col: 1,
        });
        let mut b = node("b", "beta", Some(0));
        b.source_file = "mod.rs".into();
        b.set_span(synaptic_core::Span {
            start_line: 10,
            start_col: 1,
            end_line: 12,
            end_col: 1,
        });
        let mut c = node("c", "gamma", Some(0));
        c.source_file = "other.rs".into();
        let gd = GraphData {
            nodes: vec![a, b, c],
            links: vec![],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None);
        let out = call_tool(&mut s, "structural_search", json!({"file": "mod.rs"}));
        assert!(out.contains("alpha") && out.contains("beta"), "{out}");
        assert!(!out.contains("gamma"), "scopes to the file: {out}");
        // Ordered by start line: beta (L10) before alpha (L30).
        assert!(
            out.find("beta").unwrap() < out.find("alpha").unwrap(),
            "ordered by line: {out}"
        );
    }

    #[test]
    fn structural_search_query_takes_precedence_over_file() {
        let mut a = node("a", "alpha", Some(0));
        a.source_file = "mod.rs".into();
        let mut c = node("c", "gamma", Some(0));
        c.source_file = "other.rs".into();
        let gd = GraphData {
            nodes: vec![a, c],
            links: vec![],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None);
        // Both a query (matches gamma) and file given: the query wins, file ignored.
        let out = call_tool(
            &mut s,
            "structural_search",
            json!({"query": "MATCH (n) WHERE n.name =~ \"gamma\" RETURN n", "file": "mod.rs"}),
        );
        assert!(out.contains("gamma"), "query result present: {out}");
        assert!(
            !out.contains("alpha"),
            "file ignored when query given: {out}"
        );
    }

    #[test]
    fn find_references_surfaces_cross_repo_usages() {
        // Federated graph: a `web` repo calls an `api` repo's handler across the
        // service boundary, and an `api`-local helper uses it. find_references must
        // surface BOTH -- the cross-repo edge is just another incoming edge.
        let mut handler = node("api::handler", "Handler", Some(0));
        handler.repo = Some("api".into());
        handler.source_file = "api/src/handler.rs".into();
        let mut client = node("web::client", "WebClient", Some(0));
        client.repo = Some("web".into());
        client.source_file = "web/src/client.rs".into();
        let mut helper = node("api::helper", "ApiHelper", Some(0));
        helper.repo = Some("api".into());
        helper.source_file = "api/src/helper.rs".into();

        let mut cross = edge("web::client", "api::handler", "calls_service");
        cross.cross_repo = true;
        let gd = GraphData {
            nodes: vec![handler, client, helper],
            links: vec![cross, edge("api::helper", "api::handler", "uses")],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None);

        let refs = call_tool(&mut s, "find_references", json!({"label": "Handler"}));
        assert!(
            refs.contains("WebClient") && refs.contains("calls_service"),
            "cross-repo reference surfaced: {refs}"
        );
        assert!(
            refs.contains("ApiHelper") && refs.contains("uses"),
            "same-repo reference surfaced: {refs}"
        );
    }

    #[test]
    fn structural_search_file_outline_handles_federated_tag_prefix() {
        // Federated nodes carry `tag/rel` source paths. A tag-qualified `file`
        // scopes to one repo; a bare path matches that file across every member.
        let mut a = node("a", "ApiBig", Some(0));
        a.repo = Some("api".into());
        a.source_file = "api/src/lib.rs".into();
        a.set_span(synaptic_core::Span {
            start_line: 20,
            start_col: 1,
            end_line: 22,
            end_col: 1,
        });
        let mut b = node("b", "ApiSmall", Some(0));
        b.repo = Some("api".into());
        b.source_file = "api/src/lib.rs".into();
        b.set_span(synaptic_core::Span {
            start_line: 5,
            start_col: 1,
            end_line: 7,
            end_col: 1,
        });
        let mut c = node("c", "WebThing", Some(0));
        c.repo = Some("web".into());
        c.source_file = "web/src/lib.rs".into();
        let gd = GraphData {
            nodes: vec![a, b, c],
            links: vec![],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None);

        // Tag-qualified path -> only the `api` member's symbols, ordered by line.
        let scoped = call_tool(
            &mut s,
            "structural_search",
            json!({"file": "api/src/lib.rs"}),
        );
        assert!(
            scoped.contains("ApiBig") && scoped.contains("ApiSmall"),
            "{scoped}"
        );
        assert!(
            !scoped.contains("WebThing"),
            "tag scopes to one member: {scoped}"
        );
        assert!(
            scoped.find("ApiSmall").unwrap() < scoped.find("ApiBig").unwrap(),
            "ordered by line across the federated member: {scoped}"
        );

        // Bare path -> the same file in every member (substring match spans repos).
        let across = call_tool(&mut s, "structural_search", json!({"file": "src/lib.rs"}));
        assert!(
            across.contains("ApiBig") && across.contains("WebThing"),
            "bare path matches across members: {across}"
        );
    }

    #[test]
    fn find_callers_caps_at_limit_and_verbose_uncaps() {
        // A hub with 60 callers must summarize by default and dump all on verbose,
        // mirroring `affected`.
        let mut nodes = vec![node("hub", "hub", Some(0))];
        let mut links = Vec::new();
        for i in 0..60u32 {
            let cid = format!("c{i}");
            nodes.push(node(&cid, &cid, Some(0)));
            links.push(edge(&cid, "hub", "calls"));
        }
        let gd = GraphData {
            nodes,
            links,
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None);

        let capped = call_tool(&mut s, "find_callers", json!({"label": "hub"}));
        assert!(
            capped.starts_with("60 Callers of hub:"),
            "true total in header: {capped}"
        );
        assert!(
            capped.contains("+10 more"),
            "default limit 50 summarizes the tail: {capped}"
        );

        let full = call_tool(
            &mut s,
            "find_callers",
            json!({"label": "hub", "verbose": true}),
        );
        assert!(!full.contains("more"), "verbose uncaps: {full}");
        assert_eq!(
            full.matches("[calls]").count(),
            60,
            "verbose lists every caller"
        );

        let limited = call_tool(&mut s, "find_callers", json!({"label": "hub", "limit": 5}));
        assert!(
            limited.contains("+55 more"),
            "custom limit honored: {limited}"
        );
    }

    #[test]
    fn find_callees_header_shows_relation_breakdown() {
        // AuthService calls login_user and uses Database: two relation kinds, so
        // the header carries a per-relation breakdown.
        let mut s = server();
        let out = call_tool(&mut s, "find_callees", json!({"label": "AuthService"}));
        assert!(
            out.starts_with("2 Callees of AuthService [calls: 1, uses: 1]:"),
            "{out}"
        );
    }

    #[test]
    fn plan_rename_sites_render_with_location_and_cap() {
        fn site(file: &str, line: u32) -> synaptic_refactor::EditSite {
            synaptic_refactor::EditSite {
                file: file.into(),
                span: Some(synaptic_core::Span {
                    start_line: line,
                    start_col: 5,
                    end_line: line,
                    end_col: 9,
                }),
                line: Some(line),
                old: "foo".into(),
                new: "bar".into(),
                confidence: Confidence::Extracted,
                reason: "call site".into(),
                needs_review: false,
                repo: None,
            }
        }
        let sites = vec![site("a.rs", 1), site("b.rs", 2), site("c.rs", 3)];
        let mut o = String::new();
        append_capped_sites(&mut o, "Edits", &sites, 2);
        assert!(o.starts_with("\nEdits (3):"), "true count in header: {o}");
        assert!(o.contains("a.rs:1:5"), "file:line:col present: {o}");
        assert!(o.contains("`foo` -> `bar`"), "old -> new present: {o}");
        assert!(o.contains("+1 more"), "capped with summary: {o}");

        // An empty list emits no section at all.
        let mut empty = String::new();
        append_capped_sites(&mut empty, "Review", &[], 50);
        assert!(empty.is_empty(), "no section for an empty list: {empty:?}");
    }

    #[test]
    fn affected_lists_transitive_dependents() {
        // login_user calls check; AuthService calls login_user.
        // Changing `check` affects login_user (1 hop) and AuthService (2 hops).
        let gd = GraphData {
            nodes: vec![
                node("check", "check", Some(0)),
                node("login", "login_user", Some(0)),
                node("auth", "AuthService", Some(0)),
            ],
            links: vec![
                edge("login", "check", "calls"),
                edge("auth", "login", "calls"),
            ],
            ..Default::default()
        };
        let mut s = Server::from_graph_data(gd, None);
        let out = call_tool(&mut s, "affected", json!({"label": "check", "depth": 5}));
        assert!(out.contains("login_user"), "{out}");
        assert!(out.contains("AuthService"), "{out}");
        assert!(out.contains("via calls"), "{out}");
    }

    #[test]
    fn unknown_tool_is_a_tool_result_not_a_protocol_error() {
        let mut s = server();
        let r = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":3,"method":"tools/call",
                "params":{"name":"no_such_tool","arguments":{}}
            }))
            .unwrap();
        // Not a JSON-RPC error envelope; a tool result flagged isError.
        assert!(
            r.get("error").is_none(),
            "must not be a protocol error: {r}"
        );
        assert_eq!(r["result"]["isError"], json!(true));
        assert!(r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Unknown tool: no_such_tool"));
    }

    #[test]
    fn relation_filter_is_case_insensitive() {
        let mut s = server();
        let neigh = call_tool(
            &mut s,
            "get_neighbors",
            json!({"label": "AuthService", "relation_filter": "CALLS"}),
        );
        assert!(
            neigh.contains("login_user") && neigh.contains("[calls]"),
            "{neigh}"
        );
    }

    #[test]
    fn truncate_uses_real_token_count() {
        // A long ASCII body; a 5-token budget must cut it to ~5 real tokens.
        let body = "alpha beta gamma delta epsilon zeta eta theta iota kappa ".repeat(20);
        let out = truncate_to_tokens(body.clone(), 5);
        assert!(out.contains("truncated"), "should truncate: {out}");
        let kept = out.split('\n').next().unwrap();
        let bpe = tiktoken_rs::cl100k_base().unwrap();
        let n = bpe.encode_with_special_tokens(kept).len();
        assert!(n <= 6, "kept ~5 tokens, got {n}");
    }

    #[test]
    fn truncate_keeps_short_text_intact() {
        // Under budget in bytes -> returned verbatim, no tokenizing, no note.
        let short = "NODE a [code] a.py\n".to_string();
        assert_eq!(truncate_to_tokens(short.clone(), 2000), short);
    }

    #[test]
    fn nodes_found_parses_the_header() {
        assert_eq!(
            nodes_found("Traversal: bfs | Start: [a, b] | 5 nodes found\nNODE ..."),
            5
        );
        assert_eq!(nodes_found("Traversal: dfs | Start: [] | 0 nodes found"), 0);
        assert_eq!(nodes_found("garbage"), 0);
    }

    #[test]
    fn resources_list_and_read() {
        let mut s = server();
        let rl = s
            .handle_request(&json!({"jsonrpc":"2.0","id":1,"method":"resources/list"}))
            .unwrap();
        assert_eq!(rl["result"]["resources"].as_array().unwrap().len(), 6);
        let stats = s
            .handle_request(
                &json!({"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"synaptic://stats"}}),
            )
            .unwrap();
        assert!(stats["result"]["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("3 nodes"));
    }

    /// Mock gh/git runner returning a canned PR list.
    struct MockGh;
    impl CommandRunner for MockGh {
        fn run(&self, program: &str, args: &[&str]) -> Option<String> {
            if program == "gh" && args.first() == Some(&"pr") && args.get(1) == Some(&"list") {
                Some(
                    json!([{
                        "number": 7, "title": "Add auth", "headRefName": "feat/auth",
                        "baseRefName": "main", "author": {"login": "alice"}, "isDraft": false,
                        "reviewDecision": "APPROVED",
                        "statusCheckRollup": [{"conclusion": "SUCCESS"}],
                        "updatedAt": "2026-06-12T00:00:00Z"
                    }])
                    .to_string(),
                )
            } else if program == "gh" && args.first() == Some(&"repo") {
                Some(r#"{"defaultBranchRef":{"name":"main"}}"#.to_string())
            } else {
                None
            }
        }
    }

    // A git runner that reports db.py as heavily changed on `main`, for the
    // query_graph recency tests. Answers the three calls resolve_recency makes.
    struct RecencyGit;
    impl CommandRunner for RecencyGit {
        fn run(&self, program: &str, args: &[&str]) -> Option<String> {
            if program != "git" {
                return None;
            }
            match args.first().copied() {
                Some("rev-parse") => Some("a".repeat(40)),
                Some("merge-base") => Some("b".repeat(40)),
                Some("diff") => Some("20\t5\tdb.py\n".to_string()), // db.py churned
                _ => None,
            }
        }
    }

    fn query_graph_structured(s: &mut Server, args: Value) -> Value {
        let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"query_graph","arguments":args}});
        s.handle_request(&req).unwrap()["result"].clone()
    }

    #[test]
    fn recency_flags_changed_nodes_and_adds_header() {
        let mut s = server().with_runner(Box::new(RecencyGit));
        let res =
            query_graph_structured(&mut s, json!({"question":"auth database","since":"main"}));
        let nodes = res["structuredContent"]["nodes"].as_array().unwrap();
        let by_file = |f: &str| nodes.iter().find(|n| n["source_file"] == json!(f)).cloned();
        assert_eq!(
            by_file("db.py").unwrap()["changed"],
            json!(true),
            "db.py changed"
        );
        assert_eq!(
            by_file("auth.py").unwrap()["changed"],
            json!(false),
            "auth.py unchanged"
        );
        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("Recency: since main"),
            "header present: {text}"
        );
        assert!(text.contains("(changed)"), "changed marker present: {text}");
    }

    #[test]
    fn recency_seed_mode_surfaces_changed_node_with_no_query_match() {
        // The question matches nothing; seed mode must still surface the changed db.
        let mut s = server().with_runner(Box::new(RecencyGit));
        let res = query_graph_structured(
            &mut s,
            json!({"question":"zzz nomatch","since":"main","recency_mode":"seed"}),
        );
        let nodes = res["structuredContent"]["nodes"].as_array().unwrap();
        assert!(
            nodes
                .iter()
                .any(|n| n["source_file"] == json!("db.py") && n["changed"] == json!(true)),
            "seed mode should inject changed db.py: {nodes:?}"
        );
    }

    #[test]
    fn recency_degrades_gracefully_when_git_unavailable() {
        struct NoGit;
        impl CommandRunner for NoGit {
            fn run(&self, _p: &str, _a: &[&str]) -> Option<String> {
                None
            }
        }
        let mut s = server().with_runner(Box::new(NoGit));
        let res = query_graph_structured(&mut s, json!({"question":"auth","since":"main"}));
        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(
            !text.contains("Recency:"),
            "no recency header when git fails: {text}"
        );
        // Nodes still returned, none flagged changed.
        let nodes = res["structuredContent"]["nodes"].as_array().unwrap();
        assert!(!nodes.is_empty());
        assert!(nodes.iter().all(|n| n["changed"] == json!(false)));
    }

    /// A real symbol that imports an unresolved external target. The import
    /// target is an external stub: file_type code but empty source_file, so it
    /// cannot be opened with get_source.
    fn stub_server() -> Server {
        let real = node("auth", "AuthService", Some(0));
        let stub = synaptic_core::Node {
            id: NodeId("jsonwebtoken".into()),
            label: "jsonwebtoken".into(),
            file_type: FileType::Code,
            source_file: String::new(),
            source_location: None,
            community: Some(0),
            repo: None,
            extra: Map::new(),
        };
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![real, stub],
            links: vec![edge("auth", "jsonwebtoken", "imports_from")],
            hyperedges: vec![],
            built_at_commit: None,
        };
        Server::from_graph_data(gd, None)
    }

    #[test]
    fn query_graph_flags_external_stub_nodes() {
        let mut s = stub_server();
        let res = query_graph_structured(
            &mut s,
            json!({"question":"AuthService jsonwebtoken","full":true,"token_budget":400}),
        );
        let nodes = res["structuredContent"]["nodes"].as_array().unwrap();
        let stub = nodes
            .iter()
            .find(|n| n["label"] == json!("jsonwebtoken"))
            .expect("stub node present");
        assert_eq!(stub["external_stub"], json!(true), "stub flagged: {stub}");
        // A real symbol carries no stub flag (the key is omitted when false).
        let real = nodes
            .iter()
            .find(|n| n["label"] == json!("AuthService"))
            .expect("real node present");
        assert_eq!(
            real.get("external_stub"),
            None,
            "real node unflagged: {real}"
        );
        // The text rendering marks the stub so a reader knows get_source won't work.
        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("jsonwebtoken") && text.contains("(external)"),
            "text marks the stub: {text}"
        );
    }

    /// A function whose only outgoing edges are type references (no real calls);
    /// its callees should carry a note that none are in-graph call targets.
    fn type_ref_only_server() -> Server {
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                node("cof", "communities_of", Some(0)),
                node("btm", "BTreeMap", Some(0)),
                node("kg", "KnowledgeGraph", Some(0)),
            ],
            links: vec![
                edge("cof", "btm", "references"),
                edge("cof", "kg", "references"),
            ],
            hyperedges: vec![],
            built_at_commit: None,
        };
        Server::from_graph_data(gd, None)
    }

    #[test]
    fn find_callees_notes_when_only_type_references() {
        let mut s = type_ref_only_server();
        let text = call_tool(&mut s, "find_callees", json!({"label": "communities_of"}));
        // The entries are type references, not calls; the output must say so
        // rather than reading like "this function calls 2 things".
        assert!(text.contains("BTreeMap"), "{text}");
        assert!(
            text.contains("no in-graph callee"),
            "callee note present: {text}"
        );
    }

    #[test]
    fn shortest_path_shows_relation_per_hop() {
        let mut s = server();
        let path = call_tool(
            &mut s,
            "shortest_path",
            json!({"source": "login_user", "target": "Database"}),
        );
        // login_user <-calls- AuthService -uses-> Database; the rendered path must
        // surface the relation on each hop, not just the node labels.
        assert!(path.starts_with("Shortest path"), "{path}");
        assert!(path.contains("-[calls]->"), "calls hop shown: {path}");
        assert!(path.contains("-[uses]->"), "uses hop shown: {path}");
    }

    #[test]
    fn working_changes_impact_uses_git_diff() {
        struct GitRunner;
        impl CommandRunner for GitRunner {
            fn run(&self, program: &str, args: &[&str]) -> Option<String> {
                if program == "git" && args.first() == Some(&"rev-parse") {
                    return Some("true\n".to_string()); // inside a work tree
                }
                if program == "git" && args.first() == Some(&"diff") {
                    return Some("auth.py\n".to_string()); // one changed file
                }
                None
            }
        }
        // node "auth" lives in auth.py, community 0.
        let mut a = node("auth", "AuthService", Some(0));
        a.source_file = "auth.py".into();
        let gd = GraphData {
            nodes: vec![a],
            ..Default::default()
        };
        let s = Server::from_graph_data(gd, None).with_runner(Box::new(GitRunner));
        let out = s.tool_working_changes_impact(Some("main"), 20, false, false);
        assert!(out.contains("auth.py"), "names the changed file: {out}");
        assert!(
            out.contains("1 communities touched"),
            "reports impact: {out}"
        );
        // Default output is files-only; node/community detail is opt-in.
        assert!(
            !out.contains("Top nodes"),
            "default output stays files-only: {out}"
        );

        // Verbose lists the touched nodes and labeled communities.
        let verbose = s.tool_working_changes_impact(Some("main"), 20, true, false);
        assert!(
            verbose.contains("Top nodes (1):"),
            "verbose lists touched nodes: {verbose}"
        );
        assert!(
            verbose.contains("AuthService [node] auth.py (0 edges)"),
            "node line carries kind/file/degree: {verbose}"
        );
        assert!(
            verbose.contains("community 0: AuthService"),
            "verbose labels the touched community: {verbose}"
        );

        // In a repo with no diff -> clean-tree message, no panic.
        struct CleanRunner;
        impl CommandRunner for CleanRunner {
            fn run(&self, _p: &str, args: &[&str]) -> Option<String> {
                if args.first() == Some(&"rev-parse") {
                    return Some("true\n".to_string());
                }
                Some(String::new()) // empty diff
            }
        }
        let s2 = server().with_runner(Box::new(CleanRunner));
        let clean = s2.tool_working_changes_impact(Some("main"), 20, false, false);
        assert!(clean.contains("No changes"), "clean tree: {clean}");
        assert!(
            !clean.contains("git unavailable"),
            "a clean tree is not git-unavailable: {clean}"
        );

        // Not a repo / git missing -> a distinct outcome, not "No changes".
        struct NoRepoRunner;
        impl CommandRunner for NoRepoRunner {
            fn run(&self, _p: &str, _a: &[&str]) -> Option<String> {
                None // git fails / not a repo
            }
        }
        let s3 = server().with_runner(Box::new(NoRepoRunner));
        let no_repo = s3.tool_working_changes_impact(Some("main"), 20, false, false);
        assert!(
            no_repo.contains("not a git repository"),
            "git-unavailable is distinct from no-changes: {no_repo}"
        );
        assert!(!no_repo.contains("No changes vs"), "{no_repo}");
    }

    #[test]
    fn working_changes_impact_code_only_filters_non_code() {
        // A code file and a non-code config file both change. `code_only` should
        // drop the config node from the blast-radius count and the node list.
        struct GitRunner;
        impl CommandRunner for GitRunner {
            fn run(&self, program: &str, args: &[&str]) -> Option<String> {
                if program == "git" && args.first() == Some(&"rev-parse") {
                    return Some("true\n".to_string());
                }
                if program == "git" && args.first() == Some(&"diff") {
                    return Some("app.ts\npackage.json\n".to_string());
                }
                None
            }
        }
        let mut code = node("app", "App", Some(0));
        code.source_file = "app.ts".into();
        let mut cfg = node("pkg", "package.json", Some(0));
        cfg.source_file = "package.json".into();
        cfg.file_type = FileType::Document;
        let gd = GraphData {
            nodes: vec![code, cfg],
            ..Default::default()
        };
        let s = Server::from_graph_data(gd, None).with_runner(Box::new(GitRunner));
        // Default counts both nodes (existing behavior preserved).
        let all = s.tool_working_changes_impact(Some("main"), 20, true, false);
        assert!(all.contains("2 graph nodes"), "default counts all: {all}");
        assert!(all.contains("package.json [node]"), "lists config: {all}");
        // code_only drops the non-code node from count and list.
        let code_only = s.tool_working_changes_impact(Some("main"), 20, true, true);
        assert!(
            code_only.contains("1 code graph nodes"),
            "code_only excludes config from count: {code_only}"
        );
        assert!(
            code_only.contains("App [") && !code_only.contains("package.json ["),
            "code_only lists only code nodes: {code_only}"
        );
    }

    #[test]
    fn pr_tools_use_the_injected_runner() {
        let mut s = server().with_runner(Box::new(MockGh));
        let out = call_tool(&mut s, "list_prs", json!({"base": "main"}));
        assert!(out.contains("#7"), "list_prs renders the PR: {out}");
        assert!(out.contains("Add auth"));
    }

    /// C1 part B: `triage_prs` must fetch each PR's changed files concurrently
    /// (the `gh pr diff` subprocess is the dominant latency), not one at a time.
    /// A runner records the peak number of in-flight `diff` calls; with the old
    /// sequential loop that is 1, with bounded-concurrent fetch it is > 1.
    #[test]
    fn triage_fetches_pr_files_concurrently() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct ConcurRunner {
            inflight: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
            list: String,
        }
        impl CommandRunner for ConcurRunner {
            fn run(&self, program: &str, args: &[&str]) -> Option<String> {
                if program == "gh" && args.first() == Some(&"pr") && args.get(1) == Some(&"list") {
                    return Some(self.list.clone());
                }
                if program == "gh" && args.first() == Some(&"pr") && args.get(1) == Some(&"diff") {
                    let n = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
                    self.peak.fetch_max(n, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    self.inflight.fetch_sub(1, Ordering::SeqCst);
                    return Some("a.rs\nb.rs".to_string());
                }
                None // git/worktrees etc. -> empty
            }
        }

        // 3 actionable PRs targeting main. A far-future `updatedAt` keeps
        // `days_old` clamped at 0 so they never go stale (run-date independent).
        let pr = |n: u64| {
            json!({
                "number": n, "title": format!("pr{n}"), "headRefName": format!("f{n}"),
                "baseRefName": "main", "author": {"login": "a"}, "isDraft": false,
                "reviewDecision": "APPROVED",
                "statusCheckRollup": [{"conclusion": "SUCCESS"}],
                "updatedAt": "2099-01-01T00:00:00Z"
            })
        };
        let list = json!([pr(1), pr(2), pr(3)]).to_string();

        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let runner = ConcurRunner {
            inflight: inflight.clone(),
            peak: peak.clone(),
            list,
        };
        let s = server().with_runner(Box::new(runner));
        let out = s.tool_triage_prs(Some("main"), None);

        assert!(
            out.contains("PR #1") && out.contains("PR #3"),
            "all PRs triaged: {out}"
        );
        assert!(
            peak.load(Ordering::SeqCst) >= 2,
            "per-PR file fetches must run concurrently; peak in-flight = {}",
            peak.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn hot_reload_picks_up_a_changed_graph() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.json");
        let one = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![node("a", "A", Some(0))],
            links: vec![],
            hyperedges: vec![],
            built_at_commit: None,
        };
        std::fs::write(&path, serde_json::to_vec(&one).unwrap()).unwrap();
        let mut s = Server::load(path.clone()).unwrap();
        assert!(call_tool(&mut s, "graph_stats", json!({})).contains("1 nodes"));

        // Rewrite graph.json with more nodes; ensure a distinct mtime/size.
        let two = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![node("a", "A", Some(0)), node("b", "B", Some(0))],
            links: vec![edge("a", "b", "calls")],
            hyperedges: vec![],
            built_at_commit: None,
        };
        std::fs::write(&path, serde_json::to_vec(&two).unwrap()).unwrap();
        assert!(
            call_tool(&mut s, "graph_stats", json!({})).contains("2 nodes"),
            "tool call hot-reloads the changed graph"
        );
    }

    #[test]
    fn god_nodes_and_stats_caches_reflect_the_graph_and_hot_reload() {
        // H3: cached god_nodes/stats must render the current graph exactly, and
        // a rebuilt graph.json must refresh both caches on the next request.
        let mut s = server();
        assert!(call_tool(&mut s, "god_nodes", json!({"top_n": 1}))
            .contains("\n  1. AuthService - 2 connections, 0 test(s)"));
        let three = call_tool(&mut s, "god_nodes", json!({"top_n": 3}));
        assert!(
            three.contains("\n  1. AuthService - 2 connections, 0 test(s)"),
            "{three}"
        );
        assert!(
            three.contains("\n  2. Database - 1 connections, 0 test(s)"),
            "{three}"
        );
        assert!(
            three.contains("\n  3. login_user - 1 connections, 0 test(s)"),
            "{three}"
        );
        assert!(call_tool(&mut s, "graph_stats", json!({})).contains("3 nodes, 2 edges"));

        // Hot reload to a graph with a new, higher-degree hub.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.json");
        let g1 = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![node("a", "Alpha", Some(0)), node("b", "Beta", Some(0))],
            links: vec![edge("a", "b", "calls")],
            hyperedges: vec![],
            built_at_commit: None,
        };
        std::fs::write(&path, serde_json::to_vec(&g1).unwrap()).unwrap();
        let mut s = Server::load(path.clone()).unwrap();
        assert!(call_tool(&mut s, "god_nodes", json!({"top_n": 1})).contains("Alpha"));

        let g2 = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                node("core", "Core", Some(0)),
                node("x", "X", Some(0)),
                node("y", "Y", Some(0)),
                node("z", "Z", Some(0)),
            ],
            links: vec![
                edge("core", "x", "calls"),
                edge("core", "y", "calls"),
                edge("core", "z", "calls"),
            ],
            hyperedges: vec![],
            built_at_commit: None,
        };
        std::fs::write(&path, serde_json::to_vec(&g2).unwrap()).unwrap();
        let gods = call_tool(&mut s, "god_nodes", json!({"top_n": 1}));
        assert!(
            gods.contains("Core - 3 connections") && !gods.contains("Alpha"),
            "god_nodes must reflect the reloaded graph: {gods}"
        );
        assert!(call_tool(&mut s, "graph_stats", json!({})).contains("4 nodes, 3 edges"));
    }
}
