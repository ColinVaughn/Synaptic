use std::path::Path;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use synaptic_memory::{
    MemoryKind, MemoryRecord, MemoryStore, SourceArtifact, SymbolAnchor, VerificationStatus,
};

fn synaptic(args: &[&str], dir: &Path) -> std::process::Output {
    Command::cargo_bin("synaptic")
        .unwrap()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run synaptic")
}

fn write_graph(root: &Path) {
    std::fs::create_dir_all(root.join("synaptic-out")).unwrap();
    std::fs::write(
        root.join("synaptic-out/graph.json"),
        r#"{
            "directed": true,
            "nodes": [
                {"id":"helper", "label":"helper", "file_type":"code", "source_file":"helper.py", "kind":"function", "visibility":"public"},
                {"id":"helper_test", "label":"helper works", "file_type":"code", "source_file":"tests/helper_test.py", "kind":"function", "_is_test":true}
            ],
            "links": [
                {"source":"helper_test", "target":"helper", "relation":"calls", "confidence":"EXTRACTED", "source_file":"tests/helper_test.py", "weight":1.0}
            ],
            "built_at_commit":"base-1"
        }"#,
    )
    .unwrap();
}

fn write_invariant(root: &Path) {
    let store = MemoryStore::open(root.join(".synaptic/memory"));
    let mut record = MemoryRecord::new(
        "helper-contract-invariant",
        MemoryKind::Invariant,
        "Preserve helper compatibility",
        "Existing callers must keep working",
        "fixture",
        2,
        vec![SourceArtifact {
            kind: "adr".into(),
            uri: "docs/adr/helper.md".into(),
            revision: Some("base-1".into()),
            digest: None,
        }],
    );
    record.affected_symbols = vec![SymbolAnchor {
        node_id: "helper".into(),
        label: "helper".into(),
        source_file: "helper.py".into(),
        repo: None,
        commit: Some("base-1".into()),
        confidence: 1.0,
    }];
    record.verification.status = VerificationStatus::Passed;
    store.record(&record).unwrap();
}

#[test]
fn contract_cli_recovers_persists_and_verifies_fail_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_graph(root);
    write_invariant(root);

    let recovered = synaptic(
        &[
            "contract",
            "recover",
            "change helper",
            "--base",
            "base-1",
            "--approve",
            "--json",
        ],
        root,
    );
    assert!(
        recovered.status.success(),
        "recover: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    let contract: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(contract["state"], "approved");
    assert!(
        contract["requirements"]
            .as_array()
            .unwrap()
            .iter()
            .any(|requirement| requirement["category"] == "historical_invariant")
    );
    let id = contract["id"].as_str().unwrap();
    let proof = contract["proofs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|proof| proof["kind"] == "affected_tests")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let historical_proof = contract["proofs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|proof| proof["kind"] == "manual_attestation")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let dir = root.join(".synaptic/contracts").join(id);
    assert!(dir.join("v1.json").is_file());
    assert!(dir.join("v2.json").is_file());

    let repeated = synaptic(
        &[
            "contract",
            "recover",
            "change helper",
            "--base",
            "base-1",
            "--approve",
            "--json",
        ],
        root,
    );
    assert!(
        repeated.status.success(),
        "repeat recover: {}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    let repeated_contract: Value = serde_json::from_slice(&repeated.stdout).unwrap();
    assert_eq!(repeated_contract["id"], id);
    assert_eq!(repeated_contract["revision"], 4);
    assert!(dir.join("v3.json").is_file());
    assert!(dir.join("v4.json").is_file());

    let missing = synaptic(
        &[
            "contract",
            "verify",
            id,
            "helper.py",
            "--base",
            "base-1",
            "--json",
        ],
        root,
    );
    assert!(!missing.status.success(), "missing proof must fail closed");

    let verified = synaptic(
        &[
            "contract",
            "verify",
            id,
            "helper.py",
            "--base",
            "base-1",
            "--passed-proof",
            proof,
            "--passed-proof",
            historical_proof,
            "--json",
        ],
        root,
    );
    assert!(
        verified.status.success(),
        "verify: {}",
        String::from_utf8_lossy(&verified.stderr)
    );
    let report: Value = serde_json::from_slice(&verified.stdout).unwrap();
    assert_eq!(report["state"], "satisfied");
}
