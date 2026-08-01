use std::process::Command;

use serde_json::Map;
use synaptic_core::{FileType, GraphData, Node, NodeId};
use synaptic_graph::KnowledgeGraph;
use synaptic_memory::{
    ingest_commit, MemoryKind, MemoryQuery, MemoryStore, PathChangeKind, RecordOutcome,
    SymbolChangeKind,
};

fn git(root: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn rename_ingest_retains_old_and_new_paths_for_history_lookup() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init"]);
    git(repo.path(), &["config", "user.email", "test@example.com"]);
    git(repo.path(), &["config", "user.name", "Synaptic Test"]);
    std::fs::write(repo.path().join("auth.rs"), "fn refresh_token() {}\n").unwrap();
    git(repo.path(), &["add", "auth.rs"]);
    git(repo.path(), &["commit", "-m", "Add refresh token"]);
    git(repo.path(), &["mv", "auth.rs", "token.rs"]);
    git(repo.path(), &["commit", "-m", "Move refresh token module"]);
    let sha = git(repo.path(), &["rev-parse", "HEAD"]);

    let node = Node {
        id: NodeId("refresh_token".into()),
        label: "refresh_token".into(),
        file_type: FileType::Code,
        source_file: "token.rs".into(),
        source_location: Some("L1".into()),
        community: Some(0),
        repo: None,
        extra: Map::new(),
    };
    let graph = KnowledgeGraph::from_graph_data(GraphData {
        directed: true,
        nodes: vec![node],
        built_at_commit: Some(sha.clone()),
        ..GraphData::default()
    });
    let store = MemoryStore::open(repo.path().join(".synaptic/memory"));

    let (record, _) = ingest_commit(&store, repo.path(), &sha, Some(&graph)).unwrap();
    assert_eq!(record.path_changes.len(), 1, "{record:#?}");
    assert_eq!(record.path_changes[0].kind, PathChangeKind::Renamed);
    assert_eq!(record.path_changes[0].old_path.as_deref(), Some("auth.rs"));
    assert_eq!(record.path_changes[0].new_path.as_deref(), Some("token.rs"));
    assert_eq!(record.affected_symbols[0].source_file, "token.rs");

    let old_path_hits = store
        .search(&MemoryQuery {
            symbol: Some("auth.rs".into()),
            limit: 10,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(old_path_hits.len(), 1);
    assert_eq!(
        old_path_hits[0].record.commit.as_deref(),
        Some(sha.as_str())
    );
}

#[test]
fn symbol_rename_is_revision_aware_and_searchable_by_both_names() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init"]);
    git(repo.path(), &["config", "user.email", "test@example.com"]);
    git(repo.path(), &["config", "user.name", "Synaptic Test"]);
    std::fs::write(
        repo.path().join("auth.rs"),
        "pub fn refresh_token() -> bool { true }\n",
    )
    .unwrap();
    git(repo.path(), &["add", "auth.rs"]);
    git(repo.path(), &["commit", "-m", "Add token refresh"]);
    std::fs::write(
        repo.path().join("auth.rs"),
        "pub fn refresh_session() -> bool { true }\n",
    )
    .unwrap();
    git(repo.path(), &["add", "auth.rs"]);
    git(repo.path(), &["commit", "-m", "Rename refresh entrypoint"]);
    let sha = git(repo.path(), &["rev-parse", "HEAD"]);
    let parent = git(repo.path(), &["rev-parse", "HEAD^"]);

    let graph = KnowledgeGraph::from_graph_data(GraphData {
        nodes: vec![Node {
            id: NodeId("refresh_session".into()),
            label: "refresh_session".into(),
            file_type: FileType::Code,
            source_file: "auth.rs".into(),
            source_location: Some("L1".into()),
            community: Some(0),
            repo: None,
            extra: Map::new(),
        }],
        built_at_commit: Some(sha.clone()),
        ..GraphData::default()
    });
    let store = MemoryStore::open(repo.path().join(".synaptic/memory"));

    let (record, _) = ingest_commit(&store, repo.path(), &sha, Some(&graph)).unwrap();
    assert_eq!(record.symbol_changes.len(), 1, "{record:#?}");
    let rename = &record.symbol_changes[0];
    assert_eq!(rename.kind, SymbolChangeKind::Renamed);
    assert_eq!(rename.old.label, "refresh_token");
    assert_eq!(rename.old.commit.as_deref(), Some(parent.as_str()));
    assert_eq!(rename.new.label, "refresh_session");
    assert_eq!(rename.new.commit.as_deref(), Some(sha.as_str()));
    assert!(rename.confidence >= 0.8);

    for subject in ["refresh_token", "refresh_session"] {
        let hits = store
            .search(&MemoryQuery {
                symbol: Some(subject.into()),
                limit: 10,
                ..MemoryQuery::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1, "{subject}: {hits:#?}");
        assert_eq!(hits[0].record.commit.as_deref(), Some(sha.as_str()));
    }
}

#[test]
fn reingesting_a_commit_does_not_conflict_when_new_enrichment_is_available() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init"]);
    git(repo.path(), &["config", "user.email", "test@example.com"]);
    git(repo.path(), &["config", "user.name", "Synaptic Test"]);
    std::fs::write(repo.path().join("auth.rs"), "fn refresh_token() {}\n").unwrap();
    git(repo.path(), &["add", "auth.rs"]);
    git(repo.path(), &["commit", "-m", "Add refresh token"]);
    let sha = git(repo.path(), &["rev-parse", "HEAD"]);
    let store = MemoryStore::open(repo.path().join(".synaptic/memory"));
    let (_, created) = ingest_commit(&store, repo.path(), &sha, None).unwrap();
    assert_eq!(created, RecordOutcome::Created);

    let graph = KnowledgeGraph::from_graph_data(GraphData {
        nodes: vec![Node {
            id: NodeId("refresh_token".into()),
            label: "refresh_token".into(),
            file_type: FileType::Code,
            source_file: "auth.rs".into(),
            source_location: Some("L1".into()),
            community: Some(0),
            repo: None,
            extra: Map::new(),
        }],
        ..GraphData::default()
    });
    let (existing, outcome) = ingest_commit(&store, repo.path(), &sha, Some(&graph)).unwrap();
    assert_eq!(outcome, RecordOutcome::AlreadyPresent);
    assert!(existing.affected_symbols.is_empty());
    assert_eq!(store.all().unwrap().len(), 1);
}

#[test]
fn ingests_a_commit_and_anchors_changed_graph_symbols_idempotently() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init"]);
    git(repo.path(), &["config", "user.email", "test@example.com"]);
    git(repo.path(), &["config", "user.name", "Synaptic Test"]);
    std::fs::write(
        repo.path().join("auth.rs"),
        "fn refresh_token() { /* serialize */ }\n",
    )
    .unwrap();
    git(repo.path(), &["add", "auth.rs"]);
    git(
        repo.path(),
        &["commit", "-m", "Fix authentication refresh race"],
    );
    let sha = git(repo.path(), &["rev-parse", "HEAD"]);

    let node = Node {
        id: NodeId("refresh_token".into()),
        label: "refresh_token".into(),
        file_type: FileType::Code,
        source_file: "auth.rs".into(),
        source_location: Some("L1".into()),
        community: Some(0),
        repo: None,
        extra: Map::new(),
    };
    let graph = KnowledgeGraph::from_graph_data(GraphData {
        directed: true,
        nodes: vec![node],
        built_at_commit: Some(sha.clone()),
        ..GraphData::default()
    });
    let store = MemoryStore::open(repo.path().join(".synaptic/memory"));

    let (record, outcome) = ingest_commit(&store, repo.path(), &sha, Some(&graph)).unwrap();
    assert_eq!(outcome, RecordOutcome::Created);
    assert_eq!(record.kind, MemoryKind::ChangeEpisode);
    assert_eq!(record.commit.as_deref(), Some(sha.as_str()));
    assert!(record.title.contains("authentication refresh race"));
    assert_eq!(record.affected_symbols.len(), 1);
    assert_eq!(record.affected_symbols[0].node_id, "refresh_token");

    let (_, again) = ingest_commit(&store, repo.path(), &sha, Some(&graph)).unwrap();
    assert_eq!(again, RecordOutcome::AlreadyPresent);
}
