//! Git merge driver for `graph.json`: when two branches both rebuilt the graph,
//! git invokes `synaptic merge-driver %O %A %B`; we union-compose `%A`
//! (current) and `%B`
//! (other) and write the result back to `%A`, so `graph.json` never produces a
//! textual conflict. The base (`%O`) is unused — a union can't lose nodes.
//!
//! **Fail-loud** (the locked decision): a corrupt or oversized input returns an
//! error so git surfaces a real conflict instead of silently writing garbage.

use std::path::Path;

use synaptic_core::{GraphData, Node, NodeId};

/// Refuse inputs larger than this (memory-bomb / runaway guard): a 50 MB cap.
const MAX_GRAPH_BYTES: u64 = 50 * 1024 * 1024;
/// Refuse graphs with more nodes than this: a 100k cap.
const MAX_NODES: usize = 100_000;

/// Errors the merge driver can surface (all fail-loud).
#[derive(Debug, thiserror::Error)]
pub enum MergeDriverError {
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("{path} exceeds the {MAX_GRAPH_BYTES}-byte cap ({size} bytes)")]
    TooBig { path: String, size: u64 },
    #[error("parsing {path} as graph.json: {source}")]
    Parse {
        path: String,
        source: serde_json::Error,
    },
    #[error("merged graph has {0} nodes, over the {MAX_NODES} cap")]
    TooManyNodes(usize),
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
    use std::collections::{HashMap, HashSet};

    // Nodes: keep first-seen position; let `other` overwrite content on collision.
    let mut order: Vec<NodeId> = Vec::new();
    let mut nodes: HashMap<NodeId, Node> = HashMap::new();
    for n in current.nodes.into_iter().chain(other.nodes) {
        if !nodes.contains_key(&n.id) {
            order.push(n.id.clone());
        }
        nodes.insert(n.id.clone(), n);
    }
    let merged_nodes: Vec<Node> = order
        .into_iter()
        .filter_map(|id| nodes.remove(&id))
        .collect();

    // Edges: union by (source, target, relation), first occurrence kept.
    let mut seen: HashSet<(NodeId, NodeId, String)> = HashSet::new();
    let mut merged_edges = Vec::new();
    for e in current.links.into_iter().chain(other.links) {
        if seen.insert((e.source.clone(), e.target.clone(), e.relation.clone())) {
            merged_edges.push(e);
        }
    }

    // Hyperedges: union by id.
    let mut hseen: HashSet<String> = HashSet::new();
    let mut merged_hyper = Vec::new();
    for h in current.hyperedges.into_iter().chain(other.hyperedges) {
        if hseen.insert(h.id.clone()) {
            merged_hyper.push(h);
        }
    }

    GraphData {
        directed: current.directed || other.directed,
        multigraph: current.multigraph,
        graph: current.graph,
        nodes: merged_nodes,
        links: merged_edges,
        hyperedges: merged_hyper,
        built_at_commit: current.built_at_commit.or(other.built_at_commit),
    }
}

fn load(path: &Path) -> Result<GraphData, MergeDriverError> {
    let label = path.display().to_string();
    let meta = std::fs::metadata(path).map_err(|source| MergeDriverError::Read {
        path: label.clone(),
        source,
    })?;
    if meta.len() > MAX_GRAPH_BYTES {
        return Err(MergeDriverError::TooBig {
            path: label,
            size: meta.len(),
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
/// to `current`. Returns the merged node count.
pub fn run_merge_driver(current: &Path, other: &Path) -> Result<usize, MergeDriverError> {
    let cur = load(current)?;
    let oth = load(other)?;
    let merged = union_graphs(cur, oth);
    if merged.nodes.len() > MAX_NODES {
        return Err(MergeDriverError::TooManyNodes(merged.nodes.len()));
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
