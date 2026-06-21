//! Shared fixtures for the writer test modules.

use serde_json::Map;
use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};
use synaptic_graph::{apply_communities, cluster, ClusterOptions, KnowledgeGraph};

fn node(id: &str, label: &str, sf: &str) -> Node {
    Node {
        id: NodeId(id.into()),
        label: label.into(),
        file_type: FileType::Code,
        source_file: sf.into(),
        source_location: Some("L1".into()),
        community: None,
        repo: None,
        extra: Map::new(),
    }
}

fn edge(s: &str, t: &str, c: Confidence) -> Edge {
    Edge {
        source: NodeId(s.into()),
        target: NodeId(t.into()),
        relation: "calls".into(),
        confidence: c,
        source_file: "a.py".into(),
        source_location: Some("L1".into()),
        confidence_score: None,
        weight: 1.0,
        context: None,
        cross_repo: false,
        extra: Map::new(),
    }
}

/// A tiny clustered graph: a→b→c (one Inferred, one Extracted `calls` edge).
pub(crate) fn sample_kg() -> KnowledgeGraph {
    let gd = GraphData {
        directed: false,
        multigraph: false,
        graph: Map::new(),
        nodes: vec![
            node("a", "A", "a.py"),
            node("b", "B", "b.py"),
            node("c", "C", "c.py"),
        ],
        links: vec![
            edge("a", "b", Confidence::Inferred),
            edge("b", "c", Confidence::Extracted),
        ],
        hyperedges: vec![],
        built_at_commit: Some("deadbeef".into()),
    };
    let mut kg = KnowledgeGraph::from_graph_data(gd);
    let comms = cluster(&kg, &ClusterOptions::default());
    apply_communities(&mut kg, &comms);
    kg
}

/// Two nodes (caller-supplied labels) with `id1 → id2` — for link/filename tests.
pub(crate) fn kg_two_linked(id1: &str, l1: &str, id2: &str, l2: &str) -> KnowledgeGraph {
    let gd = GraphData {
        directed: false,
        multigraph: false,
        graph: Map::new(),
        nodes: vec![node(id1, l1, "a.py"), node(id2, l2, "b.py")],
        links: vec![edge(id1, id2, Confidence::Extracted)],
        hyperedges: vec![],
        built_at_commit: None,
    };
    KnowledgeGraph::from_graph_data(gd)
}

/// A code file importing a stylesheet asset node (`asset_kind: stylesheet`),
/// for the asset-rendering view tests.
pub(crate) fn kg_with_asset() -> KnowledgeGraph {
    let mut asset = node("src/theme.css", "src/theme.css", "src/theme.css");
    asset.file_type = FileType::Document;
    asset
        .extra
        .insert("asset_kind".into(), serde_json::json!("stylesheet"));
    let mut edge_css = edge("a", "src/theme.css", Confidence::Extracted);
    edge_css.relation = "imports_from".into();
    edge_css.context = Some("import".into());
    let gd = GraphData {
        directed: false,
        multigraph: false,
        graph: Map::new(),
        nodes: vec![node("a", "Button.tsx", "src/Button.tsx"), asset],
        links: vec![edge_css],
        hyperedges: vec![],
        built_at_commit: None,
    };
    KnowledgeGraph::from_graph_data(gd)
}

/// A federated 2-repo graph: `app::Main` (repo app) → `billing::Ledger` (repo
/// billing) via a cross-repo edge, plus an external-package stub `app::serde`.
pub(crate) fn kg_federated() -> KnowledgeGraph {
    let mut main = node("app::Main", "Main", "app/main.rs");
    main.repo = Some("app".into());
    let mut ledger = node("billing::Ledger", "Ledger", "billing/ledger.rs");
    ledger.repo = Some("billing".into());
    let mut serde_stub = node("app::serde", "serde", "");
    serde_stub.repo = Some("app".into());
    serde_stub
        .extra
        .insert("external_package".into(), serde_json::json!(true));

    let mut cross = edge("app::Main", "billing::Ledger", Confidence::Inferred);
    cross.cross_repo = true;
    let imp = edge("app::Main", "app::serde", Confidence::Extracted);

    let gd = GraphData {
        directed: false,
        multigraph: false,
        graph: Map::new(),
        nodes: vec![main, ledger, serde_stub],
        links: vec![cross, imp],
        hyperedges: vec![],
        built_at_commit: None,
    };
    let mut kg = KnowledgeGraph::from_graph_data(gd);
    let comms = cluster(&kg, &ClusterOptions::default());
    apply_communities(&mut kg, &comms);
    kg
}

/// A single-node graph with a caller-supplied label (for escaping tests).
pub(crate) fn kg_with_label(id: &str, label: &str) -> KnowledgeGraph {
    let gd = GraphData {
        directed: false,
        multigraph: false,
        graph: Map::new(),
        nodes: vec![node(id, label, "a.py")],
        links: vec![],
        hyperedges: vec![],
        built_at_commit: None,
    };
    KnowledgeGraph::from_graph_data(gd)
}
