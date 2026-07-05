//! Shard write -> read round-trips a `GraphData` losslessly and *in order*.

use serde_json::Map;
use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};
use synaptic_store::shard;

fn node(id: &str, sf: &str) -> Node {
    Node {
        id: NodeId(id.into()),
        label: id.into(),
        file_type: FileType::Code,
        source_file: sf.into(),
        source_location: Some("L1".into()),
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
        source_file: format!("{s}.rs"),
        source_location: None,
        confidence_score: None,
        weight: 1.0,
        context: None,
        cross_repo: false,
        extra: Map::new(),
    }
}

fn sample() -> GraphData {
    GraphData {
        directed: true,
        multigraph: false,
        graph: Map::new(),
        nodes: vec![node("a", "src/a.rs"), node("b", "src/b.rs")],
        links: vec![edge("a", "b")],
        hyperedges: vec![],
        built_at_commit: Some("deadbeef".into()),
    }
}

#[test]
fn write_then_read_reconstructs_graph_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("local.redb");
    let gd = sample();
    shard::write(&path, &gd).unwrap();
    let back = shard::read_graph_data(&path).unwrap();
    assert_eq!(back, gd, "shard round-trip must be lossless");
}

#[test]
fn preserves_node_and_link_order() {
    // reverse-sorted ids: an id-keyed B-tree store would reorder them, which
    // would break byte-identical export. Sequence-keyed storage must not.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("local.redb");
    let gd = GraphData {
        directed: false,
        multigraph: false,
        graph: Map::new(),
        nodes: vec![node("z", "z.rs"), node("a", "a.rs"), node("m", "m.rs")],
        links: vec![edge("z", "a"), edge("a", "m")],
        hyperedges: vec![],
        built_at_commit: None,
    };
    shard::write(&path, &gd).unwrap();
    let back = shard::read_graph_data(&path).unwrap();
    let ids: Vec<String> = back.nodes.iter().map(|n| n.id.0.clone()).collect();
    assert_eq!(ids, vec!["z", "a", "m"], "node order must be preserved");
    assert_eq!(back, gd);
}

#[test]
fn materialize_matches_from_graph_data() {
    use synaptic_graph::KnowledgeGraph;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.redb");
    let gd = sample();
    shard::write(&path, &gd).unwrap();

    let from_store = shard::materialize(&path).unwrap();
    let direct = KnowledgeGraph::from_graph_data(gd.clone());

    assert_eq!(from_store.node_count(), direct.node_count());
    assert_eq!(from_store.edge_count(), direct.edge_count());

    let dump = |kg: &KnowledgeGraph| {
        let mut ns: Vec<(String, String, String)> = kg
            .nodes()
            .map(|n| (n.id.0.clone(), n.label.clone(), n.source_file.clone()))
            .collect();
        ns.sort();
        let mut es: Vec<(String, String, String)> = kg
            .edges()
            .map(|e| (e.source.0.clone(), e.target.0.clone(), e.relation.clone()))
            .collect();
        es.sort();
        (ns, es)
    };
    assert_eq!(dump(&from_store), dump(&direct));
}

#[test]
fn empty_graph_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.redb");
    let gd = GraphData::default();
    shard::write(&path, &gd).unwrap();
    let back = shard::read_graph_data(&path).unwrap();
    assert_eq!(back, gd);
}
