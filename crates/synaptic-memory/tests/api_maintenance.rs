use synaptic_memory::{
    ApiMaintenanceMemory, MemoryKind, MemoryStore, RecordOutcome, VerificationStatus,
    record_api_maintenance_memory,
};

#[test]
fn api_event_run_failure_and_pr_are_source_grounded_and_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(root.path());
    let base = ApiMaintenanceMemory {
        repository: "acme/widgets".into(),
        vendor: "acme".into(),
        event_id: "event_1".into(),
        run_id: Some("run_1".into()),
        occurred_at: 42,
        source_uri: "https://acme.example/openapi".into(),
        source_revision: "v2".into(),
        source_digest: "digest".into(),
        base_sha: Some("abc".into()),
        branch: Some("synaptic/api/acme/event_1".into()),
        pull_request_url: Some("https://github.example/pr/1".into()),
        summary: "migrated widgets API".into(),
        commands: vec!["npm test".into()],
        verification: VerificationStatus::Passed,
    };
    assert_eq!(
        record_api_maintenance_memory(&store, MemoryKind::Release, &base).unwrap(),
        RecordOutcome::Created
    );
    assert_eq!(
        record_api_maintenance_memory(&store, MemoryKind::Release, &base).unwrap(),
        RecordOutcome::AlreadyPresent
    );
    record_api_maintenance_memory(&store, MemoryKind::AgentTask, &base).unwrap();
    record_api_maintenance_memory(
        &store,
        MemoryKind::FailedAttempt,
        &ApiMaintenanceMemory {
            verification: VerificationStatus::Failed,
            ..base.clone()
        },
    )
    .unwrap();
    record_api_maintenance_memory(&store, MemoryKind::PullRequest, &base).unwrap();
    let kinds = store
        .all()
        .unwrap()
        .into_iter()
        .map(|record| record.kind)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(kinds.len(), 4);
}
