use synaptic_memory::{
    AccessScope, MemoryKind, MemoryPrincipal, MemoryQuery, MemoryRecord, MemoryStore,
    SourceArtifact, export_bundle, import_bundle,
};

fn record(key: &str, repository: &str, title: &str) -> MemoryRecord {
    let mut record = MemoryRecord::new(
        key,
        MemoryKind::ChangeEpisode,
        title,
        format!("Source-grounded history for {title}."),
        repository,
        100,
        vec![SourceArtifact {
            kind: "git_commit".into(),
            uri: format!("git:{key}"),
            revision: Some(key.into()),
            digest: Some(key.into()),
        }],
    );
    record.access_scope = AccessScope::Repository;
    record
}

#[test]
fn compacted_snapshot_is_verified_reused_and_invalidated_by_external_writes() {
    let dir = tempfile::tempdir().unwrap();
    let writer = MemoryStore::open(dir.path());
    writer
        .record(&record("one", "repo/a", "Initial authentication change"))
        .unwrap();
    writer
        .record(&record("two", "repo/a", "Follow-up authentication change"))
        .unwrap();
    let compacted = writer.compact().unwrap();
    assert_eq!(compacted.records, 2);
    assert!(compacted.bytes > 0);
    assert!(dir.path().join("index.compact-v1.json").is_file());

    let reader = MemoryStore::open(dir.path());
    let first = reader
        .search_with_diagnostics(&MemoryQuery {
            text: "authentication".into(),
            limit: 10,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert!(first.diagnostics.loaded_from_compaction);
    assert_eq!(first.hits.len(), 2);

    writer
        .record(&record("three", "repo/a", "External authentication change"))
        .unwrap();
    let refreshed = reader
        .search_with_diagnostics(&MemoryQuery {
            text: "authentication".into(),
            limit: 10,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert!(!refreshed.diagnostics.loaded_from_compaction);
    assert_eq!(refreshed.hits.len(), 3);
}

#[test]
fn federated_store_queries_multiple_repositories_and_deduplicates_replicas() {
    let root_a = tempfile::tempdir().unwrap();
    let root_b = tempfile::tempdir().unwrap();
    let a = MemoryStore::open(root_a.path());
    let b = MemoryStore::open(root_b.path());
    let shared = record("shared", "repo/a", "Shared replicated change");
    a.record(&shared).unwrap();
    b.record(&shared).unwrap();
    b.record(&record("remote", "repo/b", "Remote billing change"))
        .unwrap();

    let federation = MemoryStore::open_federated(root_a.path(), [root_b.path()]);
    let hits = federation
        .search(&MemoryQuery {
            text: "change".into(),
            limit: 10,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 2, "{hits:#?}");
    assert_eq!(federation.roots().len(), 2);
}

#[test]
fn checksummed_team_bundle_round_trips_only_authorized_records() {
    let source_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let source = MemoryStore::open(source_dir.path());
    let mut private = record("private", "repo/a", "Private investigation");
    private.access_scope = AccessScope::Private;
    private.owner = Some("alice".into());
    source.record(&private).unwrap();
    source
        .record(&record("shared", "repo/a", "Shared repository change"))
        .unwrap();
    let principal = MemoryPrincipal::restricted("bob").with_repository("repo/a");
    let bundle = source_dir.path().join("team-memory.json");

    let exported = export_bundle(&source, &bundle, &principal).unwrap();
    assert_eq!(exported.records, 1);
    let target = MemoryStore::open(target_dir.path());
    let imported = import_bundle(&target, &bundle, &principal).unwrap();
    assert_eq!(imported.created, 1);
    assert_eq!(target.all().unwrap()[0].idempotency_key, "shared");

    let mut tampered: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&bundle).unwrap()).unwrap();
    tampered["records"][0]["summary"] = "tampered".into();
    std::fs::write(&bundle, serde_json::to_vec(&tampered).unwrap()).unwrap();
    assert!(import_bundle(&target, &bundle, &principal).is_err());
}
