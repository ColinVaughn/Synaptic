use serde_json::json;
use synaptic_memory::{
    AccessScope, BenchmarkGate, MemoryKind, MemoryPrincipal, MemoryRecord, MemoryStore,
    SourceArtifact, enforce_benchmark_gate, run_benchmark_file,
};

fn record(key: &str, title: &str, summary: &str, source: &str) -> MemoryRecord {
    let mut record = MemoryRecord::new(
        key,
        MemoryKind::ChangeEpisode,
        title,
        summary,
        "repo",
        100,
        vec![SourceArtifact {
            kind: "test".into(),
            uri: source.into(),
            revision: None,
            digest: None,
        }],
    );
    record.access_scope = AccessScope::Repository;
    record
}

#[test]
fn benchmark_reports_localization_quality_selectivity_and_gate_failures() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path().join("memory"));
    for index in 0..50 {
        store
            .record(&record(
                &format!("noise-{index}"),
                &format!("Routine billing cleanup {index}"),
                "Updated invoice formatting.",
                &format!("git:noise-{index}"),
            ))
            .unwrap();
    }
    store
        .record(&record(
            "auth",
            "Authentication mutex regression",
            "Holding the session mutex during refresh caused a deadlock.",
            "incident:AUTH-42",
        ))
        .unwrap();
    store
        .record(&record(
            "release",
            "Release rollback procedure",
            "Rollback requires draining the deployment queue first.",
            "runbook:release",
        ))
        .unwrap();
    let manifest = dir.path().join("benchmark.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schema": "synaptic.memory-benchmark/v1",
            "cases": [
                {
                    "name": "localize authentication deadlock",
                    "query": "session mutex deadlock",
                    "expected_sources": ["incident:AUTH-42"]
                },
                {
                    "name": "localize rollback order",
                    "query": "rollback deployment queue",
                    "expected_sources": ["runbook:release"]
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let principal = MemoryPrincipal::restricted("evaluator").with_repository("repo");

    let report = run_benchmark_file(&store, &manifest, &principal).unwrap();
    assert_eq!(report.cases, 2);
    assert_eq!(report.recall_at_1, 1.0);
    assert_eq!(report.recall_at_5, 1.0);
    assert_eq!(report.mean_reciprocal_rank, 1.0);
    assert!(report.mean_candidate_fraction < 0.1, "{report:#?}");
    assert!(report.misses.is_empty());
    enforce_benchmark_gate(
        &report,
        BenchmarkGate {
            min_recall_at_5: 1.0,
            min_mean_reciprocal_rank: 1.0,
            max_mean_candidate_fraction: 0.1,
        },
    )
    .unwrap();
    assert!(
        enforce_benchmark_gate(
            &report,
            BenchmarkGate {
                min_recall_at_5: 1.01,
                min_mean_reciprocal_rank: 1.0,
                max_mean_candidate_fraction: 0.1,
            },
        )
        .is_err()
    );
}
