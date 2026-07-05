//! The regression keystone: a single-repo graph survives migrate -> export
//! byte-identically, and a multi-repo graph splits into per-repo shards with
//! cross-repo edges sent to the bridge.

use proptest::prelude::*;
use serde_json::Map;
use synaptic_core::node_kind::NodeKind;
use synaptic_core::{Confidence, Edge, FileType, GraphData, Hyperedge, Node, NodeId};
use synaptic_graph::KnowledgeGraph;
use synaptic_output::to_json_value;
use synaptic_store::{migrate, Scope, ShardStore};

fn code_node(id: &str, label: &str, sf: &str, repo: Option<&str>) -> Node {
    Node {
        id: NodeId(id.into()),
        label: label.into(),
        file_type: FileType::Code,
        source_file: sf.into(),
        source_location: Some("10:0-40:1".into()),
        community: Some(2),
        repo: repo.map(|r| r.into()),
        extra: Map::new(),
    }
}

fn edge(s: &str, t: &str) -> Edge {
    Edge {
        source: NodeId(s.into()),
        target: NodeId(t.into()),
        relation: "calls".into(),
        confidence: Confidence::Extracted,
        source_file: "src/ledger.rs".into(),
        source_location: Some("12:4".into()),
        confidence_score: None,
        weight: 1.0,
        context: None,
        cross_repo: false,
        extra: Map::new(),
    }
}

/// A representative single-repo graph with enrichment (kind, span, community),
/// a hyperedge, and provenance — the shapes that must survive a round-trip.
fn sample_kg() -> KnowledgeGraph {
    let mut a = code_node("billing_ledger", "Ledger", "src/ledger.rs", None);
    a.set_kind(NodeKind::Class);
    let b = code_node("post", "post", "src/ledger.rs", None);
    let gd = GraphData {
        directed: true,
        multigraph: false,
        graph: Map::new(),
        nodes: vec![a, b],
        links: vec![edge("billing_ledger", "post")],
        hyperedges: vec![Hyperedge {
            id: "h1".into(),
            label: "cluster".into(),
            nodes: vec![NodeId("billing_ledger".into()), NodeId("post".into())],
            relation: None,
            confidence: None,
        }],
        built_at_commit: Some("abc123".into()),
    };
    KnowledgeGraph::from_graph_data(gd)
}

#[test]
fn single_repo_migrate_export_is_byte_identical() {
    // Original graph.json exactly as `synaptic export json` would write it.
    let kg = sample_kg();
    let original_value = to_json_value(&kg);
    let original_str = serde_json::to_string_pretty(&original_value).unwrap();

    // migrate: parse the graph.json -> store (single repo -> one `local` shard)
    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join("store");
    let gd: GraphData = serde_json::from_value(original_value.clone()).unwrap();
    let mut store = ShardStore::open(&store_dir).unwrap();
    migrate::migrate_into(&mut store, &gd).unwrap();
    assert_eq!(store.list_shards().len(), 1);
    assert_eq!(store.list_shards()[0].tag, "local");

    // export: store -> graph.json
    let exported_kg = store.export_graph(&Scope::All).unwrap();
    let exported_value = to_json_value(&exported_kg);
    let exported_str = serde_json::to_string_pretty(&exported_value).unwrap();

    assert_eq!(
        exported_value, original_value,
        "exported JSON value must match"
    );
    assert_eq!(
        exported_str, original_str,
        "exported graph.json must be byte-identical"
    );
}

#[test]
fn multi_repo_splits_into_shards_with_bridge() {
    let gd = GraphData {
        directed: true,
        multigraph: false,
        graph: Map::new(),
        nodes: vec![
            code_node("a", "A", "billing/a.rs", Some("billing")),
            code_node("b", "B", "billing/b.rs", Some("billing")),
            code_node("c", "C", "web/c.rs", Some("web")),
        ],
        links: vec![
            edge("a", "b"), // intra billing
            edge("b", "c"), // cross billing -> web
        ],
        hyperedges: vec![],
        built_at_commit: None,
    };

    let split = migrate::split(&gd);
    assert_eq!(split.shards.len(), 2);
    let billing = &split.shards.iter().find(|(t, _)| t == "billing").unwrap().1;
    let web = &split.shards.iter().find(|(t, _)| t == "web").unwrap().1;
    assert_eq!(billing.nodes.len(), 2);
    assert_eq!(billing.links.len(), 1, "intra-repo edge stays in its shard");
    assert_eq!(web.nodes.len(), 1);
    assert_eq!(web.links.len(), 0);
    assert_eq!(split.bridge.len(), 1, "cross-repo edge goes to the bridge");
    assert_eq!(split.bridge[0].source.0, "b");
    assert_eq!(split.bridge[0].target.0, "c");
}

#[test]
fn cross_repo_bridge_is_opt_in() {
    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join("store");
    let gd = GraphData {
        directed: true,
        multigraph: false,
        graph: Map::new(),
        nodes: vec![
            code_node("a", "A", "billing/a.rs", Some("billing")),
            code_node("b", "B", "billing/b.rs", Some("billing")),
            code_node("c", "C", "web/c.rs", Some("web")),
        ],
        links: vec![edge("a", "b"), edge("b", "c")], // a->b intra billing, b->c cross
        hyperedges: vec![],
        built_at_commit: None,
    };

    let mut store = ShardStore::open(&store_dir).unwrap();
    migrate::migrate_into(&mut store, &gd).unwrap();
    let store = ShardStore::open(&store_dir).unwrap();

    // Isolation default: the cross-repo edge is not present.
    let iso = store.export_graph(&Scope::All).unwrap();
    assert_eq!(iso.edge_count(), 1, "only the intra-repo edge survives");
    assert!(iso
        .edges()
        .all(|e| !(e.source.0 == "b" && e.target.0 == "c")));

    // Cross-repo opt-in: the bridge edge is grafted back.
    let cross = store.export_cross_repo().unwrap();
    assert_eq!(cross.edge_count(), 2, "bridge edge included");
    assert!(cross
        .edges()
        .any(|e| e.source.0 == "b" && e.target.0 == "c"));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// A random single-repo graph survives migrate -> export with the same
    /// node and edge sets (single-shard order is preserved, so structurally equal).
    #[test]
    fn random_single_repo_graph_round_trips(
        n in 1usize..16,
        raw_edges in proptest::collection::vec((0usize..16, 0usize..16), 0..30),
    ) {
        let nodes: Vec<Node> = (0..n)
            .map(|i| code_node(&format!("n{i}"), &format!("N{i}"), &format!("src/n{i}.rs"), None))
            .collect();
        let links: Vec<Edge> = raw_edges
            .iter()
            .map(|(s, t)| edge(&format!("n{}", s % n), &format!("n{}", t % n)))
            .collect();
        let original = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes,
            links,
            hyperedges: vec![],
            built_at_commit: None,
        };

        let dir = tempfile::tempdir().unwrap();
        let mut store = ShardStore::open(&dir.path().join("store")).unwrap();
        migrate::migrate_into(&mut store, &original).unwrap();
        let back = store.export_graph(&Scope::All).unwrap();

        prop_assert_eq!(back.node_count(), original.nodes.len());
        prop_assert_eq!(back.edge_count(), original.links.len());

        let mut got: Vec<(String, String, String)> = back
            .edges()
            .map(|e| (e.source.0.clone(), e.target.0.clone(), e.relation.clone()))
            .collect();
        let mut want: Vec<(String, String, String)> = original
            .links
            .iter()
            .map(|e| (e.source.0.clone(), e.target.0.clone(), e.relation.clone()))
            .collect();
        got.sort();
        want.sort();
        prop_assert_eq!(got, want);
    }
}
