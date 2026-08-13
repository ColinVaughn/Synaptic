use serde_json::{Map, json};
use synaptic_core::{FileType, GraphData, Node, NodeId};
use synaptic_graph::KnowledgeGraph;
use synaptic_memory::{MemoryKind, MemoryQuery, MemoryRelation, MemoryStore, ingest_artifact_file};

fn graph() -> KnowledgeGraph {
    KnowledgeGraph::from_graph_data(GraphData {
        nodes: vec![Node {
            id: NodeId("refresh_session".into()),
            label: "refresh_session".into(),
            file_type: FileType::Code,
            source_file: "src/auth.rs".into(),
            source_location: Some("L1".into()),
            community: Some(0),
            repo: None,
            extra: Map::new(),
            ..Default::default()
        }],
        built_at_commit: Some("abc123".into()),
        ..GraphData::default()
    })
}

#[test]
fn ingests_every_external_artifact_kind_with_grounding_and_retry() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("artifacts.json");
    let kinds = [
        "issue",
        "pull_request",
        "review_finding",
        "ci_run",
        "incident",
        "release",
        "agent_task",
    ];
    let artifacts = kinds
        .iter()
        .enumerate()
        .map(|(index, kind)| {
            json!({
                "kind": kind,
                "external_id": format!("external-{index}"),
                "title": format!("{kind} authentication observation"),
                "summary": format!("Source-grounded {kind} evidence for refresh."),
                "source_uri": format!("https://tracker.example/{kind}/{index}"),
                "repository": "example/repo",
                "occurred_at": 100 + index as i64,
                "status": if *kind == "ci_run" { "failed" } else { "resolved" },
                "commit": "abc123",
                "affected_symbols": ["refresh_session"],
                "affected_files": ["src/auth.rs"],
                "verification_status": if *kind == "ci_run" { "failed" } else { "passed" },
                "verification_commands": ["cargo test auth"],
                "scope": "repository"
            })
        })
        .collect::<Vec<_>>();
    std::fs::write(
        &input,
        serde_json::to_vec_pretty(&json!({
            "schema": "synaptic.memory-artifacts/v1",
            "artifacts": artifacts
        }))
        .unwrap(),
    )
    .unwrap();
    let store = MemoryStore::open(dir.path().join("memory"));

    let report = ingest_artifact_file(&store, &input, Some(&graph())).unwrap();
    assert_eq!(report.scanned, 7);
    assert_eq!(report.created, 7);
    assert_eq!(report.already_present, 0);
    let records = store.all().unwrap();
    assert_eq!(records.len(), 7);
    for expected in [
        MemoryKind::Issue,
        MemoryKind::PullRequest,
        MemoryKind::ReviewFinding,
        MemoryKind::CiRun,
        MemoryKind::Incident,
        MemoryKind::Release,
        MemoryKind::AgentTask,
    ] {
        assert!(records.iter().any(|record| record.kind == expected));
    }
    assert!(records.iter().all(|record| {
        record.sources[0].digest.is_some()
            && record.affected_symbols[0].node_id == "refresh_session"
            && record.path_changes[0].new_path.as_deref() == Some("src/auth.rs")
    }));

    let retry = ingest_artifact_file(&store, &input, Some(&graph())).unwrap();
    assert_eq!(retry.created, 0);
    assert_eq!(retry.already_present, 7);
}

#[test]
fn changed_external_artifact_supersedes_the_prior_source_revision() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("issue.json");
    let store = MemoryStore::open(dir.path().join("memory"));
    let write = |summary: &str| {
        std::fs::write(
            &input,
            serde_json::to_vec(&json!({
                "schema": "synaptic.memory-artifacts/v1",
                "artifacts": [{
                    "kind": "issue",
                    "external_id": "ISSUE-42",
                    "title": "Refresh race",
                    "summary": summary,
                    "source_uri": "https://tracker.example/issues/42",
                    "repository": "example/repo",
                    "occurred_at": 100,
                    "affected_symbols": ["refresh_session"]
                }]
            }))
            .unwrap(),
        )
        .unwrap();
    };
    write("The first investigation suspected the session cache.");
    ingest_artifact_file(&store, &input, Some(&graph())).unwrap();
    write("The accepted investigation identified a mutex ordering bug.");
    ingest_artifact_file(&store, &input, Some(&graph())).unwrap();

    let all = store.all().unwrap();
    assert_eq!(all.len(), 2);
    let current = store
        .search(&MemoryQuery {
            text: "mutex ordering".into(),
            limit: 10,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(current.len(), 1);
    assert!(
        current[0]
            .record
            .links
            .iter()
            .any(|link| link.relation == MemoryRelation::Supersedes)
    );
    let active_by_symbol = store
        .search(&MemoryQuery {
            symbol: Some("refresh_session".into()),
            limit: 10,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(active_by_symbol.len(), 1);
}
