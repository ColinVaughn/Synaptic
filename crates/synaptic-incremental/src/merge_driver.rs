//! Git merge driver for `graph.json`: when two branches both rebuilt the graph,
//! git invokes `synaptic merge-driver %O %A %B`; we union-compose `%A`
//! (current) and `%B`
//! (other) and write the result back to `%A`, so `graph.json` never produces a
//! textual conflict. The base (`%O`) is unused — a union can't lose nodes.
//!
//! **Fail-loud** (the locked decision): a corrupt or oversized input returns an
//! error so git surfaces a real conflict instead of silently writing garbage.

use std::path::Path;

use synaptic_core::{EdgeKey, EdgeSiteAccumulator, GraphData, Node, NodeId};

/// Errors the merge driver can surface (all fail-loud). The byte and node caps
/// default to 50 MiB / 100k (`synaptic_core::limits`) and honor the
/// `SYNAPTIC_MAX_GRAPH_MB` / `SYNAPTIC_MAX_NODES` overrides.
#[derive(Debug, thiserror::Error)]
pub enum MergeDriverError {
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("{path} is {size} bytes, over the {limit}-byte graph cap (set SYNAPTIC_MAX_GRAPH_MB to raise it; 0 = no cap)")]
    TooBig { path: String, size: u64, limit: u64 },
    #[error("parsing {path} as graph.json: {source}")]
    Parse {
        path: String,
        source: serde_json::Error,
    },
    #[error("merged graph has {count} nodes, over the {limit}-node cap (set SYNAPTIC_MAX_NODES to raise it; 0 = no cap)")]
    TooManyNodes { count: usize, limit: usize },
    #[error("writing {path}: {source}")]
    Write {
        path: String,
        source: std::io::Error,
    },
}

/// Union two graphs. `other` wins on a node-id collision; edges and hyperedges
/// are unioned by identity. `current`'s ordering is preserved, with `other`'s
/// new entries appended — deterministic (`nx.compose` analogue).
pub fn union_graphs(current: GraphData, other: GraphData) -> GraphData {
    union_graphs_many([current, other])
}

/// Union any number of graphs with one set of node/edge/hyperedge indexes.
/// First-seen positions are stable, later node content wins, and metadata comes
/// from the first graph, matching repeated binary union semantics.
pub fn union_graphs_many(graphs: impl IntoIterator<Item = GraphData>) -> GraphData {
    use std::collections::{HashMap, HashSet};

    let mut order: Vec<NodeId> = Vec::new();
    let mut nodes: HashMap<NodeId, Node> = HashMap::new();
    let mut seen: HashMap<EdgeKey, usize> = HashMap::new();
    let mut merged_edges: Vec<synaptic_core::Edge> = Vec::new();
    let mut edge_sites: Vec<Option<EdgeSiteAccumulator>> = Vec::new();
    let mut hseen: HashSet<String> = HashSet::new();
    let mut merged_hyper = Vec::new();
    let mut directed = false;
    let mut multigraph = false;
    let mut graph_meta = serde_json::Map::new();
    let mut built_at_commit = None;
    let mut first = true;

    for graph in graphs {
        let GraphData {
            directed: graph_directed,
            multigraph: graph_multigraph,
            graph,
            nodes: graph_nodes,
            links,
            hyperedges,
            built_at_commit: graph_commit,
        } = graph;

        if first {
            multigraph = graph_multigraph;
            graph_meta = graph;
            first = false;
        }
        directed |= graph_directed;
        if built_at_commit.is_none() {
            built_at_commit = graph_commit;
        }

        for node in graph_nodes {
            if let Some(existing) = nodes.get_mut(&node.id) {
                *existing = node;
            } else {
                order.push(node.id.clone());
                nodes.insert(node.id.clone(), node);
            }
        }
        for edge in links {
            let key = EdgeKey::new(&edge, true);
            if let Some(&index) = seen.get(&key) {
                if edge_sites[index].is_none() {
                    edge_sites[index] = Some(EdgeSiteAccumulator::new(&merged_edges[index]));
                }
                edge_sites[index]
                    .as_mut()
                    .expect("duplicate edge has a site accumulator")
                    .include_edge(&edge);
            } else {
                seen.insert(key, merged_edges.len());
                merged_edges.push(edge);
                edge_sites.push(None);
            }
        }
        for hyperedge in hyperedges {
            if hseen.insert(hyperedge.id.clone()) {
                merged_hyper.push(hyperedge);
            }
        }
    }

    for (edge, sites) in merged_edges.iter_mut().zip(edge_sites) {
        if let Some(sites) = sites {
            sites.apply_to(edge);
        }
    }

    GraphData {
        directed,
        multigraph,
        graph: graph_meta,
        nodes: order
            .into_iter()
            .filter_map(|id| nodes.remove(&id))
            .collect(),
        links: merged_edges,
        hyperedges: merged_hyper,
        built_at_commit,
    }
}

fn load(path: &Path, byte_cap: u64) -> Result<GraphData, MergeDriverError> {
    let label = path.display().to_string();
    let meta = std::fs::metadata(path).map_err(|source| MergeDriverError::Read {
        path: label.clone(),
        source,
    })?;
    if meta.len() > byte_cap {
        return Err(MergeDriverError::TooBig {
            path: label,
            size: meta.len(),
            limit: byte_cap,
        });
    }
    let bytes = std::fs::read(path).map_err(|source| MergeDriverError::Read {
        path: label.clone(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| MergeDriverError::Parse {
        path: label,
        source,
    })
}

/// Run the git merge driver: union `current` and `other`, write the result back
/// to `current`. Returns the merged node count. Caps come from
/// `synaptic_core::limits` (env-overridable).
pub fn run_merge_driver(current: &Path, other: &Path) -> Result<usize, MergeDriverError> {
    merge_with_caps(
        current,
        other,
        synaptic_core::max_graph_bytes(),
        synaptic_core::max_nodes(),
    )
}

fn merge_with_caps(
    current: &Path,
    other: &Path,
    byte_cap: u64,
    node_cap: usize,
) -> Result<usize, MergeDriverError> {
    let cur = load(current, byte_cap)?;
    let oth = load(other, byte_cap)?;
    let merged = union_graphs(cur, oth);
    if merged.nodes.len() > node_cap {
        return Err(MergeDriverError::TooManyNodes {
            count: merged.nodes.len(),
            limit: node_cap,
        });
    }
    let n = merged.nodes.len();
    let bytes = serde_json::to_vec_pretty(&merged).map_err(|source| MergeDriverError::Parse {
        path: current.display().to_string(),
        source,
    })?;
    std::fs::write(current, bytes).map_err(|source| MergeDriverError::Write {
        path: current.display().to_string(),
        source,
    })?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Confidence, Edge, FileType};

    fn node(id: &str, label: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: format!("{id}.py"),
            source_location: None,
            community: None,
            repo: None,
            extra: Map::new(),
        }
    }

    fn edge(s: &str, t: &str) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            source_file: "x".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    fn gd(nodes: Vec<Node>, links: Vec<Edge>) -> GraphData {
        GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes,
            links,
            hyperedges: vec![],
            built_at_commit: None,
        }
    }

    #[test]
    fn union_takes_the_superset_of_both_branches() {
        // Branch A added node `a`; branch B added node `b`. Union has both.
        let a = gd(
            vec![node("shared", "S"), node("a", "A")],
            vec![edge("a", "shared")],
        );
        let b = gd(
            vec![node("shared", "S"), node("b", "B")],
            vec![edge("b", "shared")],
        );
        let m = union_graphs(a, b);
        let ids: Vec<&str> = m.nodes.iter().map(|n| n.id.0.as_str()).collect();
        assert!(ids.contains(&"a") && ids.contains(&"b") && ids.contains(&"shared"));
        assert_eq!(ids.iter().filter(|i| **i == "shared").count(), 1, "no dup");
        assert_eq!(m.links.len(), 2, "both branches' edges unioned");
    }

    #[test]
    fn other_wins_on_node_collision() {
        let a = gd(vec![node("x", "OLD")], vec![]);
        let mut b = gd(vec![node("x", "NEW")], vec![]);
        b.nodes[0].community = Some(7);
        let m = union_graphs(a, b);
        assert_eq!(m.nodes.len(), 1);
        assert_eq!(m.nodes[0].label, "NEW", "other branch wins the collision");
        assert_eq!(m.nodes[0].community, Some(7));
    }

    #[test]
    fn union_keeps_distinct_contexts_and_merges_duplicate_sites() {
        let mut get = edge("a", "b");
        get.context = Some("GET".into());
        get.source_location = Some("L1".into());
        let mut post = get.clone();
        post.context = Some("POST".into());
        let mut second_get = get.clone();
        second_get.source_location = Some("L2".into());

        let merged = union_graphs(
            gd(vec![node("a", "A"), node("b", "B")], vec![get]),
            gd(vec![], vec![post, second_get]),
        );

        assert_eq!(merged.links.len(), 2);
        let get = merged
            .links
            .iter()
            .find(|edge| edge.context.as_deref() == Some("GET"))
            .unwrap();
        assert_eq!(get.sites().len(), 2);
    }

    #[test]
    fn union_many_preserves_fold_order_and_metadata() {
        let mut first = gd(vec![node("shared", "first"), node("a", "A")], vec![]);
        first
            .graph
            .insert("owner".into(), serde_json::json!("first"));
        first.built_at_commit = Some("abc".into());
        let second = gd(vec![node("shared", "second"), node("b", "B")], vec![]);
        let third = gd(vec![node("shared", "third"), node("c", "C")], vec![]);

        let merged = union_graphs_many(vec![first, second, third]);

        let ids: Vec<_> = merged.nodes.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(ids, ["shared", "a", "b", "c"]);
        assert_eq!(merged.nodes[0].label, "third");
        assert_eq!(merged.graph["owner"], "first");
        assert_eq!(merged.built_at_commit.as_deref(), Some("abc"));
    }

    #[test]
    fn union_many_empty_is_default() {
        assert_eq!(
            union_graphs_many(Vec::<GraphData>::new()),
            GraphData::default()
        );
    }

    #[test]
    fn run_merge_driver_writes_union_to_current() {
        let dir = tempfile::tempdir().unwrap();
        let cur = dir.path().join("current.json");
        let oth = dir.path().join("other.json");
        std::fs::write(
            &cur,
            serde_json::to_vec(&gd(vec![node("a", "A")], vec![])).unwrap(),
        )
        .unwrap();
        std::fs::write(
            &oth,
            serde_json::to_vec(&gd(vec![node("b", "B")], vec![])).unwrap(),
        )
        .unwrap();
        let n = run_merge_driver(&cur, &oth).unwrap();
        assert_eq!(n, 2);
        let merged: GraphData = serde_json::from_slice(&std::fs::read(&cur).unwrap()).unwrap();
        let ids: Vec<&str> = merged.nodes.iter().map(|n| n.id.0.as_str()).collect();
        assert!(
            ids.contains(&"a") && ids.contains(&"b"),
            "union written to current"
        );
    }

    #[test]
    fn over_node_cap_merge_fails_with_env_hint() {
        let dir = tempfile::tempdir().unwrap();
        let cur = dir.path().join("current.json");
        let oth = dir.path().join("other.json");
        std::fs::write(
            &cur,
            serde_json::to_vec(&gd(vec![node("a", "A")], vec![])).unwrap(),
        )
        .unwrap();
        std::fs::write(
            &oth,
            serde_json::to_vec(&gd(vec![node("b", "B")], vec![])).unwrap(),
        )
        .unwrap();
        let msg = merge_with_caps(&cur, &oth, u64::MAX, 1)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("SYNAPTIC_MAX_NODES"), "{msg}");
        assert!(msg.contains('2'), "merged count in message: {msg}");
    }

    #[test]
    fn oversized_input_fails_with_env_hint() {
        let dir = tempfile::tempdir().unwrap();
        let cur = dir.path().join("current.json");
        let oth = dir.path().join("other.json");
        std::fs::write(
            &cur,
            serde_json::to_vec(&gd(vec![node("a", "A")], vec![])).unwrap(),
        )
        .unwrap();
        std::fs::write(&oth, serde_json::to_vec(&gd(vec![], vec![])).unwrap()).unwrap();
        let msg = merge_with_caps(&cur, &oth, 10, usize::MAX)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("SYNAPTIC_MAX_GRAPH_MB"), "{msg}");
    }

    #[test]
    fn corrupt_input_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        let cur = dir.path().join("current.json");
        let oth = dir.path().join("other.json");
        std::fs::write(&cur, b"{ not json").unwrap();
        std::fs::write(&oth, serde_json::to_vec(&gd(vec![], vec![])).unwrap()).unwrap();
        assert!(
            run_merge_driver(&cur, &oth).is_err(),
            "corrupt → error, not silent"
        );
    }
}
