//! MCP server for CodeGraph.
//!
//! C3a — the read-only tool surface over **stdio**: an AI assistant drives the
//! graph via MCP. Rather than depend on `rmcp` (whose API churns), we speak the
//! MCP stdio transport directly — newline-delimited JSON-RPC 2.0 — through a
//! pure [`Server::handle_request`] dispatcher, which makes the whole protocol
//! unit-testable without an async runtime.
//!
//! Twelve read-only tools over a graph loaded at startup: graph (`query_graph`,
//! `get_node`, `get_neighbors`, `get_community`, `god_nodes`, `graph_stats`,
//! `shortest_path`), federation (`list_repos`, `repo_stats`), and PR (`list_prs`,
//! `get_pr_impact`, `triage_prs`), plus six resources. The `initialize` reply
//! returns server `instructions` orienting the agent, and each tool documents its
//! parameters, so an assistant uses them correctly. Every label is run through
//! [`codegraph_core::sanitize_label`] before it reaches tool text (a security
//! boundary on LLM/corpus-derived names).
#![forbid(unsafe_code)]

mod http;
mod prompts;
pub mod session;
mod source;
pub use http::serve_http;
pub use session::{SessionStore, DEFAULT_SESSION_IDLE};

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use codegraph_core::{sanitize_label, GraphData, NodeId};
use codegraph_graph::{
    god_nodes, graph_stats, suggest_questions, surprising_connections, GodNode, GraphStats,
    KnowledgeGraph,
};
use codegraph_prs::{
    compute_pr_impact, detect_default_branch, fetch_pr, fetch_pr_files, fetch_prs, fetch_worktrees,
    format_pr_detail, format_prs_text, today_epoch_days, CommandRunner, ImpactIndex, PrInfo,
    Status, SystemCommands,
};
use codegraph_query::{
    affected_nodes, explain, resolve_seed, shortest_path, QueryIndex, TraversalMode,
    DEFAULT_AFFECTED_RELATIONS,
};
use serde_json::{json, Value};

const SUPPORTED_PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const LATEST_PROTOCOL: &str = "2025-06-18";

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
/// changes (C3c). [`handle_request`](Server::handle_request) takes `&mut self`;
/// the HTTP transport shares one behind an `Arc<Mutex<Server>>` (requests are
/// low-QPS, so serializing them is fine).
pub struct Server {
    kg: KnowledgeGraph,
    communities: BTreeMap<u32, Vec<NodeId>>,
    /// IDF + adjacency index for `query_graph`, built once at load/reload so
    /// queries don't rebuild it per request (H1).
    query_index: QueryIndex,
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
    /// JSONL query-log path (opt-in via `CODEGRAPH_QUERY_LOG`); `None` = off.
    log_path: Option<PathBuf>,
    /// Trusted root for resolving repo-relative `source_file` paths to real
    /// files (the code-retrieval tools). `None` disables source reading.
    source_root: Option<PathBuf>,
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
    let disabled = std::env::var("CODEGRAPH_QUERY_LOG_DISABLE")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false);
    if disabled {
        return None;
    }
    std::env::var("CODEGRAPH_QUERY_LOG").ok().map(PathBuf::from)
}

impl Server {
    /// Build a server from already-parsed graph data.
    pub fn from_graph_data(gd: GraphData, graph_path: Option<PathBuf>) -> Server {
        let kg = KnowledgeGraph::from_graph_data(gd);
        let communities = communities_of(&kg);
        let query_index = QueryIndex::build(&kg);
        let stats = graph_stats(&kg);
        let god_nodes_all = god_nodes(&kg, usize::MAX);
        let reload_key = graph_path.as_deref().and_then(reload_key_for);
        Server {
            kg,
            communities,
            query_index,
            stats,
            god_nodes_all,
            graph_path,
            reload_key,
            runner: Box::new(SystemCommands),
            log_path: query_log_path(),
            source_root: None,
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
        self.source_root = Some(root);
        self
    }

    /// Resolve a node's `source_file` to a real, in-jail path (or `None`).
    fn resolve_source_path(&self, rel: &str) -> Option<PathBuf> {
        let root = self.source_root.as_deref()?;
        source::resolve_in_root(root, rel)
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
                self.kg = KnowledgeGraph::from_graph_data(gd);
                self.communities = communities_of(&self.kg);
                self.query_index = QueryIndex::build(&self.kg);
                self.stats = graph_stats(&self.kg);
                self.god_nodes_all = god_nodes(&self.kg, usize::MAX);
                self.reload_key = Some(key);
            }
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

    // tools

    /// Retrieve the subgraph for `question` and apply `context_filter`, returning
    /// the raw result plus the indices into `r.nodes` that survived the filter.
    /// Shared by the text and structured renderers so a request runs the index
    /// query once (not once per output shape).
    fn query_filtered(
        &self,
        question: &str,
        mode: TraversalMode,
        token_budget: usize,
        context_filter: &[String],
    ) -> (codegraph_query::QueryResult, Vec<usize>) {
        // Map a token budget to a node cap (heuristic); the text render is later
        // truncated to the budget by truncate_to_tokens.
        let max_nodes = (token_budget / 40).clamp(10, 400);
        let r = self.query_index.query(&self.kg, question, max_nodes, mode);
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
        (r, keep)
    }

    fn render_query_text(
        &self,
        r: &codegraph_query::QueryResult,
        keep: &[usize],
        mode: TraversalMode,
        token_budget: usize,
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
        for &i in keep {
            if let Some(n) = self.kg.node(&r.nodes[i]) {
                out.push_str(&format!(
                    "NODE {} [{}] {}\n",
                    sanitize_label(&n.label),
                    file_type_str(&n.file_type),
                    sanitize_label(&n.source_file)
                ));
            }
        }
        for e in &r.edges {
            if in_set.contains(&e.source) && in_set.contains(&e.target) {
                out.push_str(&format!(
                    "EDGE {} --{}--> {}\n",
                    sanitize_label(&self.label_of(&e.source)),
                    sanitize_label(&e.relation),
                    sanitize_label(&self.label_of(&e.target))
                ));
            }
        }
        truncate_to_tokens(out, token_budget)
    }

    /// `query_graph` text render. The MCP `tools/call` path renders text and
    /// structured output from a single [`query_filtered`] retrieval; this stays
    /// for the REST surface and direct callers.
    pub fn tool_query_graph(
        &self,
        question: &str,
        mode: TraversalMode,
        token_budget: usize,
        context_filter: &[String],
    ) -> String {
        let (r, keep) = self.query_filtered(question, mode, token_budget, context_filter);
        self.render_query_text(&r, &keep, mode, token_budget)
    }

    /// `get_node` — metadata + degree for the node matching `label`.
    pub fn tool_get_node(&self, label: &str) -> String {
        let Some(id) = resolve_seed(&self.kg, label) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        let Some(n) = self.kg.node(&id) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        format!(
            "Node: {}\nID: {}\nSource: {}\nType: {}\nCommunity: {}\nDegree: {}",
            sanitize_label(&n.label),
            sanitize_label(&n.id.0),
            sanitize_label(&n.source_file),
            file_type_str(&n.file_type),
            n.community
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
            self.degree(&id)
        )
    }

    /// `get_source` — the actual source lines for a symbol. Resolves the node,
    /// reads its file under the source-root jail, and returns a window starting
    /// at the node's recorded line (`source_location` = "L<n>"). The graph has
    /// no end line, so it returns `context_lines` lines from the start.
    pub fn tool_get_source(&self, label: &str, context_lines: usize) -> String {
        let Some(id) = resolve_seed(&self.kg, label) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        let Some(n) = self.kg.node(&id) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        let Some(path) = self.resolve_source_path(&n.source_file) else {
            return format!(
                "Source not available for {} ({}).",
                sanitize_label(&n.label),
                sanitize_label(&n.source_file)
            );
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
        let to = (from + window).min(lines.len());
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

    /// `get_neighbors` — in/out neighbours, optionally filtered by relation.
    pub fn tool_get_neighbors(&self, label: &str, relation_filter: Option<&str>) -> String {
        let Some(id) = resolve_seed(&self.kg, label) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        let Some(ex) = explain(&self.kg, &id) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        let mut out = format!("Neighbors of {}:", sanitize_label(&ex.label));
        let rel_filter = relation_filter.map(str::to_lowercase);
        for nb in &ex.neighbors {
            if let Some(f) = &rel_filter {
                // Case-insensitive substring (lowercase both sides).
                if !nb.relation.to_lowercase().contains(f.as_str()) {
                    continue;
                }
            }
            let arrow = if nb.direction == "out" { "-->" } else { "<--" };
            out.push_str(&format!(
                "\n  {} {} [{}]",
                arrow,
                sanitize_label(&nb.label),
                sanitize_label(&nb.relation)
            ));
        }
        out
    }

    /// `get_community` — members of a community. Uses the prebuilt, sorted
    /// community index (kept fresh across hot-reloads) rather than rescanning
    /// every node.
    pub fn tool_get_community(&self, community_id: u32) -> String {
        let Some(ids) = self
            .communities
            .get(&community_id)
            .filter(|v| !v.is_empty())
        else {
            return format!("No community {community_id}.");
        };
        let mut out = format!("Community {community_id} ({} nodes):", ids.len());
        for id in ids {
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

    /// `god_nodes` — the top-N highest-degree nodes.
    pub fn tool_god_nodes(&self, top_n: usize) -> String {
        // Slice from the precomputed ranked list (H3). `god_nodes` returns one
        // node even for top_n == 0 (it pushes then checks the cap), so mirror
        // that with `max(1)` to stay byte-identical to the old per-call path.
        let take = self.god_nodes_all.len().min(top_n.max(1));
        let gods = &self.god_nodes_all[..take];
        if gods.is_empty() {
            return "No nodes.".to_string();
        }
        let mut out = String::from("God nodes:");
        for (i, g) in gods.iter().enumerate() {
            out.push_str(&format!(
                "\n  {}. {} - {} edges",
                i + 1,
                sanitize_label(&g.label),
                g.degree
            ));
        }
        out
    }

    /// `graph_stats` — counts + confidence breakdown.
    pub fn tool_graph_stats(&self) -> String {
        let s = &self.stats;
        format!(
            "Graph: {} nodes, {} edges, {} communities\nEdges: {} EXTRACTED, {} INFERRED, {} AMBIGUOUS",
            s.nodes, s.edges, s.communities, s.extracted, s.inferred, s.ambiguous
        )
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
            out.push_str(&format!(
                "\n  {} - {n} nodes, {ed} edges",
                sanitize_label(repo)
            ));
        }
        out
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
        let (Some(from), Some(to)) = (
            resolve_seed(&self.kg, source),
            resolve_seed(&self.kg, target),
        ) else {
            return "Could not resolve source and/or target to a unique node.".to_string();
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
                let labels: Vec<String> = path
                    .iter()
                    .map(|id| sanitize_label(&self.label_of(id)))
                    .collect();
                format!("Shortest path ({hops} hops): {}", labels.join(" -> "))
            }
            None => format!(
                "No path between {} and {}.",
                sanitize_label(&self.label_of(&from)),
                sanitize_label(&self.label_of(&to))
            ),
        }
    }

    /// `find_callers` — who calls/uses this node (incoming call-like edges).
    pub fn tool_find_callers(&self, label: &str) -> String {
        self.directional("Callers", label, "in")
    }

    /// `find_callees` — what this node calls/uses (outgoing call-like edges).
    pub fn tool_find_callees(&self, label: &str) -> String {
        self.directional("Callees", label, "out")
    }

    fn directional(&self, title: &str, label: &str, dir: &str) -> String {
        let Some(id) = resolve_seed(&self.kg, label) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        let Some(ex) = explain(&self.kg, &id) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        let mut out = format!("{title} of {}:", sanitize_label(&ex.label));
        let mut any = false;
        for nb in &ex.neighbors {
            // Call-like relations only; direction filtered.
            let rel = nb.relation.to_lowercase();
            let call_like =
                rel.contains("call") || rel.contains("use") || rel.contains("reference");
            if nb.direction == dir && call_like {
                any = true;
                out.push_str(&format!(
                    "\n  {} [{}]",
                    sanitize_label(&nb.label),
                    sanitize_label(&nb.relation)
                ));
            }
        }
        if !any {
            out.push_str("\n  (none)");
        }
        out
    }

    /// `affected` — the nodes that transitively depend on `label`, found by
    /// walking impact edges backward up to `depth` hops. Empty `relations`
    /// uses the default structural-impact set.
    pub fn tool_affected(&self, label: &str, depth: usize, relations: &[String]) -> String {
        let Some(id) = resolve_seed(&self.kg, label) else {
            return format!("No node matches '{}'.", sanitize_label(label));
        };
        let rels: Vec<&str> = if relations.is_empty() {
            DEFAULT_AFFECTED_RELATIONS.to_vec()
        } else {
            relations.iter().map(String::as_str).collect()
        };
        let depth = depth.clamp(1, 16);
        let hits = affected_nodes(&self.kg, &id, &rels, depth);
        let seed = sanitize_label(&self.label_of(&id));
        if hits.is_empty() {
            return format!("Nothing depends on {seed} within {depth} hops.");
        }
        let mut out = format!("{} nodes depend on {seed} (<= {depth} hops):", hits.len());
        for h in &hits {
            out.push_str(&format!(
                "\n  [{}h via {}] {}",
                h.depth,
                sanitize_label(&h.via_relation),
                sanitize_label(&self.label_of(&h.node_id))
            ));
        }
        out
    }

    // PR tools (via codegraph-prs; data-only, no LLM)

    fn resolve_base(&self, base: Option<&str>, repo: Option<&str>) -> String {
        match base {
            Some(b) => b.to_string(),
            None => detect_default_branch(&*self.runner, repo),
        }
    }

    fn graph_impact(&self, files: &[String]) -> (Vec<u32>, usize) {
        compute_pr_impact(
            self.kg
                .nodes()
                .map(|n| (n.source_file.as_str(), n.community)),
            files,
        )
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
            return format!("PR #{number} not found (gh unavailable or no such PR).");
        };
        pr.files_changed = fetch_pr_files(&*self.runner, number, repo);
        if pr.files_changed.is_empty() {
            return format!("PR #{number}: no changed files found (may require gh auth).");
        }
        let (comms, nodes) = self.graph_impact(&pr.files_changed);
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

    /// Fail-silent JSONL query log. Opt-in via `CODEGRAPH_QUERY_LOG`.
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
        json!({
            "nodes": s.nodes, "edges": s.edges, "communities": s.communities,
            "extracted": s.extracted, "inferred": s.inferred, "ambiguous": s.ambiguous
        })
    }

    fn god_nodes_json(&self, top_n: usize) -> Value {
        let take = self.god_nodes_all.len().min(top_n.max(1));
        let arr: Vec<Value> = self.god_nodes_all[..take]
            .iter()
            .map(|g| {
                json!({
                    "label": sanitize_label(&g.label),
                    "degree": g.degree,
                    "id": sanitize_label(&g.id.0)
                })
            })
            .collect();
        json!({ "god_nodes": arr })
    }

    fn affected_json(&self, label: &str, depth: usize, relations: &[String]) -> Value {
        let Some(id) = resolve_seed(&self.kg, label) else {
            return json!({ "seed": sanitize_label(label), "affected": [] });
        };
        let rels: Vec<&str> = if relations.is_empty() {
            DEFAULT_AFFECTED_RELATIONS.to_vec()
        } else {
            relations.iter().map(String::as_str).collect()
        };
        let hits = affected_nodes(&self.kg, &id, &rels, depth.clamp(1, 16));
        let arr: Vec<Value> = hits
            .iter()
            .map(|h| {
                json!({
                    "label": sanitize_label(&self.label_of(&h.node_id)),
                    "depth": h.depth,
                    "via_relation": sanitize_label(&h.via_relation)
                })
            })
            .collect();
        json!({ "seed": sanitize_label(&self.label_of(&id)), "affected": arr })
    }

    /// Typed mirror of [`render_query_text`](Server::render_query_text) over the
    /// same filtered retrieval, so structuredContent stays consistent with the
    /// rendered text without re-querying.
    fn render_query_json(&self, r: &codegraph_query::QueryResult, keep: &[usize]) -> Value {
        let in_set: std::collections::HashSet<&NodeId> =
            keep.iter().map(|&i| &r.nodes[i]).collect();
        let nodes: Vec<Value> = keep
            .iter()
            .filter_map(|&i| self.kg.node(&r.nodes[i]))
            .map(|n| {
                json!({
                    "label": sanitize_label(&n.label),
                    "file_type": file_type_str(&n.file_type),
                    "source_file": sanitize_label(&n.source_file)
                })
            })
            .collect();
        let edges: Vec<Value> = r
            .edges
            .iter()
            .filter(|e| in_set.contains(&e.source) && in_set.contains(&e.target))
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
                    "capabilities": { "tools": {}, "resources": {}, "prompts": {} },
                    "serverInfo": { "name": "codegraph", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": SERVER_INSTRUCTIONS,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools_list() })),
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
    /// matching [`maybe_reload`](Server::maybe_reload)'s own decision.
    pub fn is_stale(&self) -> bool {
        let Some(path) = &self.graph_path else {
            return false;
        };
        match reload_key_for(path) {
            Some(key) => self.reload_key != Some(key),
            None => false,
        }
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
            let budget = u("token_budget", 2000) as usize;
            let (r, keep) = self.query_filtered(&question, mode, budget, &ctx);
            let text = self.render_query_text(&r, &keep, mode, budget);
            // Log the "<n> nodes found" count from the header.
            self.log_query(&question, nodes_found(&text));
            let structured = self.render_query_json(&r, &keep);
            return Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": structured,
                "isError": false
            }));
        }

        let text = match name {
            "get_node" => self.tool_get_node(&s("label")),
            "get_source" => self.tool_get_source(&s("label"), u("context_lines", 40) as usize),
            "get_neighbors" => {
                let rf = args.get("relation_filter").and_then(Value::as_str);
                self.tool_get_neighbors(&s("label"), rf)
            }
            "get_community" => self.tool_get_community(u("community_id", 0) as u32),
            "god_nodes" => self.tool_god_nodes(u("top_n", 10) as usize),
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
                self.tool_affected(&s("label"), u("depth", 3) as usize, &rels)
            }
            "find_callers" => self.tool_find_callers(&s("label")),
            "find_callees" => self.tool_find_callees(&s("label")),
            "list_prs" => self.tool_list_prs(opt("base"), opt("repo")),
            "get_pr_impact" => self.tool_get_pr_impact(u("pr_number", 0), opt("repo")),
            "triage_prs" => self.tool_triage_prs(opt("base"), opt("repo")),
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
            "god_nodes" => Some(self.god_nodes_json(u("top_n", 10) as usize)),
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
                Some(self.affected_json(&s("label"), u("depth", 3) as usize, &rels))
            }
            _ => None,
        };

        let mut result = json!({ "content": [{ "type": "text", "text": text }], "isError": false });
        if let Some(sc) = structured {
            result["structuredContent"] = sc;
        }
        Ok(result)
    }

    fn dispatch_resource(&self, params: &Value) -> Result<Value, (i64, String)> {
        let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
        let (mime, text) = match uri {
            "codegraph://report" => ("text/markdown", self.resource_report()),
            "codegraph://stats" => ("text/plain", self.tool_graph_stats()),
            "codegraph://god-nodes" => ("text/plain", self.tool_god_nodes(10)),
            "codegraph://surprises" => ("text/plain", self.resource_surprises()),
            "codegraph://audit" => ("text/plain", self.resource_audit()),
            "codegraph://questions" => ("text/plain", self.resource_questions()),
            other => return Err((-32602, format!("Unknown resource: {other}"))),
        };
        Ok(json!({ "contents": [{ "uri": uri, "mimeType": mime, "text": text }] }))
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

fn file_type_str(ft: &codegraph_core::FileType) -> &'static str {
    use codegraph_core::FileType::*;
    match ft {
        Code => "code",
        Document => "document",
        Paper => "paper",
        Image => "image",
        Rationale => "rationale",
        Concept => "concept",
    }
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

/// Truncate rendered text to roughly `token_budget` tokens (~4 chars/token),
/// appending a note when cut.
fn truncate_to_tokens(text: String, token_budget: usize) -> String {
    let cap = token_budget.saturating_mul(4);
    if text.len() <= cap {
        return text;
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (truncated to ~{token_budget} tokens)", &text[..end])
}

/// Server-level orientation returned in the MCP `initialize` result. It frames
/// the whole toolset (these tools all query THIS repo's CodeGraph), gives the
/// recommended flow, and defines the jargon, so an agent picks the right tool.
const SERVER_INSTRUCTIONS: &str = "\
This server exposes a CodeGraph knowledge graph of THIS repository's code: symbols \
(functions, classes, files) as nodes and relationships (calls, imports, inheritance) \
as edges, clustered into communities. All tools here operate on that loaded graph and \
make no code edits. Query the graph before grepping or reading files broadly; it is \
faster and surfaces structure (callers, callees, impact).\n\
\n\
Recommended flow: call graph_stats or god_nodes to orient, query_graph for a question \
(returns a relevant subgraph), then get_neighbors / shortest_path / get_node to drill \
in. For a multi-repo graph, call list_repos then pass the repo argument to scope. The \
PR tools (list_prs / get_pr_impact / triage_prs) need the `gh` CLI.\n\
\n\
Terms: a 'god node' is a high-degree hub (structurally central); a 'community' is a \
cluster of densely-connected nodes (roughly a module); edge confidence is EXTRACTED \
(observed in code), INFERRED, or AMBIGUOUS.";

/// The MCP `tools/list` payload. Descriptions and per-parameter docs make the
/// implicit domain knowledge explicit so an agent uses each tool correctly
/// (graph jargon, the lenient label resolution, the relation vocabulary).
fn tools_list() -> Value {
    let mut tools = json!([
        {
            "name": "query_graph",
            "description": "Primary entry point: return a relevant subgraph (nodes + edges) for a natural-language question about this codebase, instead of grepping or reading files. Good for 'where is X handled', 'how does auth work', 'what is related to Y'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "Natural-language question, e.g. 'how does login work' or 'what handles payments'." },
                    "mode": { "type": "string", "enum": ["bfs", "dfs"], "description": "Traversal from the seed nodes: 'bfs' (default) expands a broad neighbourhood; 'dfs' follows deep call chains. Use dfs to trace one flow end to end." },
                    "token_budget": { "type": "integer", "description": "Approximate token budget for the result (default 2000). Controls how many nodes return (about budget/40, capped 10-400). Raise it for broader context." },
                    "context_filter": { "type": "array", "items": { "type": "string" }, "description": "Optional source-file path substrings; keeps only nodes whose file matches one (e.g. ['src/auth','login']). Use to scope a question to a subsystem." }
                },
                "required": ["question"]
            },
            "outputSchema": { "type": "object", "properties": {
                "nodes": { "type": "array", "items": { "type": "object", "properties": {
                    "label": {"type":"string"}, "file_type": {"type":"string"}, "source_file": {"type":"string"} } } },
                "edges": { "type": "array", "items": { "type": "object", "properties": {
                    "source": {"type":"string"}, "relation": {"type":"string"}, "target": {"type":"string"} } } }
            }, "required": ["nodes", "edges"] }
        },
        { "name": "get_node", "description": "Show one node's metadata (type, source file, community, degree). Use after query_graph to inspect a specific symbol.",
          "inputSchema": { "type": "object", "properties": { "label": { "type": "string", "description": "Node label, id, or bare name (e.g. 'login_user', 'AuthService'); resolved leniently." } }, "required": ["label"] } },
        { "name": "get_source", "description": "Return the actual source code for a symbol (the lines at its location), so you do not have to open the file. Use after query_graph or get_node to read a function or class body directly.",
          "inputSchema": { "type": "object", "properties": {
              "label": { "type": "string", "description": "Node label, id, or bare name; resolved leniently." },
              "context_lines": { "type": "integer", "description": "How many lines to return from the symbol start (default 40, max 400)." }
          }, "required": ["label"] } },
        { "name": "get_neighbors", "description": "List a node's directly connected nodes and the relation on each edge. Answers 'what does X call/use' and 'what calls X'.",
          "inputSchema": { "type": "object", "properties": { "label": { "type": "string", "description": "Node label, id, or bare name; resolved leniently." }, "relation_filter": { "type": "string", "description": "Optional: keep only this edge relation (substring match). Common relations: calls, imports, inherits, implements, references, contains, depends_on." } }, "required": ["label"] } },
        { "name": "get_community", "description": "List the members of a community: a cluster of densely-connected nodes, roughly a module or subsystem. Use to see what belongs together.",
          "inputSchema": { "type": "object", "properties": { "community_id": { "type": "integer", "description": "Community id, as reported by graph_stats, god_nodes, or a node's 'Community' field." } }, "required": ["community_id"] } },
        { "name": "god_nodes", "description": "The most-connected nodes ('god nodes' = high-degree hubs, the structurally central symbols). Use to orient in an unfamiliar codebase.",
          "inputSchema": { "type": "object", "properties": { "top_n": { "type": "integer", "description": "How many hubs to return (default is a small list)." } } },
          "outputSchema": { "type": "object", "properties": {
              "god_nodes": { "type": "array", "items": { "type": "object", "properties": {
                  "label": {"type":"string"}, "degree": {"type":"integer"}, "id": {"type":"string"} } } }
          }, "required": ["god_nodes"] } },
        { "name": "graph_stats", "description": "Graph size and health: node/edge/community counts and the EXTRACTED/INFERRED/AMBIGUOUS edge-confidence breakdown. Good first call to confirm a graph is loaded and how large it is.",
          "inputSchema": { "type": "object", "properties": {} },
          "outputSchema": { "type": "object", "properties": {
              "nodes": {"type":"integer"}, "edges": {"type":"integer"}, "communities": {"type":"integer"},
              "extracted": {"type":"integer"}, "inferred": {"type":"integer"}, "ambiguous": {"type":"integer"}
          }, "required": ["nodes","edges","communities"] } },
        { "name": "list_repos", "description": "For a federated (multi-repo) graph, list member repos (tags) with node/edge counts; empty for a single repo. Use before scoping a query to one repo.",
          "inputSchema": { "type": "object", "properties": {} } },
        { "name": "repo_stats", "description": "Node/edge counts for one federated member repo.",
          "inputSchema": { "type": "object", "properties": { "repo": { "type": "string", "description": "Repo tag, as listed by list_repos." } }, "required": ["repo"] } },
        { "name": "shortest_path", "description": "Shortest path between two nodes, showing the chain of relations. Answers 'how does A reach B' or 'is X connected to Y'.",
          "inputSchema": { "type": "object", "properties": { "source": { "type": "string", "description": "Start node: label, id, or bare name." }, "target": { "type": "string", "description": "End node: label, id, or bare name." }, "max_hops": { "type": "integer", "description": "Optional cap on path length (hops)." } }, "required": ["source", "target"] } },
        { "name": "affected", "description": "Reverse-impact: the nodes that transitively depend on a symbol, i.e. what could break if you change it. Walks calls/imports/inherits/uses edges backward. Answers 'what is the blast radius of changing X'.",
          "inputSchema": { "type": "object", "properties": {
              "label": { "type": "string", "description": "Node label, id, or bare name; resolved leniently." },
              "depth": { "type": "integer", "description": "Max hops to walk backward (default 3, max 16)." },
              "relations": { "type": "array", "items": { "type": "string" }, "description": "Optional edge relations to follow; defaults to the structural-impact set (calls, imports, inherits, implements, uses, references, depends_on, reads_from)." }
          }, "required": ["label"] },
          "outputSchema": { "type": "object", "properties": {
              "seed": {"type":"string"},
              "affected": { "type": "array", "items": { "type": "object", "properties": {
                  "label": {"type":"string"}, "depth": {"type":"integer"}, "via_relation": {"type":"string"} } } }
          }, "required": ["seed","affected"] } },
        { "name": "find_callers", "description": "List the nodes that call, use, or reference this symbol (incoming edges only). Answers 'who calls X'.",
          "inputSchema": { "type": "object", "properties": { "label": { "type": "string", "description": "Node label, id, or bare name; resolved leniently." } }, "required": ["label"] } },
        { "name": "find_callees", "description": "List the nodes this symbol calls, uses, or references (outgoing edges only). Answers 'what does X call'.",
          "inputSchema": { "type": "object", "properties": { "label": { "type": "string", "description": "Node label, id, or bare name; resolved leniently." } }, "required": ["label"] } },
        { "name": "list_prs", "description": "Open pull requests targeting the base branch with their CI/review state. Requires the `gh` CLI authenticated for the repo.",
          "inputSchema": { "type": "object", "properties": { "base": { "type": "string", "description": "Base branch to filter to (default: the repo's default branch)." }, "repo": { "type": "string", "description": "Target repo 'owner/name' (default: the current repo)." } } } },
        { "name": "get_pr_impact", "description": "One PR's detail plus its graph blast radius: which graph nodes and communities its changed files touch. Requires the `gh` CLI.",
          "inputSchema": { "type": "object", "properties": { "pr_number": { "type": "integer", "description": "PR number." }, "repo": { "type": "string", "description": "Target repo 'owner/name' (default: the current repo)." } }, "required": ["pr_number"] } },
        { "name": "triage_prs", "description": "Open PRs ranked by actionability (status plus graph blast radius) so the model can prioritize review and merge order. Requires the `gh` CLI.",
          "inputSchema": { "type": "object", "properties": { "base": { "type": "string", "description": "Base branch (default: the repo's default branch)." }, "repo": { "type": "string", "description": "Target repo 'owner/name' (default: the current repo)." } } } }
    ]);
    // Every tool is a pure read; the PR tools additionally reach the network
    // (gh), so they carry openWorldHint. Merged here to keep the entries above
    // focused on schema.
    let open_world = ["list_prs", "get_pr_impact", "triage_prs"];
    for t in tools.as_array_mut().unwrap() {
        let name = t["name"].as_str().unwrap_or("").to_string();
        t["annotations"] = json!({
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": open_world.contains(&name.as_str()),
        });
    }
    tools
}

/// The MCP `resources/list` payload.
fn resources_list() -> Value {
    json!([
        { "uri": "codegraph://report", "name": "Graph report", "mimeType": "text/markdown" },
        { "uri": "codegraph://stats", "name": "Graph stats", "mimeType": "text/plain" },
        { "uri": "codegraph://god-nodes", "name": "God nodes", "mimeType": "text/plain" },
        { "uri": "codegraph://surprises", "name": "Surprising connections", "mimeType": "text/plain" },
        { "uri": "codegraph://audit", "name": "Confidence audit", "mimeType": "text/plain" },
        { "uri": "codegraph://questions", "name": "Suggested questions", "mimeType": "text/plain" }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use codegraph_core::{Confidence, Edge, FileType};
    use serde_json::Map;

    fn node(id: &str, label: &str, community: Option<u32>) -> codegraph_core::Node {
        codegraph_core::Node {
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

    fn call_tool(s: &mut Server, name: &str, args: Value) -> String {
        let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}});
        let resp = s.handle_request(&req).unwrap();
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn every_tool_and_param_is_documented() {
        // Findings #1/#3: each tool needs a substantive description, and every
        // input-schema property needs its own description so agents use it right.
        let tools = tools_list();
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
        text.push_str(&tools_list().to_string());
        for t in [
            '\u{2014}', '\u{2013}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2192}',
        ] {
            assert!(!text.contains(t), "AI tell {t:?} in tool surface");
        }
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
    fn initialize_and_tools_list() {
        let mut s = server();
        let init = s
            .handle_request(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
            .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "codegraph");
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");

        let tl = s
            .handle_request(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .unwrap();
        let names: Vec<&str> = tl["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), 16);
        for expected in [
            "query_graph",
            "get_node",
            "get_source",
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
            "list_prs",
            "get_pr_impact",
            "triage_prs",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
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
        // Text content is still present for display.
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("3 nodes"));
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
        let tools = tools_list();
        for name in ["graph_stats", "query_graph", "affected", "god_nodes"] {
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
    fn initialize_echoes_supported_protocol_else_latest() {
        let mut s = server();
        // Client asks for a supported version -> echoed back.
        let r = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"protocolVersion":"2025-06-18"}
            }))
            .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");

        // Unknown version -> server returns its latest supported.
        let r = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":2,"method":"initialize",
                "params":{"protocolVersion":"1999-01-01"}
            }))
            .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
    }

    #[test]
    fn every_tool_is_annotated_read_only() {
        let tools = tools_list();
        for t in tools.as_array().unwrap() {
            let name = t["name"].as_str().unwrap();
            let ann = &t["annotations"];
            assert_eq!(
                ann["readOnlyHint"],
                json!(true),
                "tool {name} must be read-only"
            );
            // PR tools reach the network (gh) -> openWorldHint true.
            let open = matches!(name, "list_prs" | "get_pr_impact" | "triage_prs");
            assert_eq!(
                ann["openWorldHint"],
                json!(open),
                "tool {name} openWorldHint"
            );
        }
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
                &json!({"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"codegraph://stats"}}),
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
        assert_eq!(
            call_tool(&mut s, "god_nodes", json!({"top_n": 1})),
            "God nodes:\n  1. AuthService - 2 edges"
        );
        assert_eq!(
            call_tool(&mut s, "god_nodes", json!({"top_n": 3})),
            "God nodes:\n  1. AuthService - 2 edges\n  2. Database - 1 edges\n  3. login_user - 1 edges"
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
            gods.contains("Core - 3 edges") && !gods.contains("Alpha"),
            "god_nodes must reflect the reloaded graph: {gods}"
        );
        assert!(call_tool(&mut s, "graph_stats", json!({})).contains("4 nodes, 3 edges"));
    }
}
