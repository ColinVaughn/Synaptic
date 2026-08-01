use std::process::Command;

use assert_cmd::cargo::cargo_bin_cmd;

fn git(root: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn memory_record_search_status_and_git_ingest_work_end_to_end() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init"]);
    git(repo.path(), &["config", "user.email", "test@example.com"]);
    git(repo.path(), &["config", "user.name", "Synaptic Test"]);
    std::fs::write(repo.path().join("auth.rs"), "fn refresh_token() {}\n").unwrap();
    git(repo.path(), &["add", "auth.rs"]);
    git(repo.path(), &["commit", "-m", "Add refresh token"]);
    std::fs::create_dir_all(repo.path().join("docs/adr")).unwrap();
    std::fs::write(
        repo.path().join("docs/adr/ADR-014.md"),
        "# Retain refresh entrypoint\n\
         Status: Accepted\n\
         Synaptic-Symbols: refresh_token\n\n\
         Production resolves the refresh entrypoint dynamically.\n",
    )
    .unwrap();

    cargo_bin_cmd!("synaptic")
        .args([
            "memory",
            "record",
            "--root",
            repo.path().to_str().unwrap(),
            "--idempotency-key",
            "agent-7",
            "--title",
            "Rejected refresh attempt",
            "--summary",
            "Network I/O under the session mutex deadlocked refresh.",
            "--outcome",
            "failed",
            "--source-uri",
            "agent://task/7",
            "--symbol",
            "refresh_token",
            "--verification-status",
            "failed",
        ])
        .assert()
        .success();

    let output = cargo_bin_cmd!("synaptic")
        .args([
            "memory",
            "search",
            "session mutex deadlock",
            "--root",
            repo.path().to_str().unwrap(),
            "--symbol",
            "refresh_token",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["total"], 1, "{value}");
    assert_eq!(value["hits"][0]["record"]["kind"], "failed_attempt");

    cargo_bin_cmd!("synaptic")
        .args([
            "memory",
            "ingest",
            "HEAD",
            "--root",
            repo.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    cargo_bin_cmd!("synaptic")
        .args([
            "memory",
            "ingest-docs",
            "--root",
            repo.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let decision = cargo_bin_cmd!("synaptic")
        .args([
            "memory",
            "search",
            "",
            "--root",
            repo.path().to_str().unwrap(),
            "--symbol",
            "refresh_token",
            "--kind",
            "architecture_decision",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(decision.status.success());
    let decision: serde_json::Value = serde_json::from_slice(&decision.stdout).unwrap();
    assert_eq!(decision["total"], 1, "{decision}");
    assert_eq!(
        decision["hits"][0]["record"]["sources"][0]["uri"],
        "file:docs/adr/ADR-014.md"
    );

    let output = cargo_bin_cmd!("synaptic")
        .args([
            "memory",
            "status",
            "--root",
            repo.path().to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["records"], 3, "{value}");
    assert_eq!(value["by_kind"]["failed_attempt"], 1);
    assert_eq!(value["by_kind"]["change_episode"], 1);
    assert_eq!(value["by_kind"]["architecture_decision"], 1);
}

#[test]
fn advanced_memory_import_refresh_compact_sync_and_eval_work_end_to_end() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("auth.rs"),
        "pub fn refresh_session() -> bool { true }\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("CONTRIBUTING.md"),
        "# Contributing\n\nRun cargo test before opening a pull request.\n",
    )
    .unwrap();
    cargo_bin_cmd!("synaptic")
        .args(["extract", repo.path().to_str().unwrap()])
        .assert()
        .success();
    let artifacts = repo.path().join("artifacts.json");
    std::fs::write(
        &artifacts,
        serde_json::to_vec(&serde_json::json!({
            "schema": "synaptic.memory-artifacts/v1",
            "artifacts": [{
                "kind": "issue",
                "external_id": "ISSUE-42",
                "title": "Authentication mutex deadlock",
                "summary": "Session refresh held a mutex during network I/O.",
                "source_uri": "https://tracker.example/issues/42",
                "repository": "example/repo",
                "occurred_at": 100,
                "affected_symbols": ["refresh_session"],
                "affected_files": ["auth.rs"],
                "scope": "repository"
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    cargo_bin_cmd!("synaptic")
        .args([
            "memory",
            "import-artifacts",
            "--root",
            repo.path().to_str().unwrap(),
            "--file",
            artifacts.to_str().unwrap(),
        ])
        .assert()
        .success();
    cargo_bin_cmd!("synaptic")
        .args(["memory", "refresh", "--root", repo.path().to_str().unwrap()])
        .assert()
        .success();
    cargo_bin_cmd!("synaptic")
        .args([
            "memory",
            "compact",
            "--root",
            repo.path().to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success();

    let bundle = repo.path().join("team-memory.json");
    cargo_bin_cmd!("synaptic")
        .args([
            "memory",
            "export",
            "--root",
            repo.path().to_str().unwrap(),
            "--output",
            bundle.to_str().unwrap(),
            "--principal",
            "bob",
            "--repository-claim",
            "example/repo",
        ])
        .assert()
        .success();
    let target = tempfile::tempdir().unwrap();
    cargo_bin_cmd!("synaptic")
        .args([
            "memory",
            "sync",
            "--root",
            target.path().to_str().unwrap(),
            "--bundle",
            bundle.to_str().unwrap(),
            "--principal",
            "bob",
            "--repository-claim",
            "example/repo",
        ])
        .assert()
        .success();
    let benchmark = target.path().join("benchmark.json");
    std::fs::write(
        &benchmark,
        serde_json::to_vec(&serde_json::json!({
            "schema": "synaptic.memory-benchmark/v1",
            "cases": [{
                "name": "auth issue",
                "query": "authentication mutex deadlock",
                "expected_sources": ["https://tracker.example/issues/42"]
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let output = cargo_bin_cmd!("synaptic")
        .args([
            "memory",
            "eval",
            "--root",
            target.path().to_str().unwrap(),
            "--manifest",
            benchmark.to_str().unwrap(),
            "--principal",
            "bob",
            "--repository-claim",
            "example/repo",
            "--min-recall-at-5",
            "1",
            "--min-mrr",
            "1",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["recall_at_5"], 1.0);
    assert_eq!(report["mean_reciprocal_rank"], 1.0);

    let hidden = cargo_bin_cmd!("synaptic")
        .args([
            "memory",
            "search",
            "authentication",
            "--root",
            target.path().to_str().unwrap(),
            "--principal",
            "outsider",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(hidden.status.success());
    let hidden: serde_json::Value = serde_json::from_slice(&hidden.stdout).unwrap();
    assert_eq!(hidden["total"], 0);
}
