//! In-house weighted-undirected community detection (Louvain + Leiden).
//!
//! Operates on a compact integer-indexed [`WGraph`] built from a
//! [`KnowledgeGraph`]'s edges (treated undirected, weights summed). Determinism
//! comes from processing nodes in index order (the caller builds the index in
//! sorted-`NodeId` order) — no RNG is used.

use std::collections::HashMap;

use codegraph_core::NodeId;

use crate::graph::KnowledgeGraph;

/// Weighted undirected graph with integer nodes `0..n`.
pub(crate) struct WGraph {
    pub n: usize,
    /// Adjacency: `adj[i]` = sorted `(neighbor, weight)` (both directions stored; excludes self).
    pub adj: Vec<Vec<(usize, f64)>>,
    /// Self-loop weight per node (counted once).
    pub self_loops: Vec<f64>,
    /// Weighted degree: `Σ_{j≠i} w_ij + 2*self_loop_i` (so `Σ degrees == 2m`).
    pub degrees: Vec<f64>,
    /// Total edge weight `m` (each undirected edge once; self-loops once).
    pub m: f64,
}

/// Build a `WGraph` over `nodes` (in the given order = index order) from `kg`'s
/// edges whose both endpoints are in `nodes`.
pub(crate) fn build_wgraph(kg: &KnowledgeGraph, nodes: &[NodeId]) -> WGraph {
    let index: HashMap<NodeId, usize> = nodes
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, id)| (id, i))
        .collect();
    let n = nodes.len();

    let mut pair_weight: HashMap<(usize, usize), f64> = HashMap::new();
    let mut self_loops = vec![0.0_f64; n];
    for e in kg.edges() {
        let (Some(&i), Some(&j)) = (index.get(&e.source), index.get(&e.target)) else {
            continue;
        };
        let w = e.weight as f64;
        if i == j {
            self_loops[i] += w;
        } else {
            let key = if i < j { (i, j) } else { (j, i) };
            *pair_weight.entry(key).or_insert(0.0) += w;
        }
    }

    let mut adj = vec![Vec::new(); n];
    let mut degrees = vec![0.0_f64; n];
    let mut m = 0.0_f64;
    for (&(i, j), &w) in &pair_weight {
        adj[i].push((j, w));
        adj[j].push((i, w));
        degrees[i] += w;
        degrees[j] += w;
        m += w;
    }
    for i in 0..n {
        degrees[i] += 2.0 * self_loops[i];
        m += self_loops[i];
    }
    for a in &mut adj {
        a.sort_by_key(|&(j, _)| j);
    }
    WGraph {
        n,
        adj,
        self_loops,
        degrees,
        m,
    }
}

const EPS: f64 = 1e-12;

/// Communities smaller than this are never visited by the within-community
/// refinement: a community of 3 or fewer nodes cannot split into two
/// well-connected (≥2-node) pieces, so refining it is wasted work.
const MIN_REFINE_SIZE: usize = 4;

/// Modularity of a labeling (resolution-weighted), undirected convention. Used
/// by [`leiden`] to guarantee the refined partition never scores below the
/// baseline, and by tests to validate optimization quality.
pub(crate) fn modularity(wg: &WGraph, labels: &[usize], resolution: f64) -> f64 {
    if wg.m == 0.0 {
        return 0.0;
    }
    let two_m = 2.0 * wg.m;
    let mut internal: HashMap<usize, f64> = HashMap::new(); // 2*L_c
    let mut degree_sum: HashMap<usize, f64> = HashMap::new(); // d_c
    for i in 0..wg.n {
        let c = labels[i];
        *degree_sum.entry(c).or_insert(0.0) += wg.degrees[i];
        let mut acc = 2.0 * wg.self_loops[i];
        for &(j, w) in &wg.adj[i] {
            if labels[j] == c {
                acc += w;
            }
        }
        *internal.entry(c).or_insert(0.0) += acc;
    }
    let mut q = 0.0;
    for (c, &l) in &internal {
        let d = degree_sum[c];
        q += l / two_m - resolution * (d / two_m) * (d / two_m);
    }
    q
}

/// One Louvain local-moving optimization pass set (to convergence). Mutates
/// `labels` in place; deterministic (node index order, sorted candidates).
fn local_move(wg: &WGraph, labels: &mut [usize], resolution: f64) {
    if wg.m == 0.0 {
        return;
    }
    let two_m = 2.0 * wg.m;
    // Σ_tot per community id (ids live in 0..n).
    let mut sigma_tot = vec![0.0_f64; wg.n];
    for i in 0..wg.n {
        sigma_tot[labels[i]] += wg.degrees[i];
    }
    loop {
        let mut improved = false;
        for i in 0..wg.n {
            let ci = labels[i];
            let ki = wg.degrees[i];
            let mut w_to: HashMap<usize, f64> = HashMap::new();
            for &(j, w) in &wg.adj[i] {
                if j != i {
                    *w_to.entry(labels[j]).or_insert(0.0) += w;
                }
            }
            // Remove i from its community.
            sigma_tot[ci] -= ki;
            // Baseline: staying in ci.
            let mut best_c = ci;
            let mut best_gain =
                w_to.get(&ci).copied().unwrap_or(0.0) - resolution * ki * sigma_tot[ci] / two_m;
            let mut cands: Vec<usize> = w_to.keys().copied().collect();
            cands.sort_unstable();
            for &c in &cands {
                let gain = w_to[&c] - resolution * ki * sigma_tot[c] / two_m;
                if gain > best_gain + EPS {
                    best_gain = gain;
                    best_c = c;
                }
            }
            sigma_tot[best_c] += ki;
            if best_c != ci {
                labels[i] = best_c;
                improved = true;
            }
        }
        if !improved {
            break;
        }
    }
}

/// Compact `labels` to `0..k` (distinct labels sorted) and aggregate `wg` into a
/// graph of `k` super-nodes. Returns `(aggregated, node -> super-node index)`.
fn aggregate(wg: &WGraph, labels: &[usize]) -> (WGraph, Vec<usize>) {
    let mut distinct: Vec<usize> = labels.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    let remap: HashMap<usize, usize> = distinct
        .iter()
        .enumerate()
        .map(|(new, &old)| (old, new))
        .collect();
    let k = distinct.len();
    let node_to_super: Vec<usize> = labels.iter().map(|c| remap[c]).collect();

    let mut pair_weight: HashMap<(usize, usize), f64> = HashMap::new();
    let mut self_loops = vec![0.0_f64; k];
    for i in 0..wg.n {
        let ci = node_to_super[i];
        self_loops[ci] += wg.self_loops[i];
        for &(j, w) in &wg.adj[i] {
            if j <= i {
                continue; // each undirected pair once
            }
            let cj = node_to_super[j];
            if ci == cj {
                self_loops[ci] += w;
            } else {
                let key = if ci < cj { (ci, cj) } else { (cj, ci) };
                *pair_weight.entry(key).or_insert(0.0) += w;
            }
        }
    }

    let mut adj = vec![Vec::new(); k];
    let mut degrees = vec![0.0_f64; k];
    let mut m = 0.0_f64;
    for (&(i, j), &w) in &pair_weight {
        adj[i].push((j, w));
        adj[j].push((i, w));
        degrees[i] += w;
        degrees[j] += w;
        m += w;
    }
    for i in 0..k {
        degrees[i] += 2.0 * self_loops[i];
        m += self_loops[i];
    }
    for a in &mut adj {
        a.sort_by_key(|&(j, _)| j);
    }
    (
        WGraph {
            n: k,
            adj,
            self_loops,
            degrees,
            m,
        },
        node_to_super,
    )
}

/// Multi-level Louvain. Returns a community label per original node.
pub(crate) fn louvain(wg: &WGraph, resolution: f64) -> Vec<usize> {
    let n = wg.n;
    if n == 0 {
        return Vec::new();
    }
    if wg.m == 0.0 {
        return (0..n).collect();
    }
    let mut orig_to_current: Vec<usize> = (0..n).collect();
    let mut current = wg.clone_graph();
    loop {
        let mut labels: Vec<usize> = (0..current.n).collect();
        local_move(&current, &mut labels, resolution);
        let mut distinct = labels.clone();
        distinct.sort_unstable();
        distinct.dedup();
        if distinct.len() == current.n {
            break; // no merging possible
        }
        let (agg, cur_to_new) = aggregate(&current, &labels);
        for x in orig_to_current.iter_mut() {
            *x = cur_to_new[*x];
        }
        current = agg;
        if current.n <= 1 {
            break;
        }
    }
    orig_to_current
}

/// Split each community of `labels` into its connected components (within the
/// community's induced subgraph). Returns fresh contiguous labels `0..r`; every
/// resulting community is connected. This is the refinement that gives Leiden
/// its defining guarantee over Louvain (no internally-disconnected community).
fn refine_connected(wg: &WGraph, labels: &[usize]) -> Vec<usize> {
    let mut refined = vec![usize::MAX; wg.n];
    let mut next = 0usize;
    for start in 0..wg.n {
        if refined[start] != usize::MAX {
            continue;
        }
        let pc = labels[start];
        let id = next;
        next += 1;
        let mut stack = vec![start];
        refined[start] = id;
        while let Some(u) = stack.pop() {
            for &(v, _) in &wg.adj[u] {
                if refined[v] == usize::MAX && labels[v] == pc {
                    refined[v] = id;
                    stack.push(v);
                }
            }
        }
    }
    refined
}

/// Leiden refinement phase (Traag et al. 2019): within each community of
/// `labels`, restart every node as its own singleton sub-community and run a
/// local-moving pass **restricted to that community** — a node may only join a
/// sub-community of its own community. Returns fresh contiguous sub-community
/// labels.
///
/// This is what gives Leiden its quality edge over Louvain + connected-component
/// refinement: it can split a community that is *connected but poorly knit* (two
/// dense groups joined by a single weak edge) into well-connected pieces, which
/// `refine_connected` (component split only) cannot. The usual randomized
/// tie-breaking is replaced by deterministic node-index ordering (no RNG), as
/// elsewhere in this module.
///
/// Communities smaller than [`MIN_REFINE_SIZE`] are kept whole and never visited:
/// they cannot split into two well-connected pieces, so the singleton-restart
/// churn would be pure overhead. Skipping them is the bulk of the refinement's
/// cost saving on graphs with many small communities.
fn refine_within_communities(wg: &WGraph, labels: &[usize], resolution: f64) -> Vec<usize> {
    if wg.m == 0.0 {
        return (0..wg.n).collect();
    }
    let two_m = 2.0 * wg.m;

    // Community sizes, and the canonical (lowest) node index per community, used
    // to keep small communities collapsed into a single sub-community.
    let mut size: HashMap<usize, usize> = HashMap::new();
    let mut canon: HashMap<usize, usize> = HashMap::new();
    for (i, &c) in labels.iter().enumerate() {
        *size.entry(c).or_insert(0) += 1;
        canon.entry(c).or_insert(i); // i ascends, so first seen is the min
    }
    let refinable = |c: usize| size[&c] >= MIN_REFINE_SIZE;

    // Nodes in refinable communities start as singletons (id = node index); nodes
    // in small communities share their community's canonical id (stay whole). Ids
    // live in 0..n, so sub-communities of different parent communities never collide.
    let mut sub: Vec<usize> = (0..wg.n)
        .map(|i| {
            if refinable(labels[i]) {
                i
            } else {
                canon[&labels[i]]
            }
        })
        .collect();
    let mut sigma_tot: Vec<f64> = vec![0.0; wg.n];
    for i in 0..wg.n {
        sigma_tot[sub[i]] += wg.degrees[i];
    }
    loop {
        let mut improved = false;
        for i in 0..wg.n {
            // Small-community nodes are frozen (kept whole), never moved.
            if !refinable(labels[i]) {
                continue;
            }
            let ci = sub[i];
            let ki = wg.degrees[i];
            // Weight from i into each sub-community, but only neighbours within
            // i's own parent community are eligible (the Leiden restriction).
            let mut w_to: HashMap<usize, f64> = HashMap::new();
            for &(j, w) in &wg.adj[i] {
                if j != i && labels[j] == labels[i] {
                    *w_to.entry(sub[j]).or_insert(0.0) += w;
                }
            }
            sigma_tot[ci] -= ki;
            let mut best_c = ci;
            let mut best_gain =
                w_to.get(&ci).copied().unwrap_or(0.0) - resolution * ki * sigma_tot[ci] / two_m;
            let mut cands: Vec<usize> = w_to.keys().copied().collect();
            cands.sort_unstable();
            for &c in &cands {
                let gain = w_to[&c] - resolution * ki * sigma_tot[c] / two_m;
                if gain > best_gain + EPS {
                    best_gain = gain;
                    best_c = c;
                }
            }
            sigma_tot[best_c] += ki;
            if best_c != ci {
                sub[i] = best_c;
                improved = true;
            }
        }
        if !improved {
            break;
        }
    }
    // Compact sub-community ids to a contiguous 0..k (sorted for determinism).
    let mut distinct: Vec<usize> = sub.clone();
    distinct.sort_unstable();
    distinct.dedup();
    let remap: HashMap<usize, usize> = distinct
        .iter()
        .enumerate()
        .map(|(new, &old)| (old, new))
        .collect();
    sub.iter().map(|c| remap[c]).collect()
}

/// Leiden community detection: Louvain optimization, then refinement.
///
/// The baseline pipeline (prior behavior) is connected-component refinement, a
/// re-merge pass, then a final component split. `local_move` can pull an
/// articulation node out of its community (it optimizes modularity over the
/// *destination*, never the source it leaves), so the final `refine_connected`
/// is what actually guarantees every community is connected.
///
/// On top of that, the within-community singleton-restart refinement is tried.
/// It only ever *splits* a community (never merges across communities), so when
/// it produces no new sub-communities it cannot change the outcome and the
/// baseline is returned directly — avoiding a redundant second pipeline + two
/// `modularity` passes on the common case (well-separated graphs). Only when the
/// refinement actually splits something do we compute the refined partition and
/// keep whichever scores higher, so the result can match or beat the baseline
/// but never regress. Deterministic (no RNG); structurally faithful to
/// graspologic's Leiden, not byte-identical.
pub(crate) fn leiden(wg: &WGraph, resolution: f64) -> Vec<usize> {
    if wg.n == 0 {
        return Vec::new();
    }
    if wg.m == 0.0 {
        return (0..wg.n).collect();
    }
    let louvain_labels = louvain(wg, resolution);

    let baseline = {
        let mut labels = refine_connected(wg, &louvain_labels);
        local_move(wg, &mut labels, resolution);
        refine_connected(wg, &labels)
    };

    // Within-community Leiden refinement. It can only subdivide communities, so
    // an unchanged community count means no split happened and the refined
    // pipeline would add nothing, so skip it.
    let sub = refine_within_communities(wg, &louvain_labels, resolution);
    if distinct_count(&sub) == distinct_count(&louvain_labels) {
        return baseline;
    }

    // A split occurred: consolidate the sub-communities (re-merge where modularity
    // improves) and re-guarantee connectivity, then keep the higher-scoring of the
    // two. Both end in `refine_connected`, so both are internally connected.
    let refined = {
        let mut labels = sub;
        local_move(wg, &mut labels, resolution);
        refine_connected(wg, &labels)
    };
    if modularity(wg, &refined, resolution) >= modularity(wg, &baseline, resolution) {
        refined
    } else {
        baseline
    }
}

/// Number of distinct labels in a partition.
fn distinct_count(labels: &[usize]) -> usize {
    let mut v = labels.to_vec();
    v.sort_unstable();
    v.dedup();
    v.len()
}

impl WGraph {
    fn clone_graph(&self) -> WGraph {
        WGraph {
            n: self.n,
            adj: self.adj.clone(),
            self_loops: self.self_loops.clone(),
            degrees: self.degrees.clone(),
            m: self.m,
        }
    }
}

// shared test helpers (used by community + cluster tests)
#[cfg(test)]
pub(crate) fn test_node(id: &str) -> codegraph_core::Node {
    codegraph_core::Node {
        id: NodeId(id.into()),
        label: id.into(),
        file_type: codegraph_core::FileType::Code,
        source_file: "a.py".into(),
        source_location: Some("L1".into()),
        community: None,
        repo: None,
        extra: serde_json::Map::new(),
    }
}

#[cfg(test)]
pub(crate) fn test_edge(s: &str, t: &str) -> codegraph_core::Edge {
    codegraph_core::Edge {
        source: NodeId(s.into()),
        target: NodeId(t.into()),
        relation: "calls".into(),
        confidence: codegraph_core::Confidence::Extracted,
        source_file: "a.py".into(),
        source_location: Some("L1".into()),
        confidence_score: None,
        weight: 1.0,
        context: None,
        cross_repo: false,
        extra: serde_json::Map::new(),
    }
}

/// Build a `KnowledgeGraph` from string node/edge specs (test helper).
#[cfg(test)]
pub(crate) fn kg_from(nodes: &[&str], edges: &[(&str, &str)]) -> KnowledgeGraph {
    let gd = codegraph_core::GraphData {
        directed: false,
        multigraph: false,
        graph: serde_json::Map::new(),
        nodes: nodes.iter().map(|n| test_node(n)).collect(),
        links: edges.iter().map(|(s, t)| test_edge(s, t)).collect(),
        hyperedges: vec![],
        built_at_commit: None,
    };
    KnowledgeGraph::from_graph_data(gd)
}

/// NodeId vec from string slice (test helper).
#[cfg(test)]
pub(crate) fn ids(names: &[&str]) -> Vec<NodeId> {
    names.iter().map(|s| NodeId(s.to_string())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triangle_degrees_and_total_weight() {
        let kg = kg_from(&["a", "b", "c"], &[("a", "b"), ("b", "c"), ("a", "c")]);
        let wg = build_wgraph(&kg, &ids(&["a", "b", "c"]));
        assert_eq!(wg.n, 3);
        assert_eq!(wg.m, 3.0);
        for d in &wg.degrees {
            assert_eq!(*d, 2.0);
        }
    }

    /// All node names and the all-pairs edges of two K5 cliques joined by one bridge.
    fn two_cliques() -> (Vec<String>, Vec<(String, String)>) {
        let mut names = Vec::new();
        let mut edges = Vec::new();
        for p in ["a", "b"] {
            for i in 0..5 {
                names.push(format!("{p}{i}"));
            }
            for i in 0..5 {
                for j in (i + 1)..5 {
                    edges.push((format!("{p}{i}"), format!("{p}{j}")));
                }
            }
        }
        edges.push(("a0".into(), "b0".into())); // bridge
        (names, edges)
    }

    fn distinct(labels: &[usize]) -> usize {
        let mut v = labels.to_vec();
        v.sort_unstable();
        v.dedup();
        v.len()
    }

    fn run_louvain(names: &[String], edges: &[(String, String)]) -> (Vec<NodeId>, Vec<usize>) {
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let edge_refs: Vec<(&str, &str)> = edges
            .iter()
            .map(|(s, t)| (s.as_str(), t.as_str()))
            .collect();
        let kg = kg_from(&name_refs, &edge_refs);
        let mut node_ids: Vec<NodeId> = kg.nodes().map(|n| n.id.clone()).collect();
        node_ids.sort();
        let wg = build_wgraph(&kg, &node_ids);
        let labels = louvain(&wg, 1.0);
        (node_ids, labels)
    }

    #[test]
    fn louvain_splits_two_cliques() {
        let (names, edges) = two_cliques();
        let (node_ids, labels) = run_louvain(&names, &edges);
        assert_eq!(
            distinct(&labels),
            2,
            "two cliques should yield two communities"
        );
        // All a* share a label distinct from all b*.
        let label_of = |name: &str| {
            let ix = node_ids.iter().position(|n| n.as_str() == name).unwrap();
            labels[ix]
        };
        let la = label_of("a0");
        let lb = label_of("b0");
        assert_ne!(la, lb);
        for i in 0..5 {
            assert_eq!(label_of(&format!("a{i}")), la);
            assert_eq!(label_of(&format!("b{i}")), lb);
        }
    }

    #[test]
    fn louvain_is_deterministic() {
        let (names, edges) = two_cliques();
        let (_, l1) = run_louvain(&names, &edges);
        let (_, l2) = run_louvain(&names, &edges);
        assert_eq!(l1, l2);
    }

    #[test]
    fn louvain_modularity_beats_single_community() {
        let (names, edges) = two_cliques();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let edge_refs: Vec<(&str, &str)> = edges
            .iter()
            .map(|(s, t)| (s.as_str(), t.as_str()))
            .collect();
        let kg = kg_from(&name_refs, &edge_refs);
        let mut node_ids: Vec<NodeId> = kg.nodes().map(|n| n.id.clone()).collect();
        node_ids.sort();
        let wg = build_wgraph(&kg, &node_ids);
        let labels = louvain(&wg, 1.0);
        let q_louvain = modularity(&wg, &labels, 1.0);
        let single = vec![0usize; wg.n];
        let q_single = modularity(&wg, &single, 1.0);
        assert!(
            q_louvain > q_single,
            "{q_louvain} should beat single {q_single}"
        );
    }

    #[test]
    fn louvain_edge_cases() {
        // empty
        let kg = kg_from(&[], &[]);
        let wg = build_wgraph(&kg, &[]);
        assert!(louvain(&wg, 1.0).is_empty());
        // single node
        let kg = kg_from(&["a"], &[]);
        let wg = build_wgraph(&kg, &ids(&["a"]));
        assert_eq!(louvain(&wg, 1.0), vec![0]);
        // two disconnected nodes: two communities
        let kg = kg_from(&["a", "b"], &[]);
        let wg = build_wgraph(&kg, &ids(&["a", "b"]));
        assert_eq!(distinct(&louvain(&wg, 1.0)), 2);
    }

    /// Assert no community's induced subgraph is internally disconnected.
    fn assert_communities_connected(wg: &WGraph, labels: &[usize]) {
        use std::collections::HashMap;
        let mut by_comm: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, &c) in labels.iter().enumerate() {
            by_comm.entry(c).or_default().push(i);
        }
        for (c, members) in by_comm {
            if members.len() <= 1 {
                continue;
            }
            let set: std::collections::HashSet<usize> = members.iter().copied().collect();
            // BFS from first member within the community.
            let mut seen = std::collections::HashSet::new();
            let mut stack = vec![members[0]];
            seen.insert(members[0]);
            while let Some(u) = stack.pop() {
                for &(v, _) in &wg.adj[u] {
                    if set.contains(&v) && seen.insert(v) {
                        stack.push(v);
                    }
                }
            }
            assert_eq!(
                seen.len(),
                members.len(),
                "community {c} is internally disconnected"
            );
        }
    }

    #[test]
    fn refine_connected_splits_disconnected_community() {
        // Two disjoint edges a-b and c-d, but all labeled the same community.
        let kg = kg_from(&["a", "b", "c", "d"], &[("a", "b"), ("c", "d")]);
        let wg = build_wgraph(&kg, &ids(&["a", "b", "c", "d"]));
        let refined = refine_connected(&wg, &[0, 0, 0, 0]);
        assert_eq!(distinct(&refined), 2);
        assert_eq!(refined[0], refined[1]); // a,b together
        assert_eq!(refined[2], refined[3]); // c,d together
        assert_ne!(refined[0], refined[2]);
    }

    #[test]
    fn leiden_splits_two_cliques_and_keeps_communities_connected() {
        let (names, edges) = two_cliques();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let edge_refs: Vec<(&str, &str)> = edges
            .iter()
            .map(|(s, t)| (s.as_str(), t.as_str()))
            .collect();
        let kg = kg_from(&name_refs, &edge_refs);
        let mut node_ids: Vec<NodeId> = kg.nodes().map(|n| n.id.clone()).collect();
        node_ids.sort();
        let wg = build_wgraph(&kg, &node_ids);
        let labels = leiden(&wg, 1.0);
        assert_eq!(distinct(&labels), 2);
        assert_communities_connected(&wg, &labels);
    }

    #[test]
    fn leiden_communities_always_connected_even_with_articulation_vertex() {
        // Bowtie: triangles {a,b,c} and {c,d,e} sharing the articulation vertex c,
        // plus a pendant magnet community {m0..m3} (K4) that pulls on c via an edge.
        // Whatever the optimizer does, the FINAL refinement guarantees every
        // returned community is internally connected.
        let mut edges: Vec<(String, String)> = vec![
            ("a".into(), "b".into()),
            ("b".into(), "c".into()),
            ("a".into(), "c".into()),
            ("c".into(), "d".into()),
            ("d".into(), "e".into()),
            ("c".into(), "e".into()),
            ("c".into(), "m0".into()),
        ];
        for i in 0..4 {
            for j in (i + 1)..4 {
                edges.push((format!("m{i}"), format!("m{j}")));
            }
        }
        let mut names: Vec<String> = vec!["a", "b", "c", "d", "e"]
            .into_iter()
            .map(String::from)
            .collect();
        names.extend((0..4).map(|i| format!("m{i}")));
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let edge_refs: Vec<(&str, &str)> = edges
            .iter()
            .map(|(s, t)| (s.as_str(), t.as_str()))
            .collect();
        let kg = kg_from(&name_refs, &edge_refs);
        let mut node_ids: Vec<NodeId> = kg.nodes().map(|n| n.id.clone()).collect();
        node_ids.sort();
        let wg = build_wgraph(&kg, &node_ids);
        let labels = leiden(&wg, 1.0);
        assert_communities_connected(&wg, &labels);
    }

    /// Two K4 cliques joined by a single bridge edge.
    fn two_bridged_cliques() -> (Vec<String>, Vec<(String, String)>) {
        let mut names = Vec::new();
        let mut edges = Vec::new();
        for p in ["a", "b"] {
            for i in 0..4 {
                names.push(format!("{p}{i}"));
            }
            for i in 0..4 {
                for j in (i + 1)..4 {
                    edges.push((format!("{p}{i}"), format!("{p}{j}")));
                }
            }
        }
        edges.push(("a0".into(), "b0".into())); // single weak bridge
        (names, edges)
    }

    fn wgraph_from(names: &[String], edges: &[(String, String)]) -> (Vec<NodeId>, WGraph) {
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let edge_refs: Vec<(&str, &str)> = edges
            .iter()
            .map(|(s, t)| (s.as_str(), t.as_str()))
            .collect();
        let kg = kg_from(&name_refs, &edge_refs);
        let mut node_ids: Vec<NodeId> = kg.nodes().map(|n| n.id.clone()).collect();
        node_ids.sort();
        let wg = build_wgraph(&kg, &node_ids);
        (node_ids, wg)
    }

    #[test]
    fn refine_within_splits_a_poorly_knit_single_community() {
        // Both K4 cliques are labeled the SAME community. They're connected (the
        // bridge), so connected-component refinement would keep them as one, but
        // Leiden's within-community refinement splits the weakly-bridged cliques
        // into two well-connected sub-communities.
        let (node_ids, wg) = wgraph_from(&two_bridged_cliques().0, &two_bridged_cliques().1);
        let one_community = vec![0usize; wg.n];
        let refined = refine_within_communities(&wg, &one_community, 1.0);
        assert_eq!(distinct(&refined), 2, "weakly-bridged cliques are split");
        // Every a* shares a sub-community distinct from every b*.
        let sub_of =
            |name: &str| refined[node_ids.iter().position(|n| n.as_str() == name).unwrap()];
        for i in 0..4 {
            assert_eq!(sub_of(&format!("a{i}")), sub_of("a0"));
            assert_eq!(sub_of(&format!("b{i}")), sub_of("b0"));
        }
        assert_ne!(sub_of("a0"), sub_of("b0"));
    }

    #[test]
    fn refine_within_keeps_a_single_clique_whole() {
        // A lone K5 (one community) has no beneficial internal split.
        let mut names: Vec<String> = (0..5).map(|i| format!("n{i}")).collect();
        names.sort();
        let mut edges = Vec::new();
        for i in 0..5 {
            for j in (i + 1)..5 {
                edges.push((format!("n{i}"), format!("n{j}")));
            }
        }
        let (_, wg) = wgraph_from(&names, &edges);
        let refined = refine_within_communities(&wg, &vec![0usize; wg.n], 1.0);
        assert_eq!(distinct(&refined), 1, "a dense clique is not fragmented");
    }

    #[test]
    fn leiden_modularity_never_below_louvain() {
        // The acceptance for the refinement: Leiden must not regress vs Louvain.
        for (names, edges) in [two_cliques(), two_bridged_cliques()] {
            let (_, wg) = wgraph_from(&names, &edges);
            let q_leiden = modularity(&wg, &leiden(&wg, 1.0), 1.0);
            let q_louvain = modularity(&wg, &louvain(&wg, 1.0), 1.0);
            assert!(
                q_leiden >= q_louvain - 1e-9,
                "leiden {q_leiden} must not regress below louvain {q_louvain}"
            );
        }
    }

    #[test]
    fn leiden_is_deterministic_and_connected_on_ring() {
        // Ring of 6 nodes.
        let edges: Vec<(String, String)> = (0..6)
            .map(|i| (format!("n{i}"), format!("n{}", (i + 1) % 6)))
            .collect();
        let names: Vec<String> = (0..6).map(|i| format!("n{i}")).collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let edge_refs: Vec<(&str, &str)> = edges
            .iter()
            .map(|(s, t)| (s.as_str(), t.as_str()))
            .collect();
        let kg = kg_from(&name_refs, &edge_refs);
        let mut node_ids: Vec<NodeId> = kg.nodes().map(|n| n.id.clone()).collect();
        node_ids.sort();
        let wg = build_wgraph(&kg, &node_ids);
        let l1 = leiden(&wg, 1.0);
        let l2 = leiden(&wg, 1.0);
        assert_eq!(l1, l2);
        assert_communities_connected(&wg, &l1);
    }
}
