use serde_json::Map;
use synaptic_core::{FileType, GraphData, Node, NodeId};
use synaptic_graph::KnowledgeGraph;
use synaptic_memory::{
    ingest_repository_documents, MemoryKind, MemoryQuery, MemoryRelation, MemoryStore,
};

fn graph() -> KnowledgeGraph {
    KnowledgeGraph::from_graph_data(GraphData {
        nodes: vec![Node {
            id: NodeId("refresh_token".into()),
            label: "refresh_token".into(),
            file_type: FileType::Code,
            source_file: "src/auth/token.rs".into(),
            source_location: Some("L10".into()),
            community: Some(0),
            repo: None,
            extra: Map::new(),
        }],
        ..GraphData::default()
    })
}

#[test]
fn ingests_adrs_and_procedures_with_sources_digests_and_symbol_anchors() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("docs/adr")).unwrap();
    std::fs::create_dir_all(repo.path().join("docs/procedures")).unwrap();
    std::fs::write(
        repo.path().join("docs/adr/ADR-014.md"),
        "# ADR-014 Retain refresh entrypoint\n\
         Status: Accepted\n\
         Synaptic-Symbols: refresh_token\n\n\
         Production loads `refresh_token` by name, so it must remain public.\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("docs/procedures/auth-release.md"),
        "# Authentication release procedure\n\
         Synaptic-Symbols: refresh_token\n\n\
         Run the auth tests before publishing a release.\n",
    )
    .unwrap();
    let store = MemoryStore::open(repo.path().join(".synaptic/memory"));

    let first = ingest_repository_documents(&store, repo.path(), Some(&graph())).unwrap();
    assert_eq!(first.created, 2, "{first:#?}");
    assert_eq!(first.already_present, 0);
    let records = store.all().unwrap();
    assert_eq!(records.len(), 2);
    let decision = records
        .iter()
        .find(|record| record.kind == MemoryKind::ArchitectureDecision)
        .unwrap();
    assert_eq!(decision.sources[0].uri, "file:docs/adr/ADR-014.md");
    assert!(decision.sources[0].digest.is_some());
    assert_eq!(decision.affected_symbols[0].node_id, "refresh_token");
    assert!(decision.summary.contains("Production loads"));
    assert!(records
        .iter()
        .any(|record| record.kind == MemoryKind::Procedure));

    let retry = ingest_repository_documents(&store, repo.path(), Some(&graph())).unwrap();
    assert_eq!(retry.created, 0);
    assert_eq!(retry.already_present, 2);
    assert_eq!(store.all().unwrap().len(), 2);
}

#[test]
fn a_changed_document_supersedes_the_previous_source_revision() {
    let repo = tempfile::tempdir().unwrap();
    let adr_dir = repo.path().join("docs/adr");
    std::fs::create_dir_all(&adr_dir).unwrap();
    let adr = adr_dir.join("ADR-014.md");
    std::fs::write(
        &adr,
        "# Keep refresh public\nSynaptic-Symbols: refresh_token\n\nOriginal rationale.\n",
    )
    .unwrap();
    let store = MemoryStore::open(repo.path().join(".synaptic/memory"));
    ingest_repository_documents(&store, repo.path(), Some(&graph())).unwrap();
    std::fs::write(
        &adr,
        "# Keep refresh public\nSynaptic-Symbols: refresh_token\n\nUpdated production rationale.\n",
    )
    .unwrap();
    ingest_repository_documents(&store, repo.path(), Some(&graph())).unwrap();

    let active = store
        .search(&MemoryQuery {
            kinds: vec![MemoryKind::ArchitectureDecision],
            symbol: Some("refresh_token".into()),
            limit: 10,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(active.len(), 1);
    assert!(active[0].record.summary.contains("Updated"));
    assert!(active[0]
        .record
        .links
        .iter()
        .any(|link| link.relation == MemoryRelation::Supersedes));

    let all = store
        .search(&MemoryQuery {
            kinds: vec![MemoryKind::ArchitectureDecision],
            symbol: Some("refresh_token".into()),
            include_superseded: true,
            limit: 10,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn unchanged_document_is_idempotent_across_anchor_enrichment() {
    let repo = tempfile::tempdir().unwrap();
    let adr_dir = repo.path().join("docs/adr");
    std::fs::create_dir_all(&adr_dir).unwrap();
    std::fs::write(
        adr_dir.join("ADR-014.md"),
        "# Keep refresh public\nSynaptic-Symbols: refresh_token\n\nStable rationale.\n",
    )
    .unwrap();
    let store = MemoryStore::open(repo.path().join(".synaptic/memory"));

    let first = ingest_repository_documents(&store, repo.path(), None).unwrap();
    assert_eq!(first.created, 1);

    let retry = ingest_repository_documents(&store, repo.path(), Some(&graph())).unwrap();
    assert_eq!(retry.created, 0);
    assert_eq!(retry.already_present, 1);
    assert_eq!(store.all().unwrap().len(), 1);
}
