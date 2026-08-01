use serde_json::Map;
use synaptic_core::{FileType, GraphData, Node, NodeId};
use synaptic_graph::KnowledgeGraph;
use synaptic_memory::{
    refresh_repository_memory, MemoryKind, MemoryQuery, MemoryRelation, MemoryStore,
};

fn node(id: &str, file: &str, community: u32) -> Node {
    Node {
        id: NodeId(id.into()),
        label: id.into(),
        file_type: FileType::Code,
        source_file: file.into(),
        source_location: Some("L1".into()),
        community: Some(community),
        repo: None,
        extra: Map::new(),
    }
}

#[test]
fn refresh_extracts_root_procedures_and_generates_community_summaries() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("CONTRIBUTING.md"),
        "# Contributing\n\nRun cargo fmt and cargo test before opening a pull request.\n",
    )
    .unwrap();
    let graph = KnowledgeGraph::from_graph_data(GraphData {
        nodes: vec![
            node("refresh_session", "src/auth.rs", 7),
            node("issue_token", "src/token.rs", 7),
            node("render_invoice", "src/billing.rs", 9),
        ],
        built_at_commit: Some("abc123".into()),
        ..GraphData::default()
    });
    let store = MemoryStore::open(repo.path().join(".synaptic/memory"));

    let report =
        refresh_repository_memory(&store, repo.path(), &graph, "file:synaptic-out/graph.json")
            .unwrap();
    assert_eq!(report.documents.scanned, 1);
    assert_eq!(report.documents.created, 1);
    assert_eq!(report.semantic.communities, 2);
    assert_eq!(report.semantic.created, 2);

    let records = store.all().unwrap();
    assert!(records
        .iter()
        .any(|record| record.kind == MemoryKind::Convention));
    let auth = store
        .search(&MemoryQuery {
            text: "refresh_session issue_token".into(),
            kinds: vec![MemoryKind::SemanticSummary],
            limit: 10,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(auth.len(), 1, "{auth:#?}");
    assert_eq!(auth[0].record.commit.as_deref(), Some("abc123"));
    assert_eq!(auth[0].record.affected_symbols.len(), 2);
    assert_eq!(
        auth[0].record.sources[0].uri,
        "file:synaptic-out/graph.json"
    );
    assert!(auth[0].record.sources[0].digest.is_some());

    let retry =
        refresh_repository_memory(&store, repo.path(), &graph, "file:synaptic-out/graph.json")
            .unwrap();
    assert_eq!(retry.documents.already_present, 1);
    assert_eq!(retry.semantic.already_present, 2);
}

#[test]
fn changed_community_summary_supersedes_the_previous_graph_revision() {
    let repo = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(repo.path().join("memory"));
    let graph_v1 = KnowledgeGraph::from_graph_data(GraphData {
        nodes: vec![node("refresh_session", "src/auth.rs", 7)],
        built_at_commit: Some("abc123".into()),
        ..GraphData::default()
    });
    refresh_repository_memory(&store, repo.path(), &graph_v1, "graph:repo").unwrap();
    let graph_v2 = KnowledgeGraph::from_graph_data(GraphData {
        nodes: vec![
            node("refresh_session", "src/auth.rs", 7),
            node("rotate_session", "src/auth.rs", 7),
        ],
        built_at_commit: Some("def456".into()),
        ..GraphData::default()
    });
    refresh_repository_memory(&store, repo.path(), &graph_v2, "graph:repo").unwrap();

    let active = store
        .search(&MemoryQuery {
            text: "rotate_session".into(),
            kinds: vec![MemoryKind::SemanticSummary],
            limit: 10,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(active.len(), 1);
    assert!(active[0]
        .record
        .links
        .iter()
        .any(|link| link.relation == MemoryRelation::Supersedes));
    assert_eq!(
        store
            .search(&MemoryQuery {
                kinds: vec![MemoryKind::SemanticSummary],
                include_superseded: true,
                limit: 10,
                ..MemoryQuery::default()
            })
            .unwrap()
            .len(),
        2
    );
}
