//! Brandes betweenness centrality (unweighted, undirected) — node and edge
//! variants. Used for cross-community bridge detection in `analyze`'s suggested
//! questions and the no-community surprise fallback. Equivalent to the standard
//! normalized betweenness / edge-betweenness centrality measures.
//!
//! For graphs over [`SAMPLE_THRESHOLD`] nodes, node betweenness is
//! approximated from the first `min(SAMPLE_K, n)` source nodes (sorted-id order)
//! and rescaled by `n/k` — deterministic `k`-sampling without RNG
//! (the analysis layer forbids RNG for reproducibility).

use std::collections::{BTreeSet, HashMap, VecDeque};

use synaptic_core::NodeId;

use crate::graph::KnowledgeGraph;

/// Above this node count, node betweenness is sampled.
const SAMPLE_THRESHOLD: usize = 1000;
/// Sampled source count when over the threshold (`min(100, n)`).
const SAMPLE_K: usize = 100;

/// Sorted node ids + undirected, deduped, self-loop-free adjacency (by index).
fn undirected_adj(kg: &KnowledgeGraph) -> (Vec<NodeId>, Vec<Vec<usize>>) {
    let mut ids: Vec<NodeId> = kg.nodes().map(|n| n.id.clone()).collect();
    ids.sort();
    ids.dedup();
    let index: HashMap<&NodeId, usize> = ids.iter().enumerate().map(|(i, id)| (id, i)).collect();
    let mut sets: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); ids.len()];
    for e in kg.edges() {
        if e.source == e.target {
            continue;
        }
        let (Some(&a), Some(&b)) = (index.get(&e.source), index.get(&e.target)) else {
            continue;
        };
        sets[a].insert(b);
        sets[b].insert(a);
    }
    let adj = sets.into_iter().map(|s| s.into_iter().collect()).collect();
    (ids, adj)
}

/// Brandes accumulation over `sources` (unweighted BFS), returning raw node
/// betweenness. Edge betweenness is accumulated into `edge_bw` only when supplied
/// (ordered index pairs `(lo, hi)`) — skipping it avoids a hash-insert per edge
/// per source when only node centrality is needed (the common case). Undirected
/// double-counting is absorbed by the public normalizers.
fn brandes(
    adj: &[Vec<usize>],
    sources: &[usize],
    mut edge_bw: Option<&mut HashMap<(usize, usize), f64>>,
) -> Vec<f64> {
    let n = adj.len();
    let mut node_bw = vec![0.0f64; n];
    for &s in sources {
        let mut stack: Vec<usize> = Vec::new();
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut sigma = vec![0.0f64; n];
        let mut dist = vec![-1i64; n];
        sigma[s] = 1.0;
        dist[s] = 0;
        let mut q = VecDeque::new();
        q.push_back(s);
        while let Some(v) = q.pop_front() {
            stack.push(v);
            for &w in &adj[v] {
                if dist[w] < 0 {
                    dist[w] = dist[v] + 1;
                    q.push_back(w);
                }
                if dist[w] == dist[v] + 1 {
                    sigma[w] += sigma[v];
                    preds[w].push(v);
                }
            }
        }
        let mut delta = vec![0.0f64; n];
        while let Some(w) = stack.pop() {
            for &v in &preds[w] {
                let c = (sigma[v] / sigma[w]) * (1.0 + delta[w]);
                if let Some(eb) = edge_bw.as_deref_mut() {
                    let key = if v < w { (v, w) } else { (w, v) };
                    *eb.entry(key).or_insert(0.0) += c;
                }
                delta[v] += c;
            }
            if w != s {
                node_bw[w] += delta[w];
            }
        }
    }
    node_bw
}

/// Normalized node betweenness centrality, keyed by node id. Matches
/// `nx.betweenness_centrality` (normalized, undirected) on small graphs and
/// approximates it via deterministic source sampling on large ones.
pub fn node_betweenness(kg: &KnowledgeGraph) -> HashMap<NodeId, f64> {
    let (ids, adj) = undirected_adj(kg);
    let n = ids.len();
    if n == 0 {
        return HashMap::new();
    }
    let sources: Vec<usize> = if n > SAMPLE_THRESHOLD {
        (0..SAMPLE_K.min(n)).collect()
    } else {
        (0..n).collect()
    };
    let raw = brandes(&adj, &sources, None);
    // nx normalized undirected: scale = 1/((n-1)(n-2)); the raw Brandes sum already
    // double-counts undirected paths, which this scale absorbs. Sampling rescales by n/k.
    let mut out = HashMap::with_capacity(n);
    let scale = if n > 2 {
        let base = 1.0 / (((n - 1) * (n - 2)) as f64);
        if sources.len() < n {
            base * (n as f64 / sources.len() as f64)
        } else {
            base
        }
    } else {
        0.0
    };
    for (i, id) in ids.into_iter().enumerate() {
        out.insert(id, raw[i] * scale);
    }
    out
}

/// Normalized edge betweenness centrality, keyed by an unordered node-id pair
/// (lo, hi); undirected. Always uses
/// every source (callers bound the graph size before invoking).
pub fn edge_betweenness(kg: &KnowledgeGraph) -> HashMap<(NodeId, NodeId), f64> {
    let (ids, adj) = undirected_adj(kg);
    let n = ids.len();
    if n < 2 {
        return HashMap::new();
    }
    let sources: Vec<usize> = (0..n).collect();
    let mut raw: HashMap<(usize, usize), f64> = HashMap::new();
    brandes(&adj, &sources, Some(&mut raw));
    // Positive scale (preserves ranking); absorbs undirected double-counting like
    // nx's normalized edge betweenness.
    let scale = 1.0 / ((n * (n - 1)) as f64);
    raw.into_iter()
        .map(|((a, b), v)| ((ids[a].clone(), ids[b].clone()), v * scale))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::KnowledgeGraph;
    use serde_json::Map;
    use synaptic_core::{Confidence, Edge, FileType, GraphData, Node};

    /// Build an undirected KG from `(id, edges-to)` adjacency.
    fn kg(ids: &[&str], edges: &[(&str, &str)]) -> KnowledgeGraph {
        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: ids
                .iter()
                .map(|id| Node {
                    id: NodeId((*id).into()),
                    label: (*id).into(),
                    file_type: FileType::Code,
                    source_file: format!("{id}.rs"),
                    source_location: Some("L1".into()),
                    community: None,
                    repo: None,
                    extra: Map::new(),
                })
                .collect(),
            links: edges
                .iter()
                .map(|(s, t)| Edge {
                    source: NodeId((*s).into()),
                    target: NodeId((*t).into()),
                    relation: "calls".into(),
                    confidence: Confidence::Extracted,
                    source_file: String::new(),
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

    fn id(s: &str) -> NodeId {
        NodeId(s.into())
    }

    #[test]
    fn path_graph_middle_is_one_ends_zero() {
        // A - B - C : B is the only bridge. nx normalized: B=1.0, A=C=0.0.
        let g = kg(&["A", "B", "C"], &[("A", "B"), ("B", "C")]);
        let bw = node_betweenness(&g);
        assert!((bw[&id("B")] - 1.0).abs() < 1e-9, "B={}", bw[&id("B")]);
        assert!(bw[&id("A")].abs() < 1e-9);
        assert!(bw[&id("C")].abs() < 1e-9);
    }

    #[test]
    fn star_center_is_one() {
        // Center H connects three leaves; nx normalized center = 1.0.
        let g = kg(&["H", "A", "B", "C"], &[("H", "A"), ("H", "B"), ("H", "C")]);
        let bw = node_betweenness(&g);
        assert!((bw[&id("H")] - 1.0).abs() < 1e-9, "H={}", bw[&id("H")]);
        for leaf in ["A", "B", "C"] {
            assert!(bw[&id(leaf)].abs() < 1e-9, "{leaf} should be 0");
        }
    }

    #[test]
    fn edge_betweenness_peaks_on_the_bridge() {
        // Two triangles joined by a single bridge edge L3-R1.
        let g = kg(
            &["L1", "L2", "L3", "R1", "R2", "R3"],
            &[
                ("L1", "L2"),
                ("L2", "L3"),
                ("L3", "L1"),
                ("L3", "R1"), // bridge
                ("R1", "R2"),
                ("R2", "R3"),
                ("R3", "R1"),
            ],
        );
        let eb = edge_betweenness(&g);
        let bridge = eb
            .get(&(id("L3"), id("R1")))
            .or_else(|| eb.get(&(id("R1"), id("L3"))))
            .copied()
            .expect("bridge edge present");
        let max_other = eb
            .iter()
            .filter(|((a, b), _)| {
                !((*a == id("L3") && *b == id("R1")) || (*a == id("R1") && *b == id("L3")))
            })
            .map(|(_, v)| *v)
            .fold(0.0f64, f64::max);
        assert!(
            bridge > max_other,
            "bridge {bridge} should exceed every other edge {max_other}"
        );
    }
}
