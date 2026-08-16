//! Print the serialized `QueryIndex` for a fixed graph.
//!
//! Used to diff the persisted shard-index format across a change to the index's
//! in-memory representation: the bytes must not move, or previously written
//! index blobs would be misread.
//!
//! ```text
//! cargo run --release -p synaptic-query --example wireformat
//! ```

use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};
use synaptic_graph::KnowledgeGraph;
use synaptic_query::QueryIndex;

fn node(id: &str, label: &str, file: &str) -> Node {
    Node {
        id: NodeId(id.into()),
        label: label.into(),
        file_type: FileType::Code,
        source_file: file.into(),
        source_location: Some("L1".into()),
        ..Default::default()
    }
}

fn edge(source: &str, target: &str, relation: &str) -> Edge {
    Edge {
        source: NodeId(source.into()),
        target: NodeId(target.into()),
        relation: relation.into(),
        confidence: Confidence::Extracted,
        source_file: "src/app.rs".into(),
        source_location: Some("L2".into()),
        confidence_score: None,
        weight: 1.0,
        context: None,
        cross_repo: false,
        extra: Default::default(),
    }
}

fn main() {
    // Deliberately unsorted ids, repeated labels, an isolated node and a
    // self-loop, so ordering, dedup and the repeated-label prior all show up.
    let gd = GraphData {
        directed: true,
        nodes: vec![
            node("zeta", "render", "src/ui/zeta.rs"),
            node("alpha", "AuthService", "src/auth/service.rs"),
            node("mid", "render", "src/ui/mid.rs"),
            node("beta", "login_user", "src/auth/login.rs"),
            node("orphan", "Unlinked", "src/misc/orphan.rs"),
        ],
        links: vec![
            edge("beta", "alpha", "calls"),
            edge("zeta", "alpha", "calls"),
            edge("mid", "zeta", "imports"),
            edge("alpha", "alpha", "calls"),
        ],
        ..Default::default()
    };

    let kg = KnowledgeGraph::from_graph_data(gd);

    let index = QueryIndex::build(&kg);
    let bytes = index.to_bytes().expect("index serializes");
    println!("{}", String::from_utf8(bytes).expect("utf8"));

    let reverse =
        synaptic_query::ReverseImpactIndex::build(&kg, synaptic_query::DEFAULT_AFFECTED_RELATIONS);
    let bytes = reverse.to_bytes().expect("reverse index serializes");
    println!("{}", String::from_utf8(bytes).expect("utf8"));
}
