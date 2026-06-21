//! Community clustering: the public `cluster()` entry point plus `cohesion_score`,
//! `remap_communities_to_previous`, and `apply_communities`. Wraps the in-house
//! Louvain/Leiden algorithms in `crate::community` with
//! post-processing (hub exclusion, splitting, deterministic renumber).

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};

use synaptic_core::NodeId;

use crate::community::{build_wgraph, leiden, louvain};
use crate::graph::KnowledgeGraph;

const MAX_COMMUNITY_FRACTION: f64 = 0.25;
const MIN_SPLIT_SIZE: usize = 10;
const COHESION_SPLIT_THRESHOLD: f64 = 0.05;
const COHESION_SPLIT_MIN_SIZE: usize = 50;

/// Community-detection algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Algorithm {
    Louvain,
    /// Default — chosen for Leiden's connectivity guarantee.
    #[default]
    Leiden,
}

/// Options for [`cluster`].
#[derive(Debug, Clone)]
pub struct ClusterOptions {
    /// Resolution: `>1.0` → more, smaller communities; `<1.0` → fewer, larger.
    pub resolution: f64,
    pub algorithm: Algorithm,
    /// If set (0-100), nodes whose degree exceeds this percentile are excluded
    /// from partitioning and reattached by majority-vote neighbour community.
    pub exclude_hubs_percentile: Option<f64>,
}

impl Default for ClusterOptions {
    fn default() -> Self {
        ClusterOptions {
            resolution: 1.0,
            algorithm: Algorithm::Leiden,
            exclude_hubs_percentile: None,
        }
    }
}

/// Ratio of actual intra-community edges to the maximum possible. `n<=1 → 1.0`.
pub fn cohesion_score(kg: &KnowledgeGraph, nodes: &[NodeId]) -> f64 {
    let n = nodes.len();
    if n <= 1 {
        return 1.0;
    }
    let set: HashSet<&NodeId> = nodes.iter().collect();
    let mut pairs: HashSet<(&NodeId, &NodeId)> = HashSet::new();
    for e in kg.edges() {
        if e.source == e.target {
            continue;
        }
        if set.contains(&e.source) && set.contains(&e.target) {
            let key = if e.source <= e.target {
                (&e.source, &e.target)
            } else {
                (&e.target, &e.source)
            };
            pairs.insert(key);
        }
    }
    let actual = pairs.len() as f64;
    let possible = (n * (n - 1)) as f64 / 2.0;
    if possible > 0.0 {
        actual / possible
    } else {
        0.0
    }
}

fn undirected_neighbors(kg: &KnowledgeGraph) -> HashMap<NodeId, HashSet<NodeId>> {
    let mut m: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
    for n in kg.nodes() {
        m.entry(n.id.clone()).or_default();
    }
    for e in kg.edges() {
        if e.source == e.target {
            continue;
        }
        m.entry(e.source.clone())
            .or_default()
            .insert(e.target.clone());
        m.entry(e.target.clone())
            .or_default()
            .insert(e.source.clone());
    }
    m
}

fn partition_into_groups(
    kg: &KnowledgeGraph,
    nodes: &[NodeId],
    opts: &ClusterOptions,
) -> Vec<Vec<NodeId>> {
    if nodes.is_empty() {
        return Vec::new();
    }
    let wg = build_wgraph(kg, nodes);
    let labels = match opts.algorithm {
        Algorithm::Leiden => leiden(&wg, opts.resolution),
        Algorithm::Louvain => louvain(&wg, opts.resolution),
    };
    let mut groups: BTreeMap<usize, Vec<NodeId>> = BTreeMap::new();
    for (i, &lab) in labels.iter().enumerate() {
        groups.entry(lab).or_default().push(nodes[i].clone());
    }
    groups.into_values().collect()
}

/// Re-partition a community's subgraph. No edges → one singleton each; a partition
/// that doesn't split → the whole community unchanged.
fn split_community(
    kg: &KnowledgeGraph,
    nodes: &[NodeId],
    opts: &ClusterOptions,
) -> Vec<Vec<NodeId>> {
    let mut sorted = nodes.to_vec();
    sorted.sort();
    let groups = partition_into_groups(kg, &sorted, opts);
    if groups.len() <= 1 {
        return vec![sorted];
    }
    groups
        .into_iter()
        .map(|mut g| {
            g.sort();
            g
        })
        .collect()
}

/// Run community detection. Returns `{community_id: [node_ids]}` with `0` = the
/// largest community. Deterministic.
pub fn cluster(kg: &KnowledgeGraph, opts: &ClusterOptions) -> BTreeMap<u32, Vec<NodeId>> {
    let mut all_nodes: Vec<NodeId> = kg.nodes().map(|n| n.id.clone()).collect();
    all_nodes.sort();
    let n = all_nodes.len();
    if n == 0 {
        return BTreeMap::new();
    }
    if kg.edge_count() == 0 {
        return all_nodes
            .into_iter()
            .enumerate()
            .map(|(i, id)| (i as u32, vec![id]))
            .collect();
    }

    let neighbors = undirected_neighbors(kg);
    let degree = |id: &NodeId| neighbors.get(id).map_or(0, HashSet::len);

    // Hub exclusion.
    let mut hubs: HashSet<NodeId> = HashSet::new();
    if let Some(pct) = opts.exclude_hubs_percentile {
        let mut degs: Vec<usize> = all_nodes.iter().map(&degree).collect();
        degs.sort_unstable();
        if !degs.is_empty() {
            let raw_idx = (n as f64 * pct / 100.0) as usize;
            let idx = raw_idx.saturating_sub(1).min(degs.len() - 1);
            let threshold = degs[idx];
            for id in &all_nodes {
                if degree(id) > threshold {
                    hubs.insert(id.clone());
                }
            }
        }
    }

    let isolates: Vec<NodeId> = all_nodes
        .iter()
        .filter(|id| degree(id) == 0 && !hubs.contains(*id))
        .cloned()
        .collect();
    let connected: Vec<NodeId> = all_nodes
        .iter()
        .filter(|id| degree(id) > 0 && !hubs.contains(*id))
        .cloned()
        .collect();

    let mut raw: Vec<Vec<NodeId>> = partition_into_groups(kg, &connected, opts);
    for iso in &isolates {
        raw.push(vec![iso.clone()]);
    }

    // Reattach excluded hubs by majority-vote neighbour community.
    if !hubs.is_empty() {
        let mut node_comm: HashMap<NodeId, usize> = HashMap::new();
        for (ci, grp) in raw.iter().enumerate() {
            for nd in grp {
                node_comm.insert(nd.clone(), ci);
            }
        }
        let mut hubs_sorted: Vec<NodeId> = hubs.iter().cloned().collect();
        hubs_sorted.sort();
        for hub in hubs_sorted {
            let mut votes: HashMap<usize, usize> = HashMap::new();
            if let Some(nbrs) = neighbors.get(&hub) {
                for nb in nbrs {
                    if let Some(&c) = node_comm.get(nb) {
                        *votes.entry(c).or_insert(0) += 1;
                    }
                }
            }
            if votes.is_empty() {
                let cid = raw.len();
                raw.push(vec![hub.clone()]);
                node_comm.insert(hub, cid);
            } else {
                // max votes, tie breaks to smallest community id.
                let best = *votes
                    .iter()
                    .min_by_key(|(c, v)| (Reverse(**v), **c))
                    .map(|(c, _)| c)
                    .expect("votes non-empty in tie branch");
                raw[best].push(hub.clone());
                node_comm.insert(hub, best);
            }
        }
    }

    // Split oversized communities.
    let max_size = MIN_SPLIT_SIZE.max((n as f64 * MAX_COMMUNITY_FRACTION) as usize);
    let mut after_size: Vec<Vec<NodeId>> = Vec::new();
    for grp in raw {
        if grp.len() > max_size {
            after_size.extend(split_community(kg, &grp, opts));
        } else {
            after_size.push(grp);
        }
    }

    // Re-split low-cohesion communities.
    let mut after_coh: Vec<Vec<NodeId>> = Vec::new();
    for grp in after_size {
        if grp.len() >= COHESION_SPLIT_MIN_SIZE
            && cohesion_score(kg, &grp) < COHESION_SPLIT_THRESHOLD
        {
            let splits = split_community(kg, &grp, opts);
            if splits.len() > 1 {
                after_coh.extend(splits);
            } else {
                after_coh.push(grp);
            }
        } else {
            after_coh.push(grp);
        }
    }

    // Deterministic renumber: sort by (-len, sorted node ids), assign 0..
    let mut final_comms = after_coh;
    for g in &mut final_comms {
        g.sort();
    }
    final_comms.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    final_comms
        .into_iter()
        .enumerate()
        .map(|(i, g)| (i as u32, g))
        .collect()
}

/// Remap community ids to maximise overlap with a previous assignment, then
/// assign fresh ids to unmatched communities in deterministic order. Assumes each
/// community's node list is sorted (as produced by [`cluster`]).
pub fn remap_communities_to_previous(
    communities: &BTreeMap<u32, Vec<NodeId>>,
    previous: &HashMap<NodeId, u32>,
) -> BTreeMap<u32, Vec<NodeId>> {
    if communities.is_empty() {
        return BTreeMap::new();
    }
    let new_sets: BTreeMap<u32, HashSet<&NodeId>> = communities
        .iter()
        .map(|(c, v)| (*c, v.iter().collect()))
        .collect();
    let mut old_sets: HashMap<u32, HashSet<&NodeId>> = HashMap::new();
    for (node, oc) in previous {
        old_sets.entry(*oc).or_default().insert(node);
    }

    let mut overlaps: Vec<(usize, u32, u32)> = Vec::new();
    for (oc, oset) in &old_sets {
        for (nc, nset) in &new_sets {
            let ov = oset.intersection(nset).count();
            if ov > 0 {
                overlaps.push((ov, *oc, *nc));
            }
        }
    }
    overlaps.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));

    let mut new_to_final: HashMap<u32, u32> = HashMap::new();
    let mut used_old: HashSet<u32> = HashSet::new();
    let mut matched_new: HashSet<u32> = HashSet::new();
    for (_ov, oc, nc) in &overlaps {
        if used_old.contains(oc) || matched_new.contains(nc) {
            continue;
        }
        new_to_final.insert(*nc, *oc);
        used_old.insert(*oc);
        matched_new.insert(*nc);
    }

    let mut unmatched: Vec<u32> = communities
        .keys()
        .filter(|c| !matched_new.contains(c))
        .copied()
        .collect();
    unmatched.sort_by(|a, b| {
        communities[b]
            .len()
            .cmp(&communities[a].len())
            .then_with(|| communities[a].cmp(&communities[b]))
    });
    let mut next_id = 0u32;
    for nc in unmatched {
        while used_old.contains(&next_id) {
            next_id += 1;
        }
        new_to_final.insert(nc, next_id);
        used_old.insert(next_id);
        next_id += 1;
    }

    let mut remapped: BTreeMap<u32, Vec<NodeId>> = BTreeMap::new();
    for (nc, nodes) in communities {
        let mut v = nodes.clone();
        v.sort();
        remapped.insert(new_to_final[nc], v);
    }
    remapped
}

/// Write each node's community id onto its `Node.community` field.
pub fn apply_communities(kg: &mut KnowledgeGraph, communities: &BTreeMap<u32, Vec<NodeId>>) {
    for (cid, nodes) in communities {
        for id in nodes {
            if let Some(node) = kg.node_mut(id) {
                node.community = Some(*cid);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community::{ids, kg_from};

    fn comms(pairs: &[(u32, &[&str])]) -> BTreeMap<u32, Vec<NodeId>> {
        pairs.iter().map(|(c, ns)| (*c, ids(ns))).collect()
    }

    fn two_clique_kg() -> KnowledgeGraph {
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
        edges.push(("a0".into(), "b0".into()));
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let edge_refs: Vec<(&str, &str)> = edges
            .iter()
            .map(|(s, t)| (s.as_str(), t.as_str()))
            .collect();
        kg_from(&name_refs, &edge_refs)
    }

    fn coverage(comms: &BTreeMap<u32, Vec<NodeId>>) -> HashSet<NodeId> {
        comms.values().flatten().cloned().collect::<HashSet<_>>()
    }

    fn all_ids(kg: &KnowledgeGraph) -> HashSet<NodeId> {
        kg.nodes().map(|n| n.id.clone()).collect()
    }

    #[test]
    fn empty_graph_yields_empty() {
        let kg = kg_from(&[], &[]);
        assert!(cluster(&kg, &ClusterOptions::default()).is_empty());
    }

    #[test]
    fn no_edges_yields_singletons() {
        let kg = kg_from(&["a", "b", "c"], &[]);
        let comms = cluster(&kg, &ClusterOptions::default());
        assert_eq!(comms.len(), 3);
        assert_eq!(coverage(&comms), all_ids(&kg));
    }

    #[test]
    fn two_cliques_two_communities_both_algorithms() {
        let kg = two_clique_kg();
        for algo in [Algorithm::Leiden, Algorithm::Louvain] {
            let opts = ClusterOptions {
                algorithm: algo,
                ..Default::default()
            };
            let comms = cluster(&kg, &opts);
            assert_eq!(comms.len(), 2, "{algo:?} should yield 2 communities");
            assert_eq!(coverage(&comms), all_ids(&kg));
        }
    }

    #[test]
    fn community_0_is_largest() {
        // Make community A bigger by adding more nodes to it.
        let kg = kg_from(
            &["a0", "a1", "a2", "b0", "b1"],
            &[("a0", "a1"), ("a1", "a2"), ("a0", "a2"), ("b0", "b1")],
        );
        let comms = cluster(&kg, &ClusterOptions::default());
        let sizes: Vec<usize> = comms.values().map(Vec::len).collect();
        // id 0 has the most nodes.
        assert!(comms[&0].len() >= *sizes.iter().max().unwrap());
    }

    #[test]
    fn cluster_is_deterministic() {
        let kg = two_clique_kg();
        let a = cluster(&kg, &ClusterOptions::default());
        let b = cluster(&kg, &ClusterOptions::default());
        assert_eq!(a, b);
    }

    #[test]
    fn split_community_splits_two_cliques() {
        let kg = two_clique_kg();
        let all: Vec<NodeId> = {
            let mut v: Vec<NodeId> = kg.nodes().map(|n| n.id.clone()).collect();
            v.sort();
            v
        };
        let parts = split_community(&kg, &all, &ClusterOptions::default());
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn hub_exclusion_separates_bridged_cliques() {
        // Two K5 cliques, each connected only through a central hub H.
        let mut names = vec!["H".to_string()];
        let mut edges = Vec::new();
        for p in ["a", "b"] {
            for i in 0..5 {
                let name = format!("{p}{i}");
                edges.push(("H".to_string(), name.clone()));
                names.push(name);
            }
            for i in 0..5 {
                for j in (i + 1)..5 {
                    edges.push((format!("{p}{i}"), format!("{p}{j}")));
                }
            }
        }
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let edge_refs: Vec<(&str, &str)> = edges
            .iter()
            .map(|(s, t)| (s.as_str(), t.as_str()))
            .collect();
        let kg = kg_from(&name_refs, &edge_refs);

        let opts = ClusterOptions {
            exclude_hubs_percentile: Some(90.0),
            ..Default::default()
        };
        let comms = cluster(&kg, &opts);
        // H excluded, so A and B are disconnected: 2 communities; H reattached.
        assert_eq!(comms.len(), 2);
        assert_eq!(coverage(&comms), all_ids(&kg));
        assert!(coverage(&comms).contains(&NodeId("H".into())));
    }

    #[test]
    fn remap_reuses_overlapping_old_ids() {
        // Overlapping communities reuse their previous ids.
        let communities = comms(&[(10, &["a", "b", "c"]), (11, &["d", "e"])]);
        let previous: HashMap<NodeId, u32> = [("a", 5u32), ("b", 5), ("c", 5), ("d", 1), ("e", 1)]
            .iter()
            .map(|(n, c)| (NodeId(n.to_string()), *c))
            .collect();
        let remapped = remap_communities_to_previous(&communities, &previous);
        assert_eq!(remapped.keys().copied().collect::<Vec<_>>(), vec![1, 5]);
        assert_eq!(remapped[&5], ids(&["a", "b", "c"]));
        assert_eq!(remapped[&1], ids(&["d", "e"]));
    }

    #[test]
    fn remap_assigns_deterministic_new_ids_when_no_overlap() {
        // With no overlap, fresh ids are assigned in deterministic order.
        let communities = comms(&[(7, &["x", "y", "z"]), (8, &["m"])]);
        let previous: HashMap<NodeId, u32> = [(NodeId("a".into()), 3u32)].into_iter().collect();
        let remapped = remap_communities_to_previous(&communities, &previous);
        assert_eq!(remapped.keys().copied().collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(remapped[&0], ids(&["x", "y", "z"]));
        assert_eq!(remapped[&1], ids(&["m"]));
    }

    #[test]
    fn apply_sets_community_on_every_node() {
        let kg0 = two_clique_kg();
        let comms = cluster(&kg0, &ClusterOptions::default());
        let mut kg = kg0;
        apply_communities(&mut kg, &comms);
        for node in kg.nodes() {
            assert!(node.community.is_some(), "{} has no community", node.id);
        }
        // Round-trips through GraphData.
        let gd = kg.to_graph_data();
        assert!(gd.nodes.iter().all(|n| n.community.is_some()));
    }
}
