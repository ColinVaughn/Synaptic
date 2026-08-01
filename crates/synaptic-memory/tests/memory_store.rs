use synaptic_memory::{
    AccessScope, MemoryKind, MemoryLifecycle, MemoryLink, MemoryPrincipal, MemoryQuery,
    MemoryRecord, MemoryRelation, MemoryStore, RecordOutcome, SourceArtifact, SymbolAnchor,
};

fn source(uri: &str) -> SourceArtifact {
    SourceArtifact {
        kind: "git".into(),
        uri: uri.into(),
        revision: Some("abc123".into()),
        digest: None,
    }
}

fn episode(key: &str, title: &str, summary: &str) -> MemoryRecord {
    let mut record = MemoryRecord::new(
        key,
        MemoryKind::ChangeEpisode,
        title,
        summary,
        "acme/auth",
        100,
        vec![source("git:abc123")],
    );
    record.access_scope = AccessScope::Repository;
    record.affected_symbols.push(SymbolAnchor {
        node_id: "auth_refresh".into(),
        label: "refresh_token".into(),
        source_file: "src/auth/token.rs".into(),
        repo: None,
        commit: Some("abc123".into()),
        confidence: 1.0,
    });
    record
}

#[test]
fn records_and_loads_source_grounded_memory_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path());
    let record = episode(
        "commit:abc123",
        "Fix token refresh race",
        "Serialized refreshes so a stale token cannot overwrite a new token.",
    );

    assert_eq!(store.record(&record).unwrap(), RecordOutcome::Created);
    assert_eq!(
        store.record(&record).unwrap(),
        RecordOutcome::AlreadyPresent
    );
    assert_eq!(store.all().unwrap(), vec![record]);
}

#[test]
fn rejects_source_less_or_conflicting_records() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path());
    let mut missing = episode("agent:1", "Attempt", "No evidence");
    missing.sources.clear();
    assert!(store.record(&missing).is_err());

    let first = episode("agent:2", "Attempt A", "One result");
    let mut conflicting = first.clone();
    conflicting.summary = "Different result".into();
    store.record(&first).unwrap();
    assert!(store.record(&conflicting).is_err());
}

#[test]
fn search_combines_text_and_symbol_grounding() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path());
    store
        .record(&episode(
            "a",
            "Fix authentication token refresh",
            "The refresh_token lock fixed a production race.",
        ))
        .unwrap();
    store
        .record(&episode(
            "b",
            "Improve billing export",
            "Reduced allocations in the invoice writer.",
        ))
        .unwrap();

    let hits = store
        .search(&MemoryQuery {
            text: "authentication refresh race".into(),
            symbol: Some("refresh_token".into()),
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].record.idempotency_key, "a");
    assert!(hits[0].matched_by.contains(&"symbol".to_string()));
    assert!(hits[0].score > 1.0);
}

#[test]
fn superseded_records_are_hidden_unless_requested() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path());
    let old = episode("old", "Use legacy refresh flow", "Original decision.");
    let mut replacement = episode(
        "new",
        "Use serialized refresh flow",
        "Replacement decision.",
    );
    replacement.kind = MemoryKind::ArchitectureDecision;
    replacement.links.push(MemoryLink {
        relation: MemoryRelation::Supersedes,
        target: old.id.clone(),
    });
    store.record(&old).unwrap();
    store.record(&replacement).unwrap();

    let active = store
        .search(&MemoryQuery {
            text: "refresh flow".into(),
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].record.id, replacement.id);

    let all = store
        .search(&MemoryQuery {
            text: "refresh flow".into(),
            include_superseded: true,
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn lifecycle_superseded_is_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path());
    let mut old = episode("old-state", "Old invariant", "No longer applies.");
    old.lifecycle = MemoryLifecycle::Superseded;
    store.record(&old).unwrap();
    assert!(store
        .search(&MemoryQuery {
            text: "invariant".into(),
            limit: 10,
            ..Default::default()
        })
        .unwrap()
        .is_empty());
}

#[test]
fn indexed_search_narrows_candidates_without_changing_results() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path());
    for index in 0..100 {
        store
            .record(&episode(
                &format!("bulk-{index}"),
                &format!("Routine billing maintenance {index}"),
                "Updated invoice formatting.",
            ))
            .unwrap();
    }
    store
        .record(&episode(
            "unique",
            "Needle unique authentication regression",
            "The refresh path regressed.",
        ))
        .unwrap();

    let result = store
        .search_with_diagnostics(&MemoryQuery {
            text: "needle_unique".into(),
            limit: 10,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].record.idempotency_key, "unique");
    assert_eq!(result.diagnostics.total_records, 101);
    assert!(
        result.diagnostics.candidate_records <= 2,
        "{:#?}",
        result.diagnostics
    );
}

#[test]
fn a_cached_reader_observes_records_written_by_another_store_instance() {
    let dir = tempfile::tempdir().unwrap();
    let reader = MemoryStore::open(dir.path());
    let writer = MemoryStore::open(dir.path());
    writer
        .record(&episode(
            "first",
            "Initial auth memory",
            "First observation.",
        ))
        .unwrap();
    assert_eq!(
        reader
            .search(&MemoryQuery {
                text: "initial auth".into(),
                limit: 10,
                ..MemoryQuery::default()
            })
            .unwrap()
            .len(),
        1
    );

    writer
        .record(&episode(
            "second",
            "External cache invalidation marker",
            "Second observation.",
        ))
        .unwrap();
    let hits = reader
        .search(&MemoryQuery {
            text: "invalidation marker".into(),
            limit: 10,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].record.idempotency_key, "second");
}

#[test]
fn principal_authorization_enforces_private_repository_and_workspace_scopes() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path());
    let mut private = episode("private-alice", "Alice private finding", "Private detail.");
    private.owner = Some("alice".into());
    private.access_scope = AccessScope::Private;
    let mut repository = episode("repo-shared", "Repository convention", "Repo detail.");
    repository.repository = "example/repo".into();
    repository.access_scope = AccessScope::Repository;
    let mut workspace = episode("workspace-shared", "Workspace runbook", "Workspace detail.");
    workspace.access_scope = AccessScope::Workspace {
        workspace: "platform".into(),
    };
    store.record(&private).unwrap();
    store.record(&repository).unwrap();
    store.record(&workspace).unwrap();

    let alice = MemoryPrincipal::restricted("alice")
        .with_repository("example/repo")
        .with_workspace("platform");
    let bob = MemoryPrincipal::restricted("bob").with_repository("example/repo");
    let outsider = MemoryPrincipal::restricted("outsider");
    let query = MemoryQuery {
        limit: 10,
        ..MemoryQuery::default()
    };
    let alice_result = store
        .search_with_diagnostics_authorized(&query, &alice)
        .unwrap();
    assert_eq!(alice_result.hits.len(), 3);
    assert_eq!(alice_result.diagnostics.total_records, 3);
    let bob_result = store
        .search_with_diagnostics_authorized(&query, &bob)
        .unwrap();
    assert_eq!(bob_result.hits.len(), 1);
    assert_eq!(bob_result.hits[0].record.idempotency_key, "repo-shared");
    assert_eq!(bob_result.diagnostics.total_records, 1);
    let outsider_result = store
        .search_with_diagnostics_authorized(&query, &outsider)
        .unwrap();
    assert!(outsider_result.hits.is_empty());
    assert_eq!(outsider_result.diagnostics.total_records, 0);

    let mut bob_private = episode("bob-private", "Bob private", "Must not be spoofed.");
    bob_private.owner = Some("bob".into());
    assert!(store.record_as(&bob_private, &alice).is_err());
}

#[test]
fn schema_version_upgrade_preserves_idempotent_retries() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path());
    let record = episode("schema-upgrade", "Schema upgrade", "Same grounded content.");
    store.record(&record).unwrap();
    let path = dir
        .path()
        .join("records")
        .join(format!("{}.json", record.id));
    let mut old: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    old["version"] = 1.into();
    std::fs::write(&path, serde_json::to_vec_pretty(&old).unwrap()).unwrap();

    assert_eq!(
        store.record(&record).unwrap(),
        RecordOutcome::AlreadyPresent
    );
}
