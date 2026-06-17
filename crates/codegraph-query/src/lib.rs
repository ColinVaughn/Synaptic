//! Graph query for CodeGraph: IDF-scored subgraph retrieval, shortest path,
//! node explanation, and reverse-impact ("affected"). Shared by the CLI and
//! (later) the MCP/REST server.
//!
//! MVP note: subgraph size is bounded by a node count, not a token budget;
//! true token-budgeting (tiktoken) is deferred (§2.9).
#![forbid(unsafe_code)]

pub mod describe;

use std::collections::{HashMap, HashSet, VecDeque};

use codegraph_core::NodeId;
use codegraph_graph::KnowledgeGraph;
use serde::Serialize;

pub use describe::{describe_node, NodeDescription};

/// Result of a text query: matched seeds plus the surrounding subgraph.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct QueryResult {
    pub seeds: Vec<NodeId>,
    pub nodes: Vec<NodeId>,
    pub edges: Vec<EdgeRef>,
}

/// A node with its display label.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EdgeRef {
    pub source: NodeId,
    pub target: NodeId,
    pub relation: String,
}

/// A neighbour in an `explain` result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Neighbor {
    pub id: NodeId,
    pub label: String,
    pub relation: String,
    /// "out" = this node → neighbour; "in" = neighbour → this node.
    pub direction: &'static str,
}

/// Result of `explain`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Explain {
    pub id: NodeId,
    pub label: String,
    pub source_file: String,
    pub community: Option<u32>,
    pub neighbors: Vec<Neighbor>,
}

/// Edge relations that propagate "impact" backward in [`affected_nodes`] — a
/// change to a node affects whatever depends on it through one of these.
/// Structural relations only; e.g.
/// `contains`/`method` (containment) are intentionally excluded.
pub const DEFAULT_AFFECTED_RELATIONS: &[&str] = &[
    "calls",
    "references",
    "imports",
    "imports_from",
    "re_exports",
    "inherits",
    "extends",
    "implements",
    "uses",
    "mixes_in",
    "embeds",
    // Data/infra relations (data/infra language extractors): a block/service's
    // declared dependency, and a view/query reading from a table, both propagate
    // impact in reverse (change the dependency, the dependent is affected).
    "depends_on",
    "reads_from",
    // Cross-language relations (the `cross-language` post-passes): a subprocess
    // invocation, an FFI binding, and an HTTP client->route->handler chain all
    // point dependent->dependency, so reverse-impact crosses the boundary.
    "invokes",
    "binds_native",
    "calls_service",
    "handled_by",
];

/// One node reached by the reverse-impact walk: which node, how many hops from
/// the seed, and the relation it was reached through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AffectedHit {
    pub node_id: NodeId,
    pub depth: usize,
    pub via_relation: String,
}

/// Split a label into lowercased word tokens (snake_case and camelCase aware).
fn tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            if ch.is_uppercase() && prev_lower && !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            cur.extend(ch.to_lowercase());
            prev_lower = ch.is_lowercase();
        } else {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens.retain(|t| t.len() >= 2);
    tokens
}

fn undirected_adjacency(kg: &KnowledgeGraph) -> HashMap<NodeId, Vec<NodeId>> {
    let mut adj: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for n in kg.nodes() {
        adj.entry(n.id.clone()).or_default();
    }
    for e in kg.edges() {
        if e.source == e.target {
            continue;
        }
        adj.entry(e.source.clone())
            .or_default()
            .push(e.target.clone());
        adj.entry(e.target.clone())
            .or_default()
            .push(e.source.clone());
    }
    for v in adj.values_mut() {
        v.sort();
        v.dedup();
    }
    adj
}

/// Graph traversal order for subgraph expansion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TraversalMode {
    /// Breadth-first (default) — expands by distance from the seeds.
    #[default]
    Bfs,
    /// Depth-first — follows a chain before fanning out.
    Dfs,
}

/// Retrieve a subgraph relevant to `query_text`: IDF-score nodes by label-token
/// overlap, pick the top seeds, then BFS-expand up to `max_nodes`.
pub fn query(kg: &KnowledgeGraph, query_text: &str, max_nodes: usize) -> QueryResult {
    query_modal(kg, query_text, max_nodes, TraversalMode::Bfs)
}

/// Precomputed, **query-independent** index for [`query_modal`]: per-node label
/// tokens, their document frequencies, and the undirected adjacency. Building it
/// is O(nodes·label + edges); the per-query [`query`](QueryIndex::query) then
/// only scores and expands. Build it **once** and reuse it across many queries —
/// the MCP server does this at graph load/reload instead of rebuilding the index
/// on every request (H1).
pub struct QueryIndex {
    /// `node_count().max(1)` as the IDF denominator base.
    n: f64,
    /// Each node's set of label tokens.
    node_tokens: HashMap<NodeId, HashSet<String>>,
    /// How many nodes contain each token (document frequency).
    df: HashMap<String, usize>,
    /// Undirected adjacency (sorted, deduped) for subgraph expansion.
    adjacency: HashMap<NodeId, Vec<NodeId>>,
}

impl QueryIndex {
    /// Build the index from a graph (the query-independent work).
    pub fn build(kg: &KnowledgeGraph) -> Self {
        let n = kg.node_count().max(1) as f64;
        let mut node_tokens: HashMap<NodeId, HashSet<String>> = HashMap::new();
        let mut df: HashMap<String, usize> = HashMap::new();
        for node in kg.nodes() {
            let toks: HashSet<String> = tokenize(&node.label).into_iter().collect();
            for t in &toks {
                *df.entry(t.clone()).or_insert(0) += 1;
            }
            node_tokens.insert(node.id.clone(), toks);
        }
        let adjacency = undirected_adjacency(kg);
        QueryIndex {
            n,
            node_tokens,
            df,
            adjacency,
        }
    }

    /// Retrieve a subgraph relevant to `query_text` using the precomputed index:
    /// IDF-score nodes by label-token overlap, pick the top seeds, then
    /// BFS/DFS-expand up to `max_nodes`. `kg` is still needed to collect the
    /// result edges (with their relations).
    pub fn query(
        &self,
        kg: &KnowledgeGraph,
        query_text: &str,
        max_nodes: usize,
        mode: TraversalMode,
    ) -> QueryResult {
        let idf =
            |t: &str| ((self.n + 1.0) / (1.0 + *self.df.get(t).unwrap_or(&0) as f64)).ln() + 1.0;

        let q_tokens: HashSet<String> = tokenize(query_text).into_iter().collect();

        // Score and rank seeds.
        let mut scored: Vec<(NodeId, f64)> = Vec::new();
        for (id, toks) in &self.node_tokens {
            let score: f64 = q_tokens
                .iter()
                .filter(|t| toks.contains(*t))
                .map(|t| idf(t))
                .sum();
            if score > 0.0 {
                scored.push((id.clone(), score));
            }
        }
        // Highest score first; tie-break by id for determinism.
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        let seeds: Vec<NodeId> = scored.iter().take(8).map(|(id, _)| id.clone()).collect();

        // Expand from seeds up to max_nodes (BFS pops the front, DFS the back).
        let mut included: Vec<NodeId> = Vec::new();
        let mut seen: HashSet<NodeId> = HashSet::new();
        let mut queue: VecDeque<NodeId> = VecDeque::new();
        for s in &seeds {
            if seen.insert(s.clone()) {
                queue.push_back(s.clone());
            }
        }
        while let Some(cur) = match mode {
            TraversalMode::Bfs => queue.pop_front(),
            TraversalMode::Dfs => queue.pop_back(),
        } {
            if included.len() >= max_nodes {
                break;
            }
            included.push(cur.clone());
            if let Some(nbrs) = self.adjacency.get(&cur) {
                for nb in nbrs {
                    if seen.insert(nb.clone()) {
                        queue.push_back(nb.clone());
                    }
                }
            }
        }

        let node_set: HashSet<&NodeId> = included.iter().collect();
        let mut edges: Vec<EdgeRef> = kg
            .edges()
            .filter(|e| node_set.contains(&e.source) && node_set.contains(&e.target))
            .map(|e| EdgeRef {
                source: e.source.clone(),
                target: e.target.clone(),
                relation: e.relation.clone(),
            })
            .collect();
        edges.sort_by(|a, b| {
            (a.source.as_str(), a.target.as_str(), a.relation.as_str()).cmp(&(
                b.source.as_str(),
                b.target.as_str(),
                b.relation.as_str(),
            ))
        });

        QueryResult {
            seeds,
            nodes: included,
            edges,
        }
    }
}

/// Like [`query`] but with an explicit traversal `mode` (the MCP `query_graph`
/// tool exposes bfs/dfs). Builds a one-shot [`QueryIndex`];
/// callers issuing many queries against the same graph should build a
/// [`QueryIndex`] once and reuse it (see the MCP server).
pub fn query_modal(
    kg: &KnowledgeGraph,
    query_text: &str,
    max_nodes: usize,
    mode: TraversalMode,
) -> QueryResult {
    QueryIndex::build(kg).query(kg, query_text, max_nodes, mode)
}

/// Shortest undirected path between two node ids (inclusive), or `None`.
pub fn shortest_path(kg: &KnowledgeGraph, from: &NodeId, to: &NodeId) -> Option<Vec<NodeId>> {
    if !kg.contains_node(from) || !kg.contains_node(to) {
        return None;
    }
    if from == to {
        return Some(vec![from.clone()]);
    }
    let adj = undirected_adjacency(kg);
    let mut prev: HashMap<NodeId, NodeId> = HashMap::new();
    let mut seen: HashSet<NodeId> = HashSet::new();
    let mut queue: VecDeque<NodeId> = VecDeque::new();
    seen.insert(from.clone());
    queue.push_back(from.clone());
    while let Some(cur) = queue.pop_front() {
        if &cur == to {
            // Reconstruct.
            let mut path = vec![cur.clone()];
            let mut at = cur;
            while let Some(p) = prev.get(&at) {
                path.push(p.clone());
                at = p.clone();
            }
            path.reverse();
            return Some(path);
        }
        if let Some(nbrs) = adj.get(&cur) {
            for nb in nbrs {
                if seen.insert(nb.clone()) {
                    prev.insert(nb.clone(), cur.clone());
                    queue.push_back(nb.clone());
                }
            }
        }
    }
    None
}

/// Explain a node: its metadata + neighbours grouped by relation/direction.
pub fn explain(kg: &KnowledgeGraph, id: &NodeId) -> Option<Explain> {
    let node = kg.node(id)?;
    let mut neighbors: Vec<Neighbor> = Vec::new();
    for e in kg.incident_edges(id) {
        if &e.source == id {
            let label = kg
                .node(&e.target)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| e.target.0.clone());
            neighbors.push(Neighbor {
                id: e.target.clone(),
                label,
                relation: e.relation.clone(),
                direction: "out",
            });
        } else if &e.target == id {
            let label = kg
                .node(&e.source)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| e.source.0.clone());
            neighbors.push(Neighbor {
                id: e.source.clone(),
                label,
                relation: e.relation.clone(),
                direction: "in",
            });
        }
    }
    neighbors.sort_by(|a, b| {
        a.direction
            .cmp(b.direction)
            .then_with(|| a.relation.cmp(&b.relation))
            .then_with(|| a.id.cmp(&b.id))
    });
    Some(Explain {
        id: id.clone(),
        label: node.label.clone(),
        source_file: node.source_file.clone(),
        community: node.community,
        neighbors,
    })
}

/// Resolve a free-text `query` to a single node id, conservatively, via a
/// fallback cascade: exact id → unique
/// case-insensitive exact label → unique bare name (label minus a trailing
/// `()`) → unique case-insensitive source_file → unique case-insensitive
/// substring of a label. Any tie (or no match) returns `None` — we never guess.
pub fn resolve_seed(kg: &KnowledgeGraph, query: &str) -> Option<NodeId> {
    // 1. Exact node id.
    let as_id = NodeId(query.to_string());
    if kg.contains_node(&as_id) {
        return Some(as_id);
    }
    let q = query.to_lowercase();

    // 2. Unique case-insensitive exact label.
    if let Some(id) = unique_match(kg, |n| n.label.to_lowercase() == q) {
        return Some(id);
    }
    // 3. Unique bare name (undecorated callable label).
    let q_bare = bare_name(&q);
    if let Some(id) = unique_match(kg, |n| bare_name(&n.label.to_lowercase()) == q_bare) {
        return Some(id);
    }
    // 4. Unique case-insensitive source_file.
    if let Some(id) = unique_match(kg, |n| n.source_file.to_lowercase() == q) {
        return Some(id);
    }
    // 5. Unique case-insensitive label substring.
    if let Some(id) = unique_match(kg, |n| n.label.to_lowercase().contains(&q)) {
        return Some(id);
    }
    None
}

/// Lowercased label with a trailing `()` callable decoration removed.
fn bare_name(label: &str) -> String {
    let l = label.to_lowercase();
    l.strip_suffix("()").map(str::to_string).unwrap_or(l)
}

/// Return the single node id matching `pred`, or `None` if zero or >1 match.
/// Iterates in node order (deterministic) so a unique match is order-independent.
fn unique_match(
    kg: &KnowledgeGraph,
    pred: impl Fn(&codegraph_core::Node) -> bool,
) -> Option<NodeId> {
    let mut found: Option<NodeId> = None;
    for n in kg.nodes() {
        if pred(n) {
            if found.is_some() {
                return None; // ambiguous
            }
            found = Some(n.id.clone());
        }
    }
    found
}

/// Reverse-impact: the nodes that (transitively) depend on `seed`, reached by
/// walking edges backward (`target → source`) but only through `relations`,
/// bounded to `depth` hops. Each hit
/// records the hop count and the relation it was first reached through.
pub fn affected_nodes(
    kg: &KnowledgeGraph,
    seed: &NodeId,
    relations: &[&str],
    depth: usize,
) -> Vec<AffectedHit> {
    if !kg.contains_node(seed) {
        return Vec::new();
    }
    let relation_set: HashSet<&str> = relations.iter().copied().collect();
    // Reverse adjacency: target -> [(source, relation)], deterministic order.
    let mut rev: HashMap<NodeId, Vec<(NodeId, String)>> = HashMap::new();
    for e in kg.edges() {
        if e.source == e.target || !relation_set.contains(e.relation.as_str()) {
            continue;
        }
        rev.entry(e.target.clone())
            .or_default()
            .push((e.source.clone(), e.relation.clone()));
    }
    for v in rev.values_mut() {
        v.sort();
    }

    let mut hits: Vec<AffectedHit> = Vec::new();
    let mut seen: HashSet<NodeId> = HashSet::new();
    seen.insert(seed.clone());
    let mut queue: VecDeque<(NodeId, usize)> = VecDeque::new();
    queue.push_back((seed.clone(), 0));
    while let Some((cur, cur_depth)) = queue.pop_front() {
        if cur_depth >= depth {
            continue;
        }
        let Some(incoming) = rev.get(&cur) else {
            continue;
        };
        for (source, relation) in incoming {
            if !seen.insert(source.clone()) {
                continue;
            }
            hits.push(AffectedHit {
                node_id: source.clone(),
                depth: cur_depth + 1,
                via_relation: relation.clone(),
            });
            queue.push_back((source.clone(), cur_depth + 1));
        }
    }
    hits
}

/// Reverse-impact from MULTIPLE seeds in a single pass. Builds the reverse
/// adjacency once (borrowing the graph, so the build allocates nothing) and runs
/// one multi-source BFS, so the cost is O(edges + reached) instead of
/// O(seeds * edges) (what calling [`affected_nodes`] per seed would cost). Each
/// reached node records the shallowest hop from any seed and, among the edges
/// reaching it at that hop, the lexicographically smallest relation -- so the
/// result is independent of seed order. Seeds are excluded; output is sorted by
/// (depth, id).
///
/// This rebuilds the adjacency on every call. A caller that runs many walks
/// against one static graph (e.g. a long-lived server) should instead build a
/// [`ReverseImpactIndex`] once and call [`ReverseImpactIndex::affected_multi`].
pub fn affected_nodes_multi(
    kg: &KnowledgeGraph,
    seeds: &[NodeId],
    relations: &[&str],
    depth: usize,
) -> Vec<AffectedHit> {
    let relation_set: HashSet<&str> = relations.iter().copied().collect();
    let seed_set: HashSet<&NodeId> = seeds.iter().filter(|s| kg.contains_node(s)).collect();
    if seed_set.is_empty() {
        return Vec::new();
    }
    // Reverse adjacency over impact relations, built ONCE: target -> [(source, relation)].
    let mut rev: HashMap<&NodeId, Vec<(&NodeId, &str)>> = HashMap::new();
    for e in kg.edges() {
        if e.source == e.target || !relation_set.contains(e.relation.as_str()) {
            continue;
        }
        rev.entry(&e.target)
            .or_default()
            .push((&e.source, e.relation.as_str()));
    }

    // Multi-source BFS. BFS processes a full depth layer before the next, so a
    // node's first visit is its min depth and all its min-depth in-edges are seen
    // during that layer; the explicit (min depth, smallest relation) comparison
    // then makes the result independent of seed/edge order.
    let mut best: HashMap<NodeId, (usize, String)> = HashMap::new();
    let mut seen: HashSet<NodeId> = seed_set.iter().map(|s| (*s).clone()).collect();
    let mut queue: VecDeque<(NodeId, usize)> = seed_set.iter().map(|s| ((*s).clone(), 0)).collect();
    while let Some((cur, d)) = queue.pop_front() {
        if d >= depth {
            continue;
        }
        let Some(adj) = rev.get(&cur) else {
            continue;
        };
        for (src, rel) in adj {
            let nd = d + 1;
            let entry = best
                .entry((*src).clone())
                .or_insert((usize::MAX, String::new()));
            if nd < entry.0 || (nd == entry.0 && *rel < entry.1.as_str()) {
                *entry = (nd, (*rel).to_string());
            }
            if seen.insert((*src).clone()) {
                queue.push_back(((*src).clone(), nd));
            }
        }
    }

    // A seed reached as another seed's dependent must not appear in the result.
    let mut hits: Vec<AffectedHit> = best
        .into_iter()
        .filter(|(id, _)| !seed_set.contains(id))
        .map(|(node_id, (depth, via_relation))| AffectedHit {
            node_id,
            depth,
            via_relation,
        })
        .collect();
    hits.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    hits
}

/// Reverse-impact adjacency built once and reused across many `affected_multi`
/// queries against the same graph. Building it is O(edges); each subsequent walk
/// is then O(reached) instead of O(edges + reached), so a long-lived server that
/// forecasts many changes against a static graph can build it once per graph load
/// rather than per request. The relation set is fixed at build time; rebuild the
/// index whenever the graph or the relation set changes.
pub struct ReverseImpactIndex {
    /// target -> [(source, relation)] over the chosen impact relations.
    rev: HashMap<NodeId, Vec<(NodeId, String)>>,
}

impl ReverseImpactIndex {
    /// Build the reverse adjacency over `relations` (e.g.
    /// [`DEFAULT_AFFECTED_RELATIONS`]). Self-loops and edges whose relation is not
    /// in the set are skipped.
    pub fn build(kg: &KnowledgeGraph, relations: &[&str]) -> Self {
        let relation_set: HashSet<&str> = relations.iter().copied().collect();
        let mut rev: HashMap<NodeId, Vec<(NodeId, String)>> = HashMap::new();
        for e in kg.edges() {
            if e.source == e.target || !relation_set.contains(e.relation.as_str()) {
                continue;
            }
            rev.entry(e.target.clone())
                .or_default()
                .push((e.source.clone(), e.relation.clone()));
        }
        ReverseImpactIndex { rev }
    }

    /// Multi-source reverse-impact walk using the prebuilt adjacency. Semantics
    /// are identical to [`affected_nodes_multi`]; only the adjacency is reused
    /// instead of rebuilt. `kg` is still passed so seeds are validated against the
    /// current graph.
    pub fn affected_multi(
        &self,
        kg: &KnowledgeGraph,
        seeds: &[NodeId],
        depth: usize,
    ) -> Vec<AffectedHit> {
        let seed_set: HashSet<&NodeId> = seeds.iter().filter(|s| kg.contains_node(s)).collect();
        if seed_set.is_empty() {
            return Vec::new();
        }

        // Multi-source BFS. BFS processes a full depth layer before the next, so a
        // node's first visit is its min depth and all its min-depth in-edges are
        // seen during that layer; the explicit (min depth, smallest relation)
        // comparison then makes the result independent of seed/edge order.
        let mut best: HashMap<NodeId, (usize, String)> = HashMap::new();
        let mut seen: HashSet<NodeId> = seed_set.iter().map(|s| (*s).clone()).collect();
        let mut queue: VecDeque<(NodeId, usize)> =
            seed_set.iter().map(|s| ((*s).clone(), 0)).collect();
        while let Some((cur, d)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            let Some(adj) = self.rev.get(&cur) else {
                continue;
            };
            for (src, rel) in adj {
                let nd = d + 1;
                let entry = best
                    .entry(src.clone())
                    .or_insert((usize::MAX, String::new()));
                if nd < entry.0 || (nd == entry.0 && rel.as_str() < entry.1.as_str()) {
                    *entry = (nd, rel.clone());
                }
                if seen.insert(src.clone()) {
                    queue.push_back((src.clone(), nd));
                }
            }
        }

        // A seed reached as another seed's dependent must not appear in the result.
        let mut hits: Vec<AffectedHit> = best
            .into_iter()
            .filter(|(id, _)| !seed_set.contains(id))
            .map(|(node_id, (depth, via_relation))| AffectedHit {
                node_id,
                depth,
                via_relation,
            })
            .collect();
        hits.sort_by(|a, b| {
            a.depth
                .cmp(&b.depth)
                .then_with(|| a.node_id.cmp(&b.node_id))
        });
        hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codegraph_core::{Confidence, Edge, FileType, GraphData, Node};
    use serde_json::Map;

    fn build(nodes: &[(&str, &str)], edges: &[(&str, &str, &str)]) -> KnowledgeGraph {
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: nodes
                .iter()
                .map(|(id, label)| Node {
                    id: NodeId(id.to_string()),
                    label: label.to_string(),
                    file_type: FileType::Code,
                    source_file: format!("{id}.py"),
                    source_location: Some("L1".into()),
                    community: Some(0),
                    repo: None,
                    extra: Map::new(),
                })
                .collect(),
            links: edges
                .iter()
                .map(|(s, t, r)| Edge {
                    source: NodeId(s.to_string()),
                    target: NodeId(t.to_string()),
                    relation: r.to_string(),
                    confidence: Confidence::Extracted,
                    source_file: "x.py".into(),
                    source_location: Some("L1".into()),
                    confidence_score: None,
                    weight: 1.0,
                    context: None,
                    cross_repo: false,
                    extra: Map::new(),
                })
                .collect(),
            hyperedges: vec![],
            built_at_commit: None,
        };
        KnowledgeGraph::from_graph_data(gd)
    }

    #[test]
    fn tokenize_handles_camel_and_snake() {
        assert_eq!(tokenize("run_analysis()"), vec!["run", "analysis"]);
        assert_eq!(tokenize("AuthService"), vec!["auth", "service"]);
    }

    #[test]
    fn query_finds_matching_seed_and_subgraph() {
        let kg = build(
            &[
                ("auth", "AuthService"),
                ("login", "login_user"),
                ("db", "Database"),
            ],
            &[("auth", "login", "calls"), ("auth", "db", "uses")],
        );
        let r = query(&kg, "authentication auth", 10);
        assert!(r.seeds.contains(&NodeId("auth".into())));
        // BFS pulls in neighbours.
        assert!(r.nodes.contains(&NodeId("login".into())));
        assert!(!r.edges.is_empty());
    }

    #[test]
    fn query_respects_max_nodes() {
        let kg = build(
            &[
                ("a", "Alpha"),
                ("b", "Beta"),
                ("c", "Gamma"),
                ("d", "Delta"),
            ],
            &[
                ("a", "b", "calls"),
                ("b", "c", "calls"),
                ("c", "d", "calls"),
            ],
        );
        let r = query(&kg, "alpha", 2);
        assert_eq!(r.nodes.len(), 2);
    }

    #[test]
    fn query_index_reused_across_queries_matches_fresh() {
        // H1: a `QueryIndex` built once and reused across many queries must give
        // byte-identical results to the per-query `query_modal` path.
        let kg = build(
            &[
                ("auth", "AuthService"),
                ("login", "login_user"),
                ("token", "TokenStore"),
                ("db", "Database"),
            ],
            &[
                ("auth", "login", "calls"),
                ("login", "token", "uses"),
                ("token", "db", "reads_from"),
            ],
        );
        let index = QueryIndex::build(&kg);
        for mode in [TraversalMode::Bfs, TraversalMode::Dfs] {
            for q in ["auth login", "token store", "database", "nomatch"] {
                assert_eq!(
                    index.query(&kg, q, 10, mode),
                    query_modal(&kg, q, 10, mode),
                    "reused QueryIndex must match query_modal for q={q:?} mode={mode:?}"
                );
            }
        }
    }

    #[test]
    fn shortest_path_finds_route() {
        let kg = build(
            &[("a", "A"), ("b", "B"), ("c", "C")],
            &[("a", "b", "calls"), ("b", "c", "calls")],
        );
        let p = shortest_path(&kg, &NodeId("a".into()), &NodeId("c".into())).unwrap();
        assert_eq!(
            p,
            vec![NodeId("a".into()), NodeId("b".into()), NodeId("c".into())]
        );
        assert!(shortest_path(&kg, &NodeId("a".into()), &NodeId("missing".into())).is_none());
    }

    #[test]
    fn explain_lists_in_and_out_neighbours() {
        let kg = build(
            &[("a", "A"), ("b", "B"), ("c", "C")],
            &[("a", "b", "calls"), ("c", "a", "imports")],
        );
        let e = explain(&kg, &NodeId("a".into())).unwrap();
        assert_eq!(e.label, "A");
        let dirs: Vec<&str> = e.neighbors.iter().map(|n| n.direction).collect();
        assert!(dirs.contains(&"out")); // a -> b
        assert!(dirs.contains(&"in")); // c -> a
    }

    #[test]
    fn affected_walks_dependents_backward_with_depth_and_relation() {
        // c -> b -> a  (c depends on b depends on a). Changing `a` affects b and c.
        let kg = build(
            &[("a", "A"), ("b", "B"), ("c", "C")],
            &[("b", "a", "calls"), ("c", "b", "calls")],
        );
        let aff = affected_nodes(&kg, &NodeId("a".into()), DEFAULT_AFFECTED_RELATIONS, 5);
        let ids: Vec<&str> = aff.iter().map(|h| h.node_id.0.as_str()).collect();
        assert!(ids.contains(&"b") && ids.contains(&"c"));
        // Hops are recorded: b at depth 1, c at depth 2.
        let b = aff.iter().find(|h| h.node_id.0 == "b").unwrap();
        assert_eq!(b.depth, 1);
        assert_eq!(b.via_relation, "calls");
        assert_eq!(aff.iter().find(|h| h.node_id.0 == "c").unwrap().depth, 2);
    }

    #[test]
    fn affected_respects_depth_bound() {
        let kg = build(
            &[("a", "A"), ("b", "B"), ("c", "C")],
            &[("b", "a", "calls"), ("c", "b", "calls")],
        );
        // depth=1 stops before reaching c.
        let aff = affected_nodes(&kg, &NodeId("a".into()), DEFAULT_AFFECTED_RELATIONS, 1);
        let ids: Vec<&str> = aff.iter().map(|h| h.node_id.0.as_str()).collect();
        assert_eq!(ids, vec!["b"], "depth 1 reaches only direct dependents");
    }

    #[test]
    fn affected_filters_by_relation() {
        // b depends on a via `contains` (not an impact relation) -> excluded.
        let kg = build(
            &[("a", "A"), ("b", "B"), ("c", "C")],
            &[("b", "a", "contains"), ("c", "a", "calls")],
        );
        let aff = affected_nodes(&kg, &NodeId("a".into()), DEFAULT_AFFECTED_RELATIONS, 5);
        let ids: Vec<&str> = aff.iter().map(|h| h.node_id.0.as_str()).collect();
        assert_eq!(ids, vec!["c"], "containment edge does not propagate impact");
    }

    #[test]
    fn affected_multi_matches_single_seed_for_one_seed() {
        // c -> b -> a, and d -> a. From {a}: b@1, c@2, d@1.
        let kg = build(
            &[("a", "A"), ("b", "B"), ("c", "C"), ("d", "D")],
            &[
                ("b", "a", "calls"),
                ("c", "b", "calls"),
                ("d", "a", "calls"),
            ],
        );
        let multi = affected_nodes_multi(&kg, &[NodeId("a".into())], DEFAULT_AFFECTED_RELATIONS, 5);
        let mut got: Vec<(String, usize)> = multi
            .iter()
            .map(|h| (h.node_id.0.clone(), h.depth))
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("b".to_string(), 1),
                ("c".to_string(), 2),
                ("d".to_string(), 1)
            ]
        );
        assert!(
            !multi.iter().any(|h| h.node_id.0 == "a"),
            "a is never its own dependent"
        );
    }

    #[test]
    fn affected_multi_takes_min_depth_and_smallest_relation_across_seeds() {
        // m depends on both x (via calls) and y (via references); n depends on m.
        // Seeding {x, y}: m at depth 1 via the smallest relation (calls); n at 2.
        let kg = build(
            &[("x", "X"), ("y", "Y"), ("m", "M"), ("n", "N")],
            &[
                ("m", "x", "calls"),
                ("m", "y", "references"),
                ("n", "m", "calls"),
            ],
        );
        let hits = affected_nodes_multi(
            &kg,
            &[NodeId("x".into()), NodeId("y".into())],
            DEFAULT_AFFECTED_RELATIONS,
            5,
        );
        let m = hits.iter().find(|h| h.node_id.0 == "m").unwrap();
        assert_eq!(m.depth, 1);
        assert_eq!(m.via_relation, "calls", "smallest relation at min depth");
        assert_eq!(hits.iter().find(|h| h.node_id.0 == "n").unwrap().depth, 2);
        assert!(
            !hits
                .iter()
                .any(|h| h.node_id.0 == "x" || h.node_id.0 == "y"),
            "seeds excluded"
        );
    }

    #[test]
    fn prebuilt_index_matches_oneshot_across_reuse() {
        // A prebuilt index queried for several different seed sets must return
        // exactly what building a throwaway index per call (affected_nodes_multi)
        // returns -- proving the cache changes cost, not results.
        let kg = build(
            &[("x", "X"), ("y", "Y"), ("m", "M"), ("n", "N"), ("z", "Z")],
            &[
                ("m", "x", "calls"),
                ("m", "y", "references"),
                ("n", "m", "calls"),
                ("z", "n", "imports"),
            ],
        );
        let index = ReverseImpactIndex::build(&kg, DEFAULT_AFFECTED_RELATIONS);
        for seeds in [
            vec![NodeId("x".into())],
            vec![NodeId("x".into()), NodeId("y".into())],
            vec![NodeId("m".into())],
            vec![NodeId("missing".into())], // unknown seed -> empty
        ] {
            for depth in [1usize, 2, 5] {
                let cached = index.affected_multi(&kg, &seeds, depth);
                let oneshot = affected_nodes_multi(&kg, &seeds, DEFAULT_AFFECTED_RELATIONS, depth);
                assert_eq!(cached, oneshot, "seeds={seeds:?} depth={depth}");
            }
        }
    }

    #[test]
    fn resolve_seed_cascade() {
        let kg = build(
            &[("transform_fn", "transform()"), ("other", "OtherThing")],
            &[],
        );
        // exact id
        assert_eq!(
            resolve_seed(&kg, "transform_fn"),
            Some(NodeId("transform_fn".into()))
        );
        // exact label (case-insensitive)
        assert_eq!(
            resolve_seed(&kg, "TRANSFORM()"),
            Some(NodeId("transform_fn".into()))
        );
        // bare name (undecorated callable label)
        assert_eq!(
            resolve_seed(&kg, "transform"),
            Some(NodeId("transform_fn".into()))
        );
        // unique substring
        assert_eq!(resolve_seed(&kg, "otherth"), Some(NodeId("other".into())));
        // no match
        assert_eq!(resolve_seed(&kg, "nonexistent"), None);
    }

    #[test]
    fn resolve_seed_is_none_on_ambiguity() {
        let kg = build(&[("h1", "transform()"), ("h2", "transform()")], &[]);
        // Two nodes share the label: ambiguous, so no guess.
        assert_eq!(resolve_seed(&kg, "transform"), None);
    }
}
