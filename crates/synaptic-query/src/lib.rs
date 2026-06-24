//! Graph query for Synaptic: IDF-scored subgraph retrieval, shortest path,
//! node explanation, and reverse-impact ("affected"). Shared by the CLI and
//! (later) the MCP/REST server.
//!
//! MVP note: subgraph size is bounded by a node count, not a token budget;
//! true token-budgeting (tiktoken) is deferred (§2.9).
#![forbid(unsafe_code)]

pub mod describe;
pub mod dynamic;

pub use dynamic::{dependents_caveat, DynamicCaveat, DynamicHazardIndex, SiteRef};

use std::collections::{HashMap, HashSet, VecDeque};

use serde::Serialize;
use synaptic_core::{NodeId, NodeKind};
use synaptic_graph::KnowledgeGraph;

pub use describe::{describe_node, NodeDescription};

/// Result of a text query: matched seeds plus the surrounding subgraph.
///
/// `nodes` is sorted by descending relevance score; `scores[i]` is the relevance
/// of `nodes[i]` (same length, same order). `edges` is sorted by descending
/// relevance of its endpoints. The scores let a caller triage signal from noise
/// instead of treating every returned node as equally relevant.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct QueryResult {
    pub seeds: Vec<NodeId>,
    pub nodes: Vec<NodeId>,
    /// Relevance score per node, parallel to `nodes` (higher = more relevant).
    pub scores: Vec<f64>,
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
    // Dynamic-dispatch evidence-link: a reflection site whose literal key resolved
    // to a unique target points caller->target, so the target's reverse-impact
    // includes the dynamic caller (low-confidence; surfaced with a caveat).
    "dynamic_ref",
];

/// One node reached by the reverse-impact walk: which node, how many hops from
/// the seed, and the relation it was reached through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AffectedHit {
    pub node_id: NodeId,
    pub depth: usize,
    pub via_relation: String,
}

/// True for edge relations that denote *ownership / definition* (a container or
/// definer pointing at what it owns) rather than *usage*. These are excluded from
/// [`references_to`] so a class is never reported as a "reference" to its own
/// member, and a file/module is not a reference to the symbols it declares.
fn is_ownership_relation(relation: &str) -> bool {
    let r = relation.to_ascii_lowercase();
    r == "contains"
        || r == "method"
        || r == "member_of"
        || r.starts_with("defines")
        || r.starts_with("declares")
        || r.starts_with("has_")
}

/// One incoming reference to a symbol: the node that uses it, and the relation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reference {
    pub id: NodeId,
    pub label: String,
    pub relation: String,
}

/// Every place a symbol is *used* — the "find all references" primitive. Returns
/// the nodes with an incoming edge to `id` through any non-ownership relation
/// (calls, imports, inheritance/implements, type uses, cross-language coupling,
/// and evidence-linked dynamic refs), deduped by (source node, relation).
///
/// Distinct from `find_callers` (call/use/reference edges only, so it misses a
/// type's imports and inheritance) and from [`affected_nodes`] (the transitive
/// closure). References are to the symbol itself; a type's members are NOT folded
/// in, matching the IDE "find all references" mental model.
pub fn references_to(kg: &KnowledgeGraph, id: &NodeId) -> Vec<Reference> {
    let Some(e) = explain(kg, id) else {
        return Vec::new();
    };
    let mut seen: HashSet<(NodeId, String)> = HashSet::new();
    e.neighbors
        .into_iter()
        .filter(|nb| nb.direction == "in" && !is_ownership_relation(&nb.relation))
        .filter(|nb| seen.insert((nb.id.clone(), nb.relation.clone())))
        .map(|nb| Reference {
            id: nb.id,
            label: nb.label,
            relation: nb.relation,
        })
        .collect()
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

/// How the recency (changed-files) signal influences a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecencyMode {
    /// Multiply the relevance of changed-file nodes (re-rank + expand). Default.
    #[default]
    Boost,
    /// Boost *and* inject changed-file nodes as seeds, so the branch's changed
    /// surface appears even when the query text matches little or nothing.
    Seed,
}

/// The changed-files signal for a single query. Borrowed; the caller (server/CLI)
/// builds it from git, so `synaptic-query` itself never touches git.
pub struct Recency<'a> {
    /// Node ids whose source file is in the changed set.
    pub changed: &'a HashSet<NodeId>,
    /// Per-node churn weight in `(0, 1]` (normalised lines-changed). A node absent
    /// from the map (or `None` map) gets weight `1.0`.
    pub churn: Option<&'a HashMap<NodeId, f64>>,
    pub mode: RecencyMode,
    /// Strength: a changed node's relevance gains an additive `boost * churn_weight`.
    pub boost: f64,
}

impl Recency<'_> {
    /// Additive relevance bonus for a node: `boost * churn_weight` if the node's
    /// file changed, else `0`. Additive (not multiplicative) so a changed node
    /// that does *not* match the query text still earns a positive score — that is
    /// what lets seed mode surface the changed surface, and lets boost re-rank
    /// zero-query-match neighbours. With no recency signal the bonus is `0`, so the
    /// frontier key is unchanged from the plain query.
    fn bonus(&self, id: &NodeId) -> f64 {
        if self.changed.contains(id) {
            let w = self.churn.and_then(|c| c.get(id).copied()).unwrap_or(1.0);
            self.boost * w
        } else {
            0.0
        }
    }
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
    /// Mean undirected degree, used to normalise the hub penalty so it is
    /// graph-relative (a "hub" is a node whose degree dwarfs the average).
    avg_degree: f64,
}

/// Relevance decay applied per expansion hop: a node `k` hops from the seed that
/// reaches it inherits `seed_score * DECAY^k`. Keeps far-flung neighbours from
/// ranking as high as the seeds while still letting a long relevant chain survive.
const DECAY: f64 = 0.5;

/// Down-weight a node by how far its degree exceeds the graph average, so a
/// high-fan-out hub (a registry, a `Builder`) is expanded last and its many
/// incidental neighbours rarely reach the node budget. Returns 1.0 for an
/// average node and falls toward 0 as degree grows; never increases relevance.
fn hub_penalty(degree: usize, avg_degree: f64) -> f64 {
    let avg = avg_degree.max(1.0);
    1.0 / (1.0 + (1.0 + degree as f64 / avg).ln())
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
        let avg_degree = if adjacency.is_empty() {
            0.0
        } else {
            adjacency.values().map(|v| v.len()).sum::<usize>() as f64 / adjacency.len() as f64
        };
        QueryIndex {
            n,
            node_tokens,
            df,
            adjacency,
            avg_degree,
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
        self.query_with_recency(kg, query_text, max_nodes, mode, None)
    }

    /// Like [`query`](Self::query) but biased toward recently-changed code: nodes
    /// in `recency.changed` have their relevance multiplied (boost mode) and, in
    /// seed mode, are injected as additional seeds so the changed surface appears
    /// even when the query text matches little. `recency = None` is byte-identical
    /// to [`query`](Self::query).
    pub fn query_with_recency(
        &self,
        kg: &KnowledgeGraph,
        query_text: &str,
        max_nodes: usize,
        mode: TraversalMode,
        recency: Option<&Recency>,
    ) -> QueryResult {
        let idf =
            |t: &str| ((self.n + 1.0) / (1.0 + *self.df.get(t).unwrap_or(&0) as f64)).ln() + 1.0;

        let q_tokens: HashSet<String> = tokenize(query_text).into_iter().collect();

        // A node's own relevance to the query: sum of matched-token IDF, length-
        // normalised so a long label can't out-score a tight match just by
        // accumulating tokens (BM25-lite). 0.0 if nothing matches.
        let node_relevance = |id: &NodeId| -> f64 {
            let Some(toks) = self.node_tokens.get(id) else {
                return 0.0;
            };
            let sum: f64 = q_tokens
                .iter()
                .filter(|t| toks.contains(*t))
                .map(|t| idf(t))
                .sum();
            if sum == 0.0 {
                0.0
            } else {
                sum / (toks.len().max(1) as f64).sqrt()
            }
        };
        let degree = |id: &NodeId| self.adjacency.get(id).map_or(0, |v| v.len());
        // Additive recency bonus (0.0 when no recency signal or node unchanged).
        // Added to a node's relevance before the hub penalty, so changed code both
        // ranks higher and is more likely to be pulled into the node budget.
        let recency_bonus = |id: &NodeId| recency.map_or(0.0, |r| r.bonus(id));

        // Score and rank seeds by raw relevance (no hub penalty here: a seed earns
        // its place by matching the query, even if it is also well-connected).
        let mut scored: Vec<(NodeId, f64)> = self
            .node_tokens
            .keys()
            .map(|id| (id.clone(), node_relevance(id)))
            .filter(|(_, s)| *s > 0.0)
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let mut seeds: Vec<NodeId> = scored.iter().take(8).map(|(id, _)| id.clone()).collect();

        // Seed mode: inject changed nodes as seeds so the changed surface appears
        // even with zero query-token overlap. They enter the frontier with a base
        // relevance (the seed-relevant amount carried by their churn-weighted
        // boost), so a changed node with no match still ranks sensibly.
        if let Some(r) = recency {
            if r.mode == RecencyMode::Seed {
                let existing: HashSet<&NodeId> = seeds.iter().collect();
                let mut inject: Vec<NodeId> = r
                    .changed
                    .iter()
                    .filter(|id| self.adjacency.contains_key(*id) && !existing.contains(*id))
                    .cloned()
                    .collect();
                inject.sort(); // deterministic injection order
                drop(existing);
                seeds.extend(inject);
            }
        }

        // Best-first expansion. The frontier is a max-heap keyed by hub-penalised
        // relevance, so the budget is spent on the most relevant neighbourhood
        // rather than on whatever a breadth-first wave happened to reach. `rel` is
        // the pre-penalty relevance carried for inheritance (so the hub penalty
        // shapes ordering without compounding along a path); `key` = rel * penalty
        // is the frontier priority and the reported score. `mode` only breaks
        // score ties: bfs prefers earlier-discovered nodes (FIFO), dfs later (LIFO).
        let mut heap: std::collections::BinaryHeap<Frontier> = std::collections::BinaryHeap::new();
        let mut seq: u64 = 0;
        for s in &seeds {
            let rel = node_relevance(s);
            let key = (rel + recency_bonus(s)) * hub_penalty(degree(s), self.avg_degree);
            heap.push(Frontier {
                key,
                rel,
                seq,
                bias: mode,
                id: s.clone(),
            });
            seq += 1;
        }

        let mut included: Vec<NodeId> = Vec::new();
        let mut scores: Vec<f64> = Vec::new();
        let mut done: HashSet<NodeId> = HashSet::new();
        while let Some(cur) = heap.pop() {
            if included.len() >= max_nodes {
                break;
            }
            if !done.insert(cur.id.clone()) {
                continue; // already settled via a higher-priority path
            }
            included.push(cur.id.clone());
            scores.push(cur.key);
            if let Some(nbrs) = self.adjacency.get(&cur.id) {
                for nb in nbrs {
                    if done.contains(nb) {
                        continue;
                    }
                    let rel = node_relevance(nb).max(cur.rel * DECAY);
                    let key = (rel + recency_bonus(nb)) * hub_penalty(degree(nb), self.avg_degree);
                    heap.push(Frontier {
                        key,
                        rel,
                        seq,
                        bias: mode,
                        id: nb.clone(),
                    });
                    seq += 1;
                }
            }
        }

        // Re-rank the included set by score descending (id tie-break) so the most
        // relevant nodes lead, regardless of the order the frontier settled them.
        let score_of: HashMap<&NodeId, f64> = included
            .iter()
            .zip(scores.iter())
            .map(|(n, s)| (n, *s))
            .collect();
        let mut order: Vec<usize> = (0..included.len()).collect();
        order.sort_by(|&a, &b| {
            scores[b]
                .total_cmp(&scores[a])
                .then_with(|| included[a].cmp(&included[b]))
        });
        let nodes: Vec<NodeId> = order.iter().map(|&i| included[i].clone()).collect();
        let scores: Vec<f64> = order.iter().map(|&i| scores[i]).collect();

        let node_set: HashSet<&NodeId> = nodes.iter().collect();
        let mut edges: Vec<EdgeRef> = kg
            .edges()
            .filter(|e| node_set.contains(&e.source) && node_set.contains(&e.target))
            .map(|e| EdgeRef {
                source: e.source.clone(),
                target: e.target.clone(),
                relation: e.relation.clone(),
            })
            .collect();
        // Rank edges by the relevance of their weaker endpoint (descending), so
        // signal edges lead; lexicographic order only breaks ties for determinism.
        let edge_rank = |e: &EdgeRef| -> f64 {
            let s = score_of.get(&e.source).copied().unwrap_or(0.0);
            let t = score_of.get(&e.target).copied().unwrap_or(0.0);
            s.min(t)
        };
        edges.sort_by(|a, b| {
            edge_rank(b).total_cmp(&edge_rank(a)).then_with(|| {
                (a.source.as_str(), a.target.as_str(), a.relation.as_str()).cmp(&(
                    b.source.as_str(),
                    b.target.as_str(),
                    b.relation.as_str(),
                ))
            })
        });

        QueryResult {
            seeds,
            nodes,
            scores,
            edges,
        }
    }
}

/// One entry on the best-first expansion frontier. `Ord` makes a `BinaryHeap`
/// pop the highest-relevance node first; score ties are broken by traversal
/// `bias` (bfs = earlier `seq` first, dfs = later first) then node id, so the
/// whole expansion is deterministic.
struct Frontier {
    key: f64,
    rel: f64,
    seq: u64,
    bias: TraversalMode,
    id: NodeId,
}

impl PartialEq for Frontier {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for Frontier {}
impl PartialOrd for Frontier {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Frontier {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        // Primary: higher key is greater (popped first).
        match self.key.total_cmp(&other.key) {
            Ordering::Equal => {}
            ord => return ord,
        }
        // Tie-break by traversal bias on insertion order.
        let seq_ord = match self.bias {
            // bfs: earlier seq should pop first => earlier seq is "greater".
            TraversalMode::Bfs => other.seq.cmp(&self.seq),
            // dfs: later seq should pop first => later seq is "greater".
            TraversalMode::Dfs => self.seq.cmp(&other.seq),
        };
        match seq_ord {
            Ordering::Equal => {}
            ord => return ord,
        }
        // Final deterministic tie-break: smaller id pops first => "greater".
        other.id.cmp(&self.id)
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
    // 1. Exact node id. Checked on the FULL query first so an id that legitimately
    // contains `@` (e.g. a `react@18` package stub) is not split into name@file.
    let as_id = NodeId(query.to_string());
    if kg.contains_node(&as_id) {
        return Some(as_id);
    }
    // An optional `name@file-substring` qualifier pins a name shared by several
    // files to one file. Honored here (not just in predict_edit) so every
    // navigation tool that resolves through resolve_seed handles it uniformly.
    let (name, file_hint) = split_file_hint(query);

    // When the query carries an `@` qualifier, first try the WHOLE query as-is
    // (no split): a label may legitimately contain `@` (e.g. an import specifier
    // `git@github.com`), and that literal meaning wins over the qualifier reading.
    // Only if the whole query resolves to nothing do we fall back to the split.
    if file_hint.is_some() {
        if let Some(id) = resolve_cascade(kg, &query.to_lowercase(), |_| true, true) {
            return Some(id);
        }
    }

    let q = name.to_lowercase();
    let file_ok = |n: &synaptic_core::Node| match &file_hint {
        Some(h) => normalized_file(&n.source_file).contains(h),
        None => true,
    };
    // The source_file stage is meaningless once a file hint is present (the bare
    // part is a symbol, not a path), so it is dropped from the cascade in that case.
    resolve_cascade(kg, &q, file_ok, file_hint.is_none())
}

/// The unique-resolution cascade over a lowercased `q` and a `file_ok` filter:
/// unique exact label -> unique bare name -> (optional) unique source_file ->
/// unique label substring. Each stage requires EXACTLY one match (a tie falls
/// through to the next stage); returns `None` if no stage resolves uniquely.
fn resolve_cascade(
    kg: &KnowledgeGraph,
    q: &str,
    file_ok: impl Fn(&synaptic_core::Node) -> bool,
    use_file_stage: bool,
) -> Option<NodeId> {
    if let Some(id) = unique_match(kg, |n| file_ok(n) && n.label.to_lowercase() == q) {
        return Some(id);
    }
    let q_bare = bare_name(q);
    if let Some(id) = unique_match(kg, |n| {
        file_ok(n) && bare_name(&n.label.to_lowercase()) == q_bare
    }) {
        return Some(id);
    }
    if use_file_stage {
        if let Some(id) = unique_match(kg, |n| n.source_file.to_lowercase() == q) {
            return Some(id);
        }
    }
    if let Some(id) = unique_match(kg, |n| file_ok(n) && n.label.to_lowercase().contains(q)) {
        return Some(id);
    }
    None
}

/// Parse an optional `name@file-substring` qualifier. Returns `(name, Some(hint))`
/// when an `@` splits the query into two non-empty halves, else `(query, None)`.
/// The hint is normalized (backslashes to `/`, lowercased) to match against a
/// node's source file the same way [`normalized_file`] does.
fn split_file_hint(query: &str) -> (&str, Option<String>) {
    if let Some((name, hint)) = query.split_once('@') {
        let name = name.trim();
        let hint = hint.trim();
        if !name.is_empty() && !hint.is_empty() {
            return (name, Some(hint.replace('\\', "/").to_lowercase()));
        }
    }
    (query, None)
}

/// A source file path normalized for substring matching: backslashes to `/`,
/// lowercased. Keeps Windows and POSIX path separators comparable.
fn normalized_file(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

/// Outcome of resolving a free-text name/id to a graph node, distinguishing a
/// genuine miss from an ambiguous one so callers can report candidates instead of
/// a misleading "no node matches".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Exactly one node resolved.
    Unique(NodeId),
    /// Several nodes match; none could be chosen. Candidate ids are returned
    /// (node order, deduped) so the caller can list them.
    Ambiguous(Vec<NodeId>),
    /// Nothing matched at any cascade stage.
    NotFound,
}

/// Like [`resolve_seed`] but reports WHY resolution failed. The unique-resolution
/// path is identical to [`resolve_seed`] (no behavior change for callers that
/// already resolve a node); only the failure case is enriched into
/// `Ambiguous(candidates)` vs `NotFound`. Shared by every name-taking tool so the
/// messaging is consistent.
pub fn resolve_detailed(kg: &KnowledgeGraph, query: &str) -> Resolution {
    if let Some(id) = resolve_seed(kg, query) {
        return Resolution::Unique(id);
    }
    // resolve_seed returned None: no cascade stage had exactly one match, so the
    // first stage that matched anything matched two or more -> ambiguous. If
    // nothing matched anywhere -> not found.
    match candidate_matches(kg, query) {
        cands if cands.len() >= 2 => Resolution::Ambiguous(cands),
        _ => Resolution::NotFound,
    }
}

/// A single candidate from an ambiguous resolution, carrying the facts a caller
/// needs to pick one WITHOUT a follow-up `get_node` round-trip: the node id, its
/// source file, and its degree (edge count).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguityCandidate {
    pub id: NodeId,
    pub file: String,
    pub degree: usize,
}

/// Enrich a list of ambiguous candidate ids with each node's file and degree, so
/// both the MCP server and the CLI can render a self-sufficient candidate list
/// from one shared place.
pub fn candidate_details(kg: &KnowledgeGraph, ids: &[NodeId]) -> Vec<AmbiguityCandidate> {
    ids.iter()
        .map(|id| AmbiguityCandidate {
            id: id.clone(),
            file: kg
                .node(id)
                .map(|n| n.source_file.clone())
                .unwrap_or_default(),
            degree: kg.degree(id),
        })
        .collect()
}

/// The matches from the first resolution stage that yields any, in node order.
/// Mirrors the [`resolve_seed`] cascade (exact label -> bare name -> source_file
/// -> label substring); the exact-id stage is omitted (an id match is unique).
fn candidate_matches(kg: &KnowledgeGraph, query: &str) -> Vec<NodeId> {
    let (name, file_hint) = split_file_hint(query);
    // Mirror resolve_seed: with an `@` qualifier, try the WHOLE query first (a
    // label may contain `@`), and only fall back to the split when it matches
    // nothing.
    if file_hint.is_some() {
        let whole = candidate_stage_hits(kg, &query.to_lowercase(), |_| true, true);
        if !whole.is_empty() {
            return whole;
        }
    }
    let file_ok = |n: &synaptic_core::Node| match &file_hint {
        Some(h) => normalized_file(&n.source_file).contains(h),
        None => true,
    };
    candidate_stage_hits(kg, &name.to_lowercase(), file_ok, file_hint.is_none())
}

/// The hits from the first cascade stage that yields any, over a lowercased `q`
/// and a `file_ok` filter. The candidate counterpart of [`resolve_cascade`]:
/// returns ALL matches of the first non-empty stage (for listing candidates),
/// where `resolve_cascade` instead requires a stage to be uniquely matched.
fn candidate_stage_hits(
    kg: &KnowledgeGraph,
    q: &str,
    file_ok: impl Fn(&synaptic_core::Node) -> bool,
    use_file_stage: bool,
) -> Vec<NodeId> {
    let q_bare = bare_name(q);
    let label_eq = |n: &synaptic_core::Node| file_ok(n) && n.label.to_lowercase() == q;
    let bare_eq =
        |n: &synaptic_core::Node| file_ok(n) && bare_name(&n.label.to_lowercase()) == q_bare;
    let file_eq = |n: &synaptic_core::Node| use_file_stage && n.source_file.to_lowercase() == q;
    let label_sub = |n: &synaptic_core::Node| file_ok(n) && n.label.to_lowercase().contains(q);
    let stages: [&dyn Fn(&synaptic_core::Node) -> bool; 4] =
        [&label_eq, &bare_eq, &file_eq, &label_sub];
    for pred in stages {
        let hits: Vec<NodeId> = kg
            .nodes()
            .filter(|n| pred(n))
            .map(|n| n.id.clone())
            .collect();
        if !hits.is_empty() {
            return hits;
        }
    }
    Vec::new()
}

/// Lowercased label with a trailing `()` callable decoration removed.
fn bare_name(label: &str) -> String {
    let l = label.to_lowercase();
    l.strip_suffix("()").map(str::to_string).unwrap_or(l)
}

/// An importer that reaches a symbol only through a module-level import edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleImporter {
    /// The importing node (typically the importing file).
    pub node_id: NodeId,
    /// True when the import edge explicitly names the target symbol; false when
    /// the import brings the module in opaquely (namespace/default import, or an
    /// older graph with no recorded names), so the reference is uncertain.
    pub confirmed: bool,
    /// The import relation the link came through (`imports_from` / `re_exports`).
    pub via_relation: String,
}

/// Importers that reach `target` through a module-level `imports_from` /
/// `re_exports` edge to the module that DEFINES it.
///
/// The symbol-level reverse-impact walk misses these: an import edge points at a
/// module stub, not at the symbol, so walking backward from the symbol never
/// traverses it. Here we match a stub to the target's defining file by path stem,
/// and — when the edge records the imported names (see the extractor's `imported`
/// edge tag) — confirm the symbol is among them. Imports that record names but not
/// this symbol are skipped; opaque imports (no recorded names) are returned
/// `confirmed = false` so callers can surface them for review rather than assume.
pub fn module_importers(kg: &KnowledgeGraph, target: &NodeId) -> Vec<ModuleImporter> {
    let Some(tnode) = kg.node(target) else {
        return Vec::new();
    };
    let file_stem = path_stem(&tnode.source_file);
    if file_stem.is_empty() {
        return Vec::new();
    }
    let sym = bare_name(&tnode.label.to_lowercase());
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for e in kg.edges() {
        if e.relation != "imports_from" && e.relation != "re_exports" {
            continue;
        }
        if e.source == *target {
            continue; // a module importing from itself is not an external user
        }
        let Some(stub) = kg.node(&e.target) else {
            continue;
        };
        // The stub label is the import specifier; match its final path component
        // (sans source extension) to the defining file's stem.
        if !spec_stem(&stub.label).eq_ignore_ascii_case(&file_stem) {
            continue;
        }
        let confirmed = match e.extra.get("imported").and_then(|v| v.as_array()) {
            Some(arr) => {
                let names_this = arr
                    .iter()
                    .filter_map(|v| v.as_str())
                    .any(|n| n.eq_ignore_ascii_case(&sym));
                if !names_this {
                    continue; // names are recorded and ours is not one of them
                }
                true
            }
            None => false, // opaque import: uncertain, surface for review
        };
        if seen.insert(e.source.clone()) {
            out.push(ModuleImporter {
                node_id: e.source.clone(),
                confirmed,
                via_relation: e.relation.clone(),
            });
        }
    }
    out
}

/// Lowercased final path component of `p` with a known source extension removed
/// (`src/darkMode.ts` -> `darkmode`).
fn path_stem(p: &str) -> String {
    let norm = p.replace('\\', "/");
    let file = norm.rsplit('/').next().unwrap_or(norm.as_str());
    strip_source_ext(file).to_lowercase()
}

/// Final path component of an import specifier with a source extension removed
/// (`./darkMode` -> `darkMode`, `../a/b.js` -> `b`).
fn spec_stem(spec: &str) -> String {
    let norm = spec.replace('\\', "/");
    let last = norm.rsplit('/').next().unwrap_or(norm.as_str());
    strip_source_ext(last).to_string()
}

fn strip_source_ext(name: &str) -> &str {
    for ext in [".d.ts", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".py"] {
        if let Some(s) = name.strip_suffix(ext) {
            return s;
        }
    }
    name
}

/// Return the single node id matching `pred`, or `None` if zero or >1 match.
/// Iterates in node order (deterministic) so a unique match is order-independent.
fn unique_match(
    kg: &KnowledgeGraph,
    pred: impl Fn(&synaptic_core::Node) -> bool,
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

/// Relations that link a type (class/struct/interface/...) to a member it owns.
/// A class's reverse-impact lives on its members (methods carry the incoming
/// `calls`), not on the class symbol itself, so impact tools fold these in.
pub const MEMBER_RELATIONS: &[&str] = &["method", "contains", "has_method"];

/// The members owned by a type node: targets of its outgoing member edges
/// ([`MEMBER_RELATIONS`]). Excludes `references`/`uses` targets (types the class
/// merely mentions, not members). Deterministic (sorted, deduped). Empty for a
/// node with no members (e.g. a leaf function or a non-type node).
pub fn type_member_ids(kg: &KnowledgeGraph, id: &NodeId) -> Vec<NodeId> {
    let member_set: HashSet<&str> = MEMBER_RELATIONS.iter().copied().collect();
    let mut out: Vec<NodeId> = kg
        .edges()
        .filter(|e| &e.source == id && member_set.contains(e.relation.as_str()) && &e.target != id)
        .map(|e| e.target.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Reverse-impact seeded from many `roots` at once, reporting every node that
/// (transitively, within `depth`) depends on any root, EXCEPT the nodes in
/// `exclude`. Unlike [`affected_nodes_multi`] (which drops every seed from the
/// output), a root that is itself a dependent of another root IS reported -- so
/// folding a class plus its members surfaces the members that call sibling
/// members (the class's internal coupling), while `exclude` drops just the class
/// symbol. Each reached node records its shallowest hop and, among edges at that
/// hop, the lexicographically smallest relation, so the result is order-stable.
pub fn affected_rooted(
    kg: &KnowledgeGraph,
    roots: &[NodeId],
    exclude: &[NodeId],
    relations: &[&str],
    depth: usize,
) -> Vec<AffectedHit> {
    let relation_set: HashSet<&str> = relations.iter().copied().collect();
    let exclude_set: HashSet<&NodeId> = exclude.iter().collect();
    let root_set: Vec<&NodeId> = roots.iter().filter(|r| kg.contains_node(r)).collect();
    if root_set.is_empty() {
        return Vec::new();
    }
    // Reverse adjacency over impact relations: target -> [(source, relation)].
    let mut rev: HashMap<&NodeId, Vec<(&NodeId, &str)>> = HashMap::new();
    for e in kg.edges() {
        if e.source == e.target || !relation_set.contains(e.relation.as_str()) {
            continue;
        }
        rev.entry(&e.target)
            .or_default()
            .push((&e.source, e.relation.as_str()));
    }
    let mut best: HashMap<NodeId, (usize, String)> = HashMap::new();
    // `queued` only dedups the BFS frontier; it is NOT the output filter, so a
    // root reached as another root's dependent can still land in `best`.
    let mut queued: HashSet<NodeId> = root_set.iter().map(|r| (*r).clone()).collect();
    let mut queue: VecDeque<(NodeId, usize)> = root_set.iter().map(|r| ((*r).clone(), 0)).collect();
    while let Some((cur, d)) = queue.pop_front() {
        if d >= depth {
            continue;
        }
        let Some(adj) = rev.get(&cur) else {
            continue;
        };
        for (src, rel) in adj {
            if exclude_set.contains(*src) {
                continue;
            }
            let nd = d + 1;
            let entry = best
                .entry((*src).clone())
                .or_insert((usize::MAX, String::new()));
            if nd < entry.0 || (nd == entry.0 && *rel < entry.1.as_str()) {
                *entry = (nd, (*rel).to_string());
            }
            if queued.insert((*src).clone()) {
                queue.push_back(((*src).clone(), nd));
            }
        }
    }
    let mut hits: Vec<AffectedHit> = best
        .into_iter()
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

/// True for type-container kinds whose reverse-impact lives on their members
/// (methods carry the incoming `calls`/`references`), not the type symbol itself.
pub fn is_type_like(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Class
            | NodeKind::Interface
            | NodeKind::Trait
            | NodeKind::Struct
            | NodeKind::Enum
            | NodeKind::Protocol
            | NodeKind::Object
    )
}

/// Reverse-impact for `seed`, folding a type's members in. When `seed` is a
/// type-like node ([`is_type_like`]) with members, seeds the walk from the type
/// PLUS its members (the members' incoming calls are the class's real coupling)
/// and returns the member count for labeling; otherwise a plain single-seed walk
/// with member count 0. Shared by the MCP server and the CLI so both surfaces give
/// a class the same non-empty blast radius.
pub fn affected_including_members(
    kg: &KnowledgeGraph,
    seed: &NodeId,
    relations: &[&str],
    depth: usize,
) -> (Vec<AffectedHit>, usize) {
    let members = match kg.node(seed).and_then(|n| n.kind()) {
        Some(k) if is_type_like(k) => type_member_ids(kg, seed),
        _ => Vec::new(),
    };
    if members.is_empty() {
        (affected_nodes(kg, seed, relations, depth), 0)
    } else {
        let mut roots = Vec::with_capacity(members.len() + 1);
        roots.push(seed.clone());
        roots.extend(members.iter().cloned());
        (
            affected_rooted(kg, &roots, std::slice::from_ref(seed), relations, depth),
            members.len(),
        )
    }
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
    use serde_json::Map;
    use synaptic_core::{Confidence, Edge, FileType, GraphData, Node};

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
    fn references_to_unions_usages_and_excludes_ownership() {
        let kg = build(
            &[
                ("user", "User"),
                ("sub", "Sub"),
                ("mod", "Mod"),
                ("pkg", "Pkg"),
            ],
            &[
                ("sub", "user", "implements"), // usage (incoming)
                ("mod", "user", "imports"),    // usage (incoming)
                ("pkg", "user", "contains"),   // ownership (incoming) -> excluded
                ("user", "sub", "calls"),      // outgoing -> not a reference to User
            ],
        );
        let refs = references_to(&kg, &NodeId("user".into()));
        let rels: std::collections::HashSet<&str> =
            refs.iter().map(|r| r.relation.as_str()).collect();
        assert!(
            rels.contains("implements") && rels.contains("imports"),
            "{rels:?}"
        );
        assert!(!rels.contains("contains"), "ownership excluded: {rels:?}");
        // Only the two incoming usages; the outgoing call is not a reference.
        assert_eq!(refs.len(), 2, "{refs:?}");
    }

    #[test]
    fn is_ownership_relation_classifies_containment() {
        assert!(is_ownership_relation("contains"));
        assert!(is_ownership_relation("has_column"));
        assert!(is_ownership_relation("defines"));
        assert!(!is_ownership_relation("calls"));
        assert!(!is_ownership_relation("imports"));
        assert!(!is_ownership_relation("implements"));
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

    // A graph shaped like the screenshot regression: one strongly-matching seed
    // (`reader`), a relevant neighbour (`mesh`), and a high-degree hub (`builder`)
    // whose many junk neighbours (`j1..j6`) flood a plain BFS. Used by the
    // best-first / hub-penalty tests below.
    fn hub_graph() -> KnowledgeGraph {
        let mut nodes = vec![
            ("reader", "VehicleScreenMapReader"),
            ("mesh", "MeshQuadInterpreter"),
            ("deep", "MeshDetailLevel"),
            ("builder", "Builder"),
        ];
        let mut edges = vec![
            ("reader", "mesh", "calls"),
            ("reader", "builder", "references"),
            ("mesh", "deep", "calls"),
        ];
        for i in 1..=6 {
            // leak ids so the &str borrows live for the call; test-only.
            let id: &'static str = Box::leak(format!("j{i}").into_boxed_str());
            let label: &'static str = Box::leak(format!("CreateFromCopy{i}").into_boxed_str());
            nodes.push((id, label));
            edges.push(("builder", id, "method"));
        }
        build(&nodes, &edges)
    }

    #[test]
    fn query_returns_relevance_scores_sorted_descending() {
        let kg = hub_graph();
        let r = query(&kg, "vehicle screen reader mesh", 20);
        assert_eq!(r.scores.len(), r.nodes.len(), "one score per returned node");
        assert!(!r.scores.is_empty());
        for w in r.scores.windows(2) {
            assert!(
                w[0] >= w[1],
                "nodes must come back sorted by score descending, got {:?}",
                r.scores
            );
        }
        // The strongly-matching seed must be the top-ranked node.
        assert_eq!(r.nodes.first(), Some(&NodeId("reader".into())));
    }

    #[test]
    fn relevant_neighbour_ranks_above_hub() {
        let kg = hub_graph();
        let r = query(&kg, "reader mesh", 20);
        let pos = |id: &str| r.nodes.iter().position(|n| n.0 == id);
        let mesh = pos("mesh").expect("mesh included");
        let builder = pos("builder").expect("builder included");
        assert!(
            mesh < builder,
            "query-matching neighbour `mesh` (#{mesh}) must outrank hub `builder` (#{builder})"
        );
    }

    #[test]
    fn best_first_spends_budget_on_relevance_not_hub_junk() {
        let kg = hub_graph();
        // Budget 4: a plain BFS from `reader` pulls in reader, builder, mesh, then
        // a junk hub-neighbour before reaching the relevant 2-hop `deep`.
        // Best-first must instead spend the budget on the relevant chain.
        let r = query(&kg, "reader mesh", 4);
        let ids: HashSet<&str> = r.nodes.iter().map(|n| n.0.as_str()).collect();
        assert!(
            ids.contains("deep"),
            "relevant 2-hop node should be included"
        );
        for i in 1..=6 {
            let j = format!("j{i}");
            assert!(
                !ids.contains(j.as_str()),
                "hub junk node {j} must not crowd out relevant nodes under a tight budget"
            );
        }
    }

    #[test]
    fn edges_sorted_by_relevance_not_lexicographically() {
        let kg = hub_graph();
        let r = query(&kg, "reader mesh", 20);
        let touches_hub = |e: &EdgeRef| {
            let h = |id: &str| id == "builder" || id.starts_with('j');
            h(&e.source.0) || h(&e.target.0)
        };
        // The lexicographic ordering this replaces put `builder`-sourced edges
        // first ("builder" < "mesh" < "reader"). Relevance ranking must instead
        // lead with an edge between query-relevant nodes, never the hub.
        assert!(
            !touches_hub(&r.edges[0]),
            "top edge should connect relevant nodes, got {:?}->{:?}",
            r.edges[0].source,
            r.edges[0].target
        );
        // Every hub/junk edge must sink below every relevant edge.
        let first_hub = r.edges.iter().position(touches_hub);
        let last_relevant = r.edges.iter().rposition(|e| !touches_hub(e));
        if let (Some(fh), Some(lr)) = (first_hub, last_relevant) {
            assert!(
                fh > lr,
                "hub edges must rank below all relevant edges (first hub #{fh}, last relevant #{lr})"
            );
        }
    }

    #[test]
    fn query_is_deterministic() {
        let kg = hub_graph();
        let a = query(&kg, "reader mesh builder", 10);
        let b = query(&kg, "reader mesh builder", 10);
        assert_eq!(a, b, "same query must return identical results");
    }

    // --- Recency (git/changed-files) awareness ---------------------------------

    fn nid(s: &str) -> NodeId {
        NodeId(s.into())
    }
    fn changed_set(ids: &[&str]) -> HashSet<NodeId> {
        ids.iter().map(|s| nid(s)).collect()
    }
    fn pos(r: &QueryResult, id: &str) -> Option<usize> {
        r.nodes.iter().position(|n| n.0 == id)
    }

    #[test]
    fn recency_boost_reranks_changed_above_equal_unchanged() {
        // x and y have identical labels and degree => identical base score (tie
        // broken by id, so x leads). Marking y as changed must float it above x.
        let kg = build(
            &[
                ("x", "auth_service"),
                ("y", "auth_service"),
                ("h", "Helper"),
            ],
            &[("x", "h", "calls"), ("y", "h", "calls")],
        );
        let idx = QueryIndex::build(&kg);
        let changed = changed_set(&["y"]);
        let rec = Recency {
            changed: &changed,
            churn: None,
            mode: RecencyMode::Boost,
            boost: 1.0,
        };
        let r = idx.query_with_recency(&kg, "auth service", 10, TraversalMode::Bfs, Some(&rec));
        assert!(
            pos(&r, "y").unwrap() < pos(&r, "x").unwrap(),
            "changed node y must outrank equal unchanged node x: {:?}",
            r.nodes
        );
    }

    #[test]
    fn recency_boost_changes_inclusion_under_tight_budget() {
        // s matches; n1/n2 are equal-score non-matching neighbours. Budget 2 keeps
        // s + one neighbour; by id that is n1. Boosting n2 must pull n2 in instead.
        let kg = build(
            &[("s", "Target"), ("n1", "Other"), ("n2", "Other")],
            &[("s", "n1", "calls"), ("s", "n2", "calls")],
        );
        let idx = QueryIndex::build(&kg);
        let changed = changed_set(&["n2"]);
        let rec = Recency {
            changed: &changed,
            churn: None,
            mode: RecencyMode::Boost,
            boost: 2.0,
        };
        let r = idx.query_with_recency(&kg, "target", 2, TraversalMode::Bfs, Some(&rec));
        let ids: HashSet<&str> = r.nodes.iter().map(|n| n.0.as_str()).collect();
        assert!(
            ids.contains("n2"),
            "boosted neighbour should be included: {ids:?}"
        );
        assert!(
            !ids.contains("n1"),
            "unboosted neighbour should be crowded out: {ids:?}"
        );
    }

    #[test]
    fn recency_churn_weight_ranks_heavier_change_higher() {
        // x and y are equal base score and both changed; y has higher churn weight
        // and must therefore rank above x.
        let kg = build(
            &[
                ("x", "auth_service"),
                ("y", "auth_service"),
                ("h", "Helper"),
            ],
            &[("x", "h", "calls"), ("y", "h", "calls")],
        );
        let idx = QueryIndex::build(&kg);
        let changed = changed_set(&["x", "y"]);
        let churn: HashMap<NodeId, f64> = [(nid("x"), 0.2), (nid("y"), 1.0)].into_iter().collect();
        let rec = Recency {
            changed: &changed,
            churn: Some(&churn),
            mode: RecencyMode::Boost,
            boost: 1.0,
        };
        let r = idx.query_with_recency(&kg, "auth service", 10, TraversalMode::Bfs, Some(&rec));
        assert!(
            pos(&r, "y").unwrap() < pos(&r, "x").unwrap(),
            "higher-churn node y must outrank lower-churn x: {:?}",
            r.nodes
        );
    }

    #[test]
    fn seed_mode_injects_changed_node_query_does_not_match() {
        // z is unrelated to the query and disconnected, so boost mode never reaches
        // it. Seed mode must inject it as a seed so the changed surface appears.
        let kg = build(
            &[("s", "Target"), ("z", "Unrelated")],
            &[("s", "s", "calls")], // self-loop ignored; z is isolated
        );
        let idx = QueryIndex::build(&kg);
        let changed = changed_set(&["z"]);
        let boost = Recency {
            changed: &changed,
            churn: None,
            mode: RecencyMode::Boost,
            boost: 1.0,
        };
        let seed = Recency {
            mode: RecencyMode::Seed,
            ..boost
        };
        let r_boost = idx.query_with_recency(&kg, "target", 10, TraversalMode::Bfs, Some(&boost));
        let r_seed = idx.query_with_recency(&kg, "target", 10, TraversalMode::Bfs, Some(&seed));
        assert!(
            pos(&r_boost, "z").is_none(),
            "boost mode should not inject z"
        );
        assert!(
            pos(&r_seed, "z").is_some(),
            "seed mode must inject changed z: {:?}",
            r_seed.nodes
        );
    }

    #[test]
    fn seed_mode_with_no_query_match_returns_changed_subgraph() {
        let kg = build(
            &[("a", "Alpha"), ("b", "Beta"), ("c", "Gamma")],
            &[("a", "b", "calls")],
        );
        let idx = QueryIndex::build(&kg);
        let changed = changed_set(&["a", "b"]);
        let rec = Recency {
            changed: &changed,
            churn: None,
            mode: RecencyMode::Seed,
            boost: 1.0,
        };
        let r = idx.query_with_recency(&kg, "zzz nomatch", 10, TraversalMode::Bfs, Some(&rec));
        assert!(
            pos(&r, "a").is_some() && pos(&r, "b").is_some(),
            "changed subgraph: {:?}",
            r.nodes
        );
    }

    #[test]
    fn recency_none_is_identical_to_plain_query() {
        let kg = build(
            &[
                ("auth", "AuthService"),
                ("login", "login_user"),
                ("db", "Database"),
            ],
            &[("auth", "login", "calls"), ("auth", "db", "uses")],
        );
        let idx = QueryIndex::build(&kg);
        for mode in [TraversalMode::Bfs, TraversalMode::Dfs] {
            assert_eq!(
                idx.query_with_recency(&kg, "auth login", 10, mode, None),
                query_modal(&kg, "auth login", 10, mode),
                "recency=None must match the plain query path (mode={mode:?})"
            );
        }
    }

    #[test]
    fn recency_query_is_deterministic() {
        let kg = build(
            &[
                ("x", "auth_service"),
                ("y", "auth_service"),
                ("h", "Helper"),
            ],
            &[("x", "h", "calls"), ("y", "h", "calls")],
        );
        let idx = QueryIndex::build(&kg);
        let changed = changed_set(&["y"]);
        let rec = Recency {
            changed: &changed,
            churn: None,
            mode: RecencyMode::Boost,
            boost: 1.0,
        };
        let a = idx.query_with_recency(&kg, "auth service", 10, TraversalMode::Bfs, Some(&rec));
        let b = idx.query_with_recency(&kg, "auth service", 10, TraversalMode::Bfs, Some(&rec));
        assert_eq!(a, b);
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
    fn type_member_ids_returns_members_not_referenced_types() {
        // C owns methods m1, m2 (via `method`) and references type T (not a member).
        let kg = build(
            &[("C", "C"), ("m1", "m1"), ("m2", "m2"), ("T", "T")],
            &[
                ("C", "m1", "method"),
                ("C", "m2", "contains"),
                ("C", "T", "references"),
            ],
        );
        let mut members: Vec<String> = type_member_ids(&kg, &NodeId("C".into()))
            .iter()
            .map(|n| n.0.clone())
            .collect();
        members.sort();
        assert_eq!(members, vec!["m1".to_string(), "m2".to_string()]);
    }

    #[test]
    fn affected_rooted_folds_members_and_excludes_the_class() {
        // C method-> m1, m2. X calls m1; m1 calls m2 (internal); R references C.
        // From roots {C, m1, m2} excluding C: X (calls m1), R (references C), and
        // m1 (calls m2, internal coupling) are dependents; m2 is a callee-only leaf
        // and C is excluded.
        let kg = build(
            &[
                ("C", "C"),
                ("m1", "m1"),
                ("m2", "m2"),
                ("X", "X"),
                ("R", "R"),
            ],
            &[
                ("C", "m1", "method"),
                ("C", "m2", "method"),
                ("X", "m1", "calls"),
                ("m1", "m2", "calls"),
                ("R", "C", "references"),
            ],
        );
        let roots = vec![NodeId("C".into()), NodeId("m1".into()), NodeId("m2".into())];
        let exclude = vec![NodeId("C".into())];
        let hits = affected_rooted(&kg, &roots, &exclude, DEFAULT_AFFECTED_RELATIONS, 5);
        let mut ids: Vec<&str> = hits.iter().map(|h| h.node_id.0.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["R", "X", "m1"]);
    }

    #[test]
    fn affected_including_members_folds_a_class_not_a_function() {
        // C (class) owns method m1; x calls m1. The bare class has no incoming
        // impact, but folding its members surfaces x. A non-type seed does not fold.
        let mk = |id: &str, kind: Option<NodeKind>| {
            let mut n = Node {
                id: NodeId(id.into()),
                label: id.into(),
                file_type: FileType::Code,
                source_file: format!("{id}.rs"),
                source_location: Some("L1".into()),
                community: None,
                repo: None,
                extra: Map::new(),
            };
            if let Some(k) = kind {
                n.set_kind(k);
            }
            n
        };
        let mk_edge = |s: &str, t: &str, rel: &str| Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: rel.into(),
            confidence: Confidence::Extracted,
            source_file: "x.rs".into(),
            source_location: Some("L1".into()),
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        };
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                mk("C", Some(NodeKind::Class)),
                mk("m1", Some(NodeKind::Method)),
                mk("x", None),
            ],
            links: vec![mk_edge("C", "m1", "method"), mk_edge("x", "m1", "calls")],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let kg = KnowledgeGraph::from_graph_data(gd);
        let (hits, mc) =
            affected_including_members(&kg, &NodeId("C".into()), DEFAULT_AFFECTED_RELATIONS, 5);
        assert_eq!(mc, 1, "one member folded in");
        assert!(hits.iter().any(|h| h.node_id.0 == "x"), "{hits:?}");
        // A non-type seed (the method itself) does not fold.
        let (_, mc2) =
            affected_including_members(&kg, &NodeId("m1".into()), DEFAULT_AFFECTED_RELATIONS, 5);
        assert_eq!(mc2, 0);
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

    #[test]
    fn resolve_detailed_distinguishes_unique_ambiguous_missing() {
        let kg = build(
            &[
                ("u", "unique_fn"),
                ("h1", "announce()"),
                ("h2", "announce()"),
            ],
            &[],
        );
        // Unique resolution is unchanged.
        assert_eq!(
            resolve_detailed(&kg, "unique_fn"),
            Resolution::Unique(NodeId("u".into()))
        );
        // An id always resolves uniquely.
        assert_eq!(
            resolve_detailed(&kg, "u"),
            Resolution::Unique(NodeId("u".into()))
        );
        // Ambiguous names return candidates (trailing () stripped consistently),
        // not a misleading "not found".
        match resolve_detailed(&kg, "announce") {
            Resolution::Ambiguous(ids) => {
                let mut got: Vec<String> = ids.iter().map(|i| i.0.clone()).collect();
                got.sort();
                assert_eq!(got, vec!["h1".to_string(), "h2".to_string()]);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        // Same ambiguity whether or not the caller appends ().
        assert_eq!(
            resolve_detailed(&kg, "announce()"),
            resolve_detailed(&kg, "announce")
        );
        // Genuinely absent -> NotFound.
        assert_eq!(
            resolve_detailed(&kg, "does_not_exist"),
            Resolution::NotFound
        );
    }

    #[test]
    fn resolve_honors_at_file_qualifier_uniformly() {
        // Two real definitions share the name `announce`, in different files
        // (the build helper gives node `h1` the file `h1.py`, etc.).
        let kg = build(&[("h1", "announce()"), ("h2", "announce()")], &[]);

        // Bare name is ambiguous (baseline).
        assert_eq!(resolve_seed(&kg, "announce"), None);

        // `name@file` pins it to one file for the SHARED resolver, so every
        // navigation tool that goes through resolve_seed/resolve_detailed honors
        // the same qualifier predict_edit documents.
        assert_eq!(resolve_seed(&kg, "announce@h1"), Some(NodeId("h1".into())));
        assert_eq!(
            resolve_detailed(&kg, "announce@h2.py"),
            Resolution::Unique(NodeId("h2".into()))
        );

        // A hint that matches no file -> NotFound, not a lenient fall-through to
        // an arbitrary node.
        assert_eq!(
            resolve_detailed(&kg, "announce@nosuchfile"),
            Resolution::NotFound
        );

        // A hint that still matches both files stays ambiguous.
        match resolve_detailed(&kg, "announce@.py") {
            Resolution::Ambiguous(ids) => assert_eq!(ids.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn candidate_details_carry_file_and_degree() {
        // `hub` has two edges, `leaf` has one. The build helper files them at
        // `<id>.py`. candidate_details must surface both so an ambiguity message
        // can list file + degree inline.
        let kg = build(
            &[("hub", "announce()"), ("leaf", "announce()"), ("x", "X")],
            &[("hub", "leaf", "calls"), ("hub", "x", "uses")],
        );
        let ids = vec![NodeId("hub".into()), NodeId("leaf".into())];
        let details = candidate_details(&kg, &ids);
        assert_eq!(details.len(), 2);
        assert_eq!(details[0].id, NodeId("hub".into()));
        assert_eq!(details[0].file, "hub.py");
        assert_eq!(details[0].degree, 2);
        assert_eq!(details[1].degree, 1);
    }

    #[test]
    fn resolve_exact_id_with_at_sign_is_not_split() {
        // A node id legitimately containing `@` (e.g. a package@version stub)
        // must resolve as an exact id, not be split into name@file-hint.
        let kg = build(&[("react@18", "react"), ("other", "Other")], &[]);
        assert_eq!(
            resolve_seed(&kg, "react@18"),
            Some(NodeId("react@18".into()))
        );
    }

    #[test]
    fn resolve_label_containing_at_is_not_split() {
        // A node LABEL may legitimately contain `@` (an import specifier or remote,
        // e.g. `git@github.com`) while its id differs. Querying the exact label must
        // resolve it -- the whole-query interpretation wins over treating `@` as a
        // file qualifier.
        let kg = build(&[("n1", "git@github.com"), ("n2", "Other")], &[]);
        assert_eq!(
            resolve_seed(&kg, "git@github.com"),
            Some(NodeId("n1".into()))
        );
        assert_eq!(
            resolve_detailed(&kg, "git@github.com"),
            Resolution::Unique(NodeId("n1".into()))
        );
        // The qualifier still works for a genuinely ambiguous bare name.
        let amb = build(&[("h1", "announce()"), ("h2", "announce()")], &[]);
        assert_eq!(resolve_seed(&amb, "announce@h1"), Some(NodeId("h1".into())));
    }
}
