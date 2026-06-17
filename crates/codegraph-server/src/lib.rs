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
// The `tools_list` JSON schema literal is large and deeply nested (tool input +
// output schemas); the default 128 macro-expansion depth is not enough.
#![recursion_limit = "256"]

mod http;
mod prompts;
pub mod session;
mod source;
pub use http::serve_http;
pub use session::{SessionStore, DEFAULT_SESSION_IDLE};

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use codegraph_core::{sanitize_label, sanitize_metadata_value, GraphData, NodeId};
use codegraph_graph::{
    god_nodes, graph_stats, suggest_questions, surprising_connections, GodNode, GraphStats,
    KnowledgeGraph,
};
use codegraph_predict::{assess_edit, forecast_changes_with_index, EditKind, ForecastOptions};
use codegraph_prs::{
    compute_pr_impact, detect_default_branch, fetch_pr, fetch_pr_files, fetch_prs, fetch_worktrees,
    format_pr_detail, format_prs_text, today_epoch_days, CommandRunner, ImpactIndex, PrInfo,
    Status, SystemCommands,
};
use codegraph_query::{
    affected_nodes, describe_node, explain, resolve_seed, shortest_path, QueryIndex,
    ReverseImpactIndex, TraversalMode, DEFAULT_AFFECTED_RELATIONS,
};
use codegraph_sandbox::{
    render_markdown as render_speculate_md, speculate, Change, SpeculateOptions,
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
    /// Reverse-impact adjacency over `DEFAULT_AFFECTED_RELATIONS`, built once at
    /// load/reload so the predict tools (`predict_impact`, `affected_tests`,
    /// `speculate`) walk the blast radius without rebuilding it per request.
    affected_index: ReverseImpactIndex,
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
    /// Whether the command-running `speculate` tool is exposed. OFF by default so
    /// the server stays strictly read-only; enabled only by an explicit operator
    /// opt-in (`serve --allow-exec`). When off, `speculate` is neither advertised
    /// in tools/list nor runnable.
    allow_exec: bool,
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
        let affected_index = ReverseImpactIndex::build(&kg, DEFAULT_AFFECTED_RELATIONS);
        let stats = graph_stats(&kg);
        let god_nodes_all = god_nodes(&kg, usize::MAX);
        let reload_key = graph_path.as_deref().and_then(reload_key_for);
        Server {
            kg,
            communities,
            query_index,
            affected_index,
            stats,
            god_nodes_all,
            graph_path,
            reload_key,
            runner: Box::new(SystemCommands),
            log_path: query_log_path(),
            source_root: None,
            allow_exec: false,
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

    /// Opt in to the command-running `speculate` tool. OFF by default; turning it
    /// on means the server can execute the project's test/build commands in a
    /// throwaway worktree, which is no longer read-only -- the caller is asserting
    /// that is acceptable for this deployment.
    pub fn with_allow_exec(mut self, allow: bool) -> Server {
        self.allow_exec = allow;
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
                self.affected_index =
                    ReverseImpactIndex::build(&self.kg, DEFAULT_AFFECTED_RELATIONS);
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

    /// `describe_node` — a compact "takes X, returns Y, calls Z" description of a
    /// symbol from its captured signature and outgoing call edges (graph-only, no
    /// source read). Built for feeding tool/function description generation.
    pub fn tool_describe_node(&self, label: &str) -> String {
        let not_found = || format!("No node matches '{}'.", sanitize_label(label));
        let Some(id) = resolve_seed(&self.kg, label) else {
            return not_found();
        };
        let Some(d) = describe_node(&self.kg, &id) else {
            return not_found();
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
        out
    }

    /// Typed mirror of [`tool_describe_node`](Server::tool_describe_node).
    fn describe_node_json(&self, label: &str) -> Value {
        let Some(d) = resolve_seed(&self.kg, label).and_then(|i| describe_node(&self.kg, &i))
        else {
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
            let raw = serde_json::to_value(sig).unwrap_or(Value::Null);
            obj.insert("signature".into(), sanitize_metadata_value(&raw));
        }
        obj.insert(
            "callees".into(),
            Value::Array(d.callees.iter().map(|c| json!(sanitize_label(c))).collect()),
        );
        Value::Object(obj)
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
    /// `offset == 0` keeps the historical output byte-identical.
    pub fn tool_god_nodes(&self, top_n: usize, offset: usize) -> String {
        // Slice from the precomputed ranked list (H3). `god_nodes` returns one
        // node even for top_n == 0 (push-then-check the cap), so mirror that with
        // `max(1)` to stay byte-identical to the old per-call path.
        let total = self.god_nodes_all.len();
        let start = offset.min(total);
        let end = start.saturating_add(top_n.max(1)).min(total);
        let gods = &self.god_nodes_all[start..end];
        if gods.is_empty() {
            return "No nodes.".to_string();
        }
        let mut out = String::from("God nodes:");
        for (i, g) in gods.iter().enumerate() {
            out.push_str(&format!(
                "\n  {}. {} - {} edges",
                start + i + 1,
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

    /// `working_changes_impact` — graph blast radius of the working-tree diff
    /// against `base` (default: the detected default branch). `git diff <base>`
    /// covers the branch's committed work plus uncommitted edits, the same set a
    /// PR would. Uses `git`, not `gh`, so it works offline and before any PR.
    pub fn tool_working_changes_impact(&self, base: Option<&str>) -> String {
        let base = self.resolve_base(base, None);
        let diff = self.runner.run("git", &["diff", "--name-only", &base]);
        let files: Vec<String> = diff
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        if files.is_empty() {
            return format!("No changes vs {base} (or git unavailable).");
        }
        let (comms, nodes) = self.graph_impact(&files);
        let mut out = format!(
            "Working changes vs {base}: {} files, {nodes} graph nodes, {} communities touched",
            files.len(),
            comms.len()
        );
        for f in &files {
            out.push_str(&format!("\n  {}", sanitize_label(f)));
        }
        out
    }

    /// `predict_impact` - forecast the consequences of a change before applying
    /// it. Given `files` (or the working-tree diff vs `base`), maps them to graph
    /// nodes, walks the reverse-impact blast radius, and flags public APIs at
    /// risk plus a verify checklist. Pure-graph and read-only; use
    /// `time_travel_diff` or the `codegraph predict` CLI for cycle / removed-API
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

    pub fn tool_predict_impact(
        &self,
        files: &[String],
        base: Option<&str>,
        depth: usize,
    ) -> String {
        let changed = self.changed_from_args(files, base);
        if changed.is_empty() {
            return "No changed files to forecast (pass `files`, or run on a branch with a diff vs the base)."
                .to_string();
        }
        let opts = ForecastOptions {
            depth: depth.clamp(1, 16),
            ..Default::default()
        };
        let f = forecast_changes_with_index(&self.kg, &self.affected_index, &changed, &opts);
        let mut out = format!("Forecast: {}", sanitize_label(&f.summary));
        if let Some(r) = &f.risk {
            out.push_str(&format!("\nChange risk: {} ({}/100)", r.level, r.score));
            for factor in &r.factors {
                out.push_str(&format!("\n  - {}", sanitize_label(factor)));
            }
        }
        if !f.changed_nodes.is_empty() {
            out.push_str(&format!("\nChanged nodes ({}):", f.changed_nodes.len()));
            for n in &f.changed_nodes {
                out.push_str(&format!(
                    "\n  {} ({})",
                    sanitize_label(&n.label),
                    sanitize_label(&n.file)
                ));
            }
        }
        if !f.public_api_breaks.is_empty() {
            out.push_str(&format!(
                "\nPublic API at risk ({}):",
                f.public_api_breaks.len()
            ));
            for n in &f.public_api_breaks {
                out.push_str(&format!(
                    "\n  {} ({})",
                    sanitize_label(&n.label),
                    sanitize_label(&n.file)
                ));
            }
        }
        if !f.at_risk_tests.is_empty() {
            out.push_str(&format!("\nTests at risk ({}):", f.at_risk_tests.len()));
            for h in &f.at_risk_tests {
                out.push_str(&format!(
                    "\n  {} ({})",
                    sanitize_label(&h.label),
                    sanitize_label(&h.file)
                ));
            }
        }
        out.push_str(&format!(
            "\nBlast radius ({} at-risk dependent(s)):",
            f.blast_radius.len()
        ));
        for h in &f.blast_radius {
            out.push_str(&format!(
                "\n  [{}h via {}] {} ({})",
                h.depth,
                sanitize_label(&h.via_relation),
                sanitize_label(&h.label),
                sanitize_label(&h.file)
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
        let changed = self.changed_from_args(files, base);
        if changed.is_empty() {
            return "No changed files (pass `files`, or run on a branch with a diff vs the base)."
                .to_string();
        }
        let opts = ForecastOptions {
            depth: depth.clamp(1, 16),
            ..Default::default()
        };
        let f = forecast_changes_with_index(&self.kg, &self.affected_index, &changed, &opts);
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
    pub fn tool_predict_edit(&self, symbol: &str, kind: &str, depth: usize) -> String {
        let Some(kind_enum) = EditKind::parse(kind) else {
            return format!(
                "Unknown edit kind '{}'. Use: delete, signature, visibility.",
                sanitize_label(kind)
            );
        };
        let Some(impact) = assess_edit(&self.kg, symbol, kind_enum, depth.clamp(1, 16)) else {
            return format!(
                "No unique node matches '{}'. If the name is shared by several files, qualify it as 'name@file-substring' (e.g. 'announce@core/foo.ts'), or pass a node id.",
                sanitize_label(symbol)
            );
        };
        let line = |d: &codegraph_predict::EditDependent| {
            format!(
                "\n  [{}h via {}] {} ({}) - {}",
                d.depth,
                sanitize_label(&d.via_relation),
                sanitize_label(&d.label),
                sanitize_label(&d.file),
                sanitize_label(&d.reason)
            )
        };
        let mut out = sanitize_label(&impact.summary);
        if !impact.breaks.is_empty() {
            out.push_str(&format!("\nWill break ({}):", impact.breaks.len()));
            for d in &impact.breaks {
                out.push_str(&line(d));
            }
        }
        if !impact.review.is_empty() {
            out.push_str(&format!("\nReview ({}):", impact.review.len()));
            for d in &impact.review {
                out.push_str(&line(d));
            }
        }
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

    fn god_nodes_json(&self, top_n: usize, offset: usize) -> Value {
        let total = self.god_nodes_all.len();
        let start = offset.min(total);
        let end = start.saturating_add(top_n.max(1)).min(total);
        let arr: Vec<Value> = self.god_nodes_all[start..end]
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

    /// Typed mirror of [`tool_structural_search`](Server::tool_structural_search):
    /// runs the same CGQL query / pattern and returns structured rows of resolved
    /// node views (label, kind, visibility, file, and the captured signature) so
    /// an agent can route on a function's shape without reading source. Aggregate
    /// queries return `groups` of scalar cells instead.
    fn structural_search_json(
        &self,
        query: Option<&str>,
        pattern: Option<&str>,
        limit: usize,
    ) -> Value {
        let result = if let Some(p) = pattern {
            codegraph_cgql::patterns::run_pattern(&self.kg, p)
        } else if let Some(q) = query {
            codegraph_cgql::run(&self.kg, q)
        } else {
            return json!({ "error": "Provide a CGQL query or a pattern name.", "results": [] });
        };
        let r = match result {
            Ok(r) => r,
            Err(e) => return json!({ "error": format!("search error: {e}"), "results": [] }),
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
                    "capabilities": {
                        "tools": {},
                        "resources": { "subscribe": true },
                        "prompts": {},
                        "completions": {},
                        "logging": {}
                    },
                    "serverInfo": { "name": "codegraph", "version": env!("CARGO_PKG_VERSION") },
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

    /// Repo root for tools that shell out (diff) or read source (plan_rename):
    /// the configured source root, else the current directory.
    fn repo_root(&self) -> std::path::PathBuf {
        self.source_root
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    }

    /// Structural search via CGQL (or a named pattern) over the loaded graph.
    pub fn tool_structural_search(
        &self,
        query: Option<&str>,
        pattern: Option<&str>,
        limit: usize,
    ) -> String {
        let result = if let Some(p) = pattern {
            codegraph_cgql::patterns::run_pattern(&self.kg, p)
        } else if let Some(q) = query {
            codegraph_cgql::run(&self.kg, q)
        } else {
            return "Provide a CGQL query or a pattern name.".to_string();
        };
        let r = match result {
            Ok(r) => r,
            Err(e) => return format!("search error: {e}"),
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
        let opts = codegraph_history::DiffOptions {
            top,
            ..Default::default()
        };
        let r = match codegraph_history::diff(&self.repo_root(), rev1, rev2, &opts) {
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
            o.push_str(&format!(
                "  hotspot {} (+{}/-{} lines)\n",
                h.file, h.lines_added, h.lines_removed
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
    ) -> String {
        // `name` may be a node id; pin it only when --id is not given.
        let (old, opt_id) = match (id, self.kg.node(&codegraph_core::NodeId(name.to_string()))) {
            (Some(_), _) => (name.to_string(), id.map(str::to_string)),
            (None, Some(n)) => (n.label.clone(), Some(n.id.0.clone())),
            (None, None) => (name.to_string(), None),
        };
        let opts = codegraph_refactor::RenameOptions {
            id: opt_id,
            file: file.map(str::to_string),
            // Reading every indexed file is too heavy for an MCP call; the CLI does it.
            scan_text: false,
            ..Default::default()
        };
        let plan =
            match codegraph_refactor::plan_rename(&self.kg, &old, to, &self.repo_root(), &opts) {
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
        o.push_str("\n  (plan-only; CodeGraph did not edit source)");
        o
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

        let text = match name {
            "get_node" => self.tool_get_node(&s("label")),
            "get_source" => self.tool_get_source(&s("label"), u("context_lines", 40) as usize),
            "get_neighbors" => {
                let rf = args.get("relation_filter").and_then(Value::as_str);
                self.tool_get_neighbors(&s("label"), rf)
            }
            "get_community" => self.tool_get_community(
                u("community_id", 0) as u32,
                u("offset", 0) as usize,
                u("limit", 100) as usize,
            ),
            "god_nodes" => self.tool_god_nodes(u("top_n", 10) as usize, u("offset", 0) as usize),
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
            "working_changes_impact" => self.tool_working_changes_impact(opt("base")),
            "predict_impact" => {
                let files: Vec<String> = args
                    .get("files")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                self.tool_predict_impact(&files, opt("base"), u("depth", 3) as usize)
            }
            "affected_tests" => {
                let files: Vec<String> = args
                    .get("files")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                self.tool_affected_tests(&files, opt("base"), u("depth", 3) as usize)
            }
            "predict_edit" => {
                self.tool_predict_edit(&s("symbol"), &s("kind"), u("depth", 3) as usize)
            }
            "structural_search" => {
                self.tool_structural_search(opt("query"), opt("pattern"), u("limit", 50) as usize)
            }
            "describe_node" => self.tool_describe_node(&s("label")),
            "time_travel_diff" => {
                self.tool_time_travel_diff(&s("rev1"), opt("rev2"), u("top", 20) as usize)
            }
            "plan_rename" => self.tool_plan_rename(&s("name"), &s("to"), opt("id"), opt("file")),
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
            "god_nodes" => {
                Some(self.god_nodes_json(u("top_n", 10) as usize, u("offset", 0) as usize))
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
                Some(self.affected_json(&s("label"), u("depth", 3) as usize, &rels))
            }
            "structural_search" => Some(self.structural_search_json(
                opt("query"),
                opt("pattern"),
                u("limit", 50) as usize,
            )),
            "describe_node" => Some(self.describe_node_json(&s("label"))),
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
        // Templated resources (resources/templates/list): any node or community
        // is addressable by URI. Checked before the static table; the static
        // URIs (codegraph://god-nodes etc.) do not share these prefixes.
        if let Some(label) = uri.strip_prefix("codegraph://node/") {
            let text = self.tool_get_node(label);
            return Ok(
                json!({ "contents": [{ "uri": uri, "mimeType": "text/plain", "text": text }] }),
            );
        }
        if let Some(id) = uri.strip_prefix("codegraph://community/") {
            let cid: u32 = id.parse().unwrap_or(u32::MAX);
            let text = self.tool_get_community(cid, 0, 1000);
            return Ok(
                json!({ "contents": [{ "uri": uri, "mimeType": "text/plain", "text": text }] }),
            );
        }
        let (mime, text) = match uri {
            "codegraph://report" => ("text/markdown", self.resource_report()),
            "codegraph://stats" => ("text/plain", self.tool_graph_stats()),
            "codegraph://god-nodes" => ("text/plain", self.tool_god_nodes(10, 0)),
            "codegraph://surprises" => ("text/plain", self.resource_surprises()),
            "codegraph://audit" => ("text/plain", self.resource_audit()),
            "codegraph://questions" => ("text/plain", self.resource_questions()),
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

/// Serialize a resolved [`NodeView`](codegraph_cgql::NodeView) for structured
/// tool output. Free-text fields (label/id/file, and the signature, which is
/// source-derived) are sanitized; `kind`/`visibility` come from fixed enums.
fn node_view_to_json(v: &codegraph_cgql::NodeView) -> Value {
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
        // The signature is source-derived text; sanitize it like other metadata.
        let raw = serde_json::to_value(sig).unwrap_or(Value::Null);
        obj.insert("signature".into(), sanitize_metadata_value(&raw));
    }
    Value::Object(obj)
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
    let kept = bpe
        .decode(toks[..token_budget].to_vec())
        .unwrap_or_default();
    format!("{kept}\n... (truncated to ~{token_budget} tokens)")
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
(returns a relevant subgraph), then get_source to read a symbol's actual code, \
get_neighbors / find_callers / find_callees / shortest_path to navigate, and get_node \
for detail. For change impact, affected gives the blast radius of editing a symbol and \
working_changes_impact does the same for your current git diff. For a multi-repo graph, \
call list_repos then pass the repo argument to scope. The PR tools (list_prs / \
get_pr_impact / triage_prs) need the `gh` CLI. To see how the architecture changed \
over time, run the CLI (not exposed as a tool here): `codegraph diff <rev1> [rev2]` \
reports new/removed dependencies, removed APIs, drift, new cycles, and hotspots \
between two git revisions.\n\
\n\
Terms: a 'god node' is a high-degree hub (structurally central); a 'community' is a \
cluster of densely-connected nodes (roughly a module); edge confidence is EXTRACTED \
(observed in code), INFERRED, or AMBIGUOUS.";

/// The MCP `tools/list` payload. Descriptions and per-parameter docs make the
/// implicit domain knowledge explicit so an agent uses each tool correctly
/// (graph jargon, the lenient label resolution, the relation vocabulary).
fn tools_list(allow_exec: bool) -> Value {
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
        { "name": "get_node", "description": "Show one node's metadata: type, source file, community, degree, plus kind (class/function/method/etc.), visibility, and LOC when available. Use after query_graph to inspect a specific symbol.",
          "inputSchema": { "type": "object", "properties": { "label": { "type": "string", "description": "Node label, id, or bare name (e.g. 'login_user', 'AuthService'); resolved leniently." } }, "required": ["label"] } },
        { "name": "get_source", "description": "Return the actual source code for a symbol (the lines at its location), so you do not have to open the file. Use after query_graph or get_node to read a function or class body directly.",
          "inputSchema": { "type": "object", "properties": {
              "label": { "type": "string", "description": "Node label, id, or bare name; resolved leniently." },
              "context_lines": { "type": "integer", "description": "How many lines to return from the symbol start (default 40, max 400)." }
          }, "required": ["label"] } },
        { "name": "get_neighbors", "description": "List a node's directly connected nodes and the relation on each edge. Answers 'what does X call/use' and 'what calls X'.",
          "inputSchema": { "type": "object", "properties": { "label": { "type": "string", "description": "Node label, id, or bare name; resolved leniently." }, "relation_filter": { "type": "string", "description": "Optional: keep only this edge relation (substring match). Common relations: calls, imports, inherits, implements, references, contains, depends_on." } }, "required": ["label"] } },
        { "name": "get_community", "description": "List the members of a community: a cluster of densely-connected nodes, roughly a module or subsystem. Use to see what belongs together. Paginates: a large community returns one page at a time.",
          "inputSchema": { "type": "object", "properties": {
              "community_id": { "type": "integer", "description": "Community id, as reported by graph_stats, god_nodes, or a node's 'Community' field." },
              "offset": { "type": "integer", "description": "Members to skip before the page (default 0). Raise it to page through a large community." },
              "limit": { "type": "integer", "description": "Max members to return in this page (default 100)." }
          }, "required": ["community_id"] } },
        { "name": "god_nodes", "description": "The most-connected nodes ('god nodes' = high-degree hubs, the structurally central symbols). Use to orient in an unfamiliar codebase.",
          "inputSchema": { "type": "object", "properties": {
              "top_n": { "type": "integer", "description": "How many hubs to return (default is a small list)." },
              "offset": { "type": "integer", "description": "Hubs to skip before the page (default 0), for paging past the top ranks." }
          } },
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
        { "name": "affected", "description": "Reverse-impact: the nodes that transitively depend on a symbol, i.e. what could break if you change it. Walks calls/imports/inheritance edges plus cross-language coupling (subprocess `invokes`, FFI `binds_native`, HTTP/gRPC `calls_service`/`handled_by`) backward, so the blast radius spans language boundaries. Answers 'what is the blast radius of changing X'.",
          "inputSchema": { "type": "object", "properties": {
              "label": { "type": "string", "description": "Node label, id, or bare name; resolved leniently." },
              "depth": { "type": "integer", "description": "Max hops to walk backward (default 3, max 16)." },
              "relations": { "type": "array", "items": { "type": "string" }, "description": "Optional edge relations to follow; defaults to the structural-impact set: calls, references, imports, imports_from, re_exports, inherits, extends, implements, uses, mixes_in, embeds, depends_on, reads_from." }
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
          "inputSchema": { "type": "object", "properties": { "base": { "type": "string", "description": "Base branch (default: the repo's default branch)." }, "repo": { "type": "string", "description": "Target repo 'owner/name' (default: the current repo)." } } } },
        { "name": "working_changes_impact", "description": "Graph blast radius of your branch's changes against a base branch (committed plus uncommitted, the same set a PR would have): which graph nodes and communities they touch, before opening a PR. Uses git, no gh needed.",
          "inputSchema": { "type": "object", "properties": { "base": { "type": "string", "description": "Base branch to diff against (default: the repo's default branch)." } } } },
        { "name": "structural_search", "description": "Structural search over the graph with CGQL, or a named architectural pattern. Not text search: matches on kind/visibility/loc/fan-in/out/etc. `.name` is the bare symbol (no parentheses); use `=~` for a regex/substring match. Example query: 'MATCH (c:class) WHERE c.loc > 500 RETURN c'. Example name match: 'MATCH (f:function) WHERE f.name =~ \"announce\" RETURN f'. Patterns: singleton, factory, observer, service-locator, god-class.",
          "inputSchema": { "type": "object", "properties": {
              "query": { "type": "string", "description": "A CGQL query. Omit when using `pattern`." },
              "pattern": { "type": "string", "description": "A built-in pattern name instead of a query." },
              "limit": { "type": "integer", "description": "Max rows to return (default 50)." }
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
        { "name": "describe_node", "description": "Compact 'takes X, returns Y, calls Z' description of a symbol, composed from its captured signature and outgoing call edges (graph-only, no source read). Useful for generating tool/function descriptions or quickly understanding a function's shape. Resolve `label` by bare name, full label, id, or file.",
          "inputSchema": { "type": "object", "properties": {
              "label": { "type": "string", "description": "Symbol to describe (bare name, label, node id, or source file)." }
          }, "required": ["label"] },
          "outputSchema": { "type": "object", "properties": {
              "found": { "type": "boolean" },
              "id": { "type": "string" },
              "label": { "type": "string" },
              "kind": { "type": "string" },
              "summary": { "type": "string", "description": "The one-line 'takes X, returns Y, calls Z' description." },
              "callees": { "type": "array", "items": { "type": "string" }, "description": "Distinct outgoing call-target labels." },
              "signature": { "type": "object", "description": "Captured signature: params (name + optional type_ref), optional return_type, raw header." }
          }, "required": ["found"] } },
        { "name": "time_travel_diff", "description": "How the code graph changed between two git revisions: added/removed module dependencies, removed APIs, architectural drift, new cycles, and hotspots. Builds each revision in a throwaway git worktree (slow on a cold repo).",
          "inputSchema": { "type": "object", "properties": {
              "rev1": { "type": "string", "description": "Base revision (e.g. HEAD~10, a branch, or a SHA)." },
              "rev2": { "type": "string", "description": "Target revision (default: the current working tree)." },
              "top": { "type": "integer", "description": "Max rows per ranked section (default 20)." }
          }, "required": ["rev1"] } },
        { "name": "plan_rename", "description": "Plan-only: a confidence-scored rename plan (edit sites, blast radius, collision check) for an agent to apply. Never edits source. Use `codegraph refactor verify` on the CLI after applying.",
          "inputSchema": { "type": "object", "properties": {
              "name": { "type": "string", "description": "The symbol to rename (its name, or a node id)." },
              "to": { "type": "string", "description": "The new name." },
              "id": { "type": "string", "description": "Disambiguate by node id when several definitions share the name." },
              "file": { "type": "string", "description": "Disambiguate by file-path substring." }
          }, "required": ["name", "to"] } },
        { "name": "predict_impact", "description": "Forecast the consequences of a change BEFORE editing: which graph nodes the changed files define, the reverse-impact blast radius that depends on them, which edited symbols are public API (callers outside the file/module may break), and a verify checklist. Pure-graph and read-only; use time_travel_diff or the `codegraph predict` CLI for new-cycle / removed-API detection.",
          "inputSchema": { "type": "object", "properties": {
              "files": { "type": "array", "items": { "type": "string" }, "description": "Repo-relative changed files to forecast. Omit to use the working-tree diff vs `base`." },
              "base": { "type": "string", "description": "Base branch to diff against when `files` is omitted (default: the repo's default branch)." },
              "depth": { "type": "integer", "description": "Reverse-impact hop bound (default 3, max 16)." }
          } } },
        { "name": "affected_tests", "description": "Predictive test selection: the tests that exercise the changed code, found by walking the reverse-impact set from the changed files and keeping the test nodes (detected by path convention). The focused 'which tests should I run for this change' view.",
          "inputSchema": { "type": "object", "properties": {
              "files": { "type": "array", "items": { "type": "string" }, "description": "Repo-relative changed files. Omit to use the working-tree diff vs `base`." },
              "base": { "type": "string", "description": "Base branch to diff against when `files` is omitted (default: the repo's default branch)." },
              "depth": { "type": "integer", "description": "Reverse-impact hop bound (default 3, max 16)." }
          } } },
        { "name": "predict_edit", "description": "What breaks if you make a specific kind of edit to a symbol, classified into 'will break' vs 'to review'. kind=delete (every dependent breaks), signature (callers/type-users break, bare imports go to review), or visibility (references from other files break when narrowing to private). Pure-graph; complements plan_rename (which is rename-only).",
          "inputSchema": { "type": "object", "properties": {
              "symbol": { "type": "string", "description": "The symbol to edit: its name, bare name, or a node id. If the name is shared by several files, qualify it as 'name@file-substring' (e.g. 'announce@core/foo.ts')." },
              "kind": { "type": "string", "description": "The edit kind: delete, signature, or visibility." },
              "depth": { "type": "integer", "description": "Reverse-impact hop bound (default 3, max 16)." }
          }, "required": ["symbol", "kind"] } }
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
        { "uri": "codegraph://report", "name": "Graph report", "mimeType": "text/markdown" },
        { "uri": "codegraph://stats", "name": "Graph stats", "mimeType": "text/plain" },
        { "uri": "codegraph://god-nodes", "name": "God nodes", "mimeType": "text/plain" },
        { "uri": "codegraph://surprises", "name": "Surprising connections", "mimeType": "text/plain" },
        { "uri": "codegraph://audit", "name": "Confidence audit", "mimeType": "text/plain" },
        { "uri": "codegraph://questions", "name": "Suggested questions", "mimeType": "text/plain" }
    ])
}

/// The MCP `resources/templates/list` payload: any node or community is
/// addressable as a resource by URI.
fn resource_templates() -> Value {
    json!([
        { "uriTemplate": "codegraph://node/{label}", "name": "Node", "mimeType": "text/plain",
          "description": "Metadata for one node by label, id, or bare name." },
        { "uriTemplate": "codegraph://community/{id}", "name": "Community", "mimeType": "text/plain",
          "description": "Members of one community by id." }
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
    fn structural_search_returns_structured_signature() {
        use codegraph_core::{NodeKind, Param, Signature};
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
        use codegraph_core::{NodeKind, Param, Signature};
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
        svc.set_visibility(codegraph_core::Visibility::Public);
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
        assert!(miss.contains("No unique node matches"), "{miss}");
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
        assert_eq!(names.len(), 24);
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
            "working_changes_impact",
            "structural_search",
            "describe_node",
            "time_travel_diff",
            "plan_rename",
            "predict_impact",
            "affected_tests",
            "predict_edit",
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
        let mut s = server(); // AuthService(2), Database(1), login_user(1)
                              // offset 0 stays byte-identical to the historical output.
        assert_eq!(
            call_tool(&mut s, "god_nodes", json!({"top_n": 1})),
            "God nodes:\n  1. AuthService - 2 edges"
        );
        // offset 1 skips the top hub and numbers from its absolute rank.
        let paged = call_tool(&mut s, "god_nodes", json!({"top_n": 1, "offset": 1}));
        assert_eq!(paged, "God nodes:\n  2. Database - 1 edges");
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
                "params":{ "ref": {"type":"ref/resource","uri":"codegraph://node/{label}"},
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
                "params":{"uri":"codegraph://stats"}
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
            .any(|t| t["uriTemplate"] == "codegraph://node/{label}"));

        let read = s
            .handle_request(&json!({
                "jsonrpc":"2.0","id":2,"method":"resources/read",
                "params":{"uri":"codegraph://node/AuthService"}
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
                "params":{"uri":"codegraph://god-nodes"}
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
        let tools = tools_list(false);
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
    fn working_changes_impact_uses_git_diff() {
        struct GitRunner;
        impl CommandRunner for GitRunner {
            fn run(&self, program: &str, args: &[&str]) -> Option<String> {
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
        let out = s.tool_working_changes_impact(Some("main"));
        assert!(out.contains("auth.py"), "names the changed file: {out}");
        assert!(
            out.contains("1 communities touched"),
            "reports impact: {out}"
        );

        // No diff -> graceful message, no panic.
        struct EmptyRunner;
        impl CommandRunner for EmptyRunner {
            fn run(&self, _p: &str, _a: &[&str]) -> Option<String> {
                Some(String::new())
            }
        }
        let s2 = server().with_runner(Box::new(EmptyRunner));
        assert!(s2
            .tool_working_changes_impact(Some("main"))
            .contains("No changes"));
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
