use synaptic_api::{
    ApiChangeEvent, ApiRunStore, GateOutcome, GateResult, GeneratedPatch, RepairAttempt,
    RepairBrief, RepairOutcome, RunState, VerificationReport, VerifiedRunHandoff,
};

fn event() -> ApiChangeEvent {
    serde_json::from_value(serde_json::json!({
        "version":1,"id":"event_123","vendor":"acme","release":"v2","occurred_at":1,
        "source":{"uri":"https://acme.example/openapi","revision":"v2","content_digest":"digest","fetched_at":1,"adapter_version":1,"evidence_kind":"openapi"},
        "changes":[]
    })).unwrap()
}

fn brief(run_id: &str, event: &ApiChangeEvent) -> RepairBrief {
    serde_json::from_value(serde_json::json!({
        "version":1,"id":run_id,"repository_identity":"repo","base_sha":"abc123",
        "event":event,
        "applicability":{"version":1,"event_id":"event_123","vendor":"acme","state":"applicable","reasons":["applicable"],"matched_change_ids":[],"bindings":[],"seed_node_ids":[],"observed_versions":["1.0.0"]},
        "usage_bindings":[],
        "impact":{"version":1,"seed_node_ids":[],"blast_radius":[],"blast_radius_total":0,"at_risk_tests":[]},
        "official_evidence":[],"source_slices":[],"memory":[],"dynamic_hazards":[],
        "allowed_files":["src/client.ts"],"required_tests":[],
        "verification":[{"gate":"all","required":true,"description":"all gates pass"}]
    })).unwrap()
}

fn verified_fixture() -> (tempfile::TempDir, VerifiedRunHandoff) {
    let root = tempfile::tempdir().unwrap();
    let store = ApiRunStore::new(root.path());
    let mut run = store
        .begin("repo", "abc123", "event_123", "policy_digest")
        .unwrap();
    store
        .transition(&mut run, RunState::Repairing, None, None)
        .unwrap();
    let verification = VerificationReport::from_gates(vec![GateResult {
        gate: "all".into(),
        outcome: GateOutcome::Passed,
        detail: "passed".into(),
        duration_ms: 1,
    }]);
    store
        .transition(
            &mut run,
            RunState::Verified,
            Some(verification.clone()),
            None,
        )
        .unwrap();
    let event = event();
    let brief = brief(&run.id, &event);
    let patch = "diff --git a/src/client.ts b/src/client.ts\n--- a/src/client.ts\n+++ b/src/client.ts\n@@ -1 +1 @@\n-old\n+new\n".to_string();
    let patch_digest = blake3::hash(patch.as_bytes()).to_hex().to_string();
    let outcome = RepairOutcome {
        version: 1,
        run_id: run.id.clone(),
        verified: true,
        attempts: vec![RepairAttempt {
            number: 1,
            patch_digest,
            rationale: "migrate".into(),
            inspection: None,
            verification: Some(verification.clone()),
            failure: None,
        }],
        final_patch: Some(GeneratedPatch {
            unified_diff: patch.clone(),
            rationale: "migrate".into(),
        }),
        final_verification: Some(verification.clone()),
    };
    let handoff = VerifiedRunHandoff::new(run, event, brief, outcome, verification, patch).unwrap();
    (root, handoff)
}

#[test]
fn verified_handoff_round_trips_and_imports_the_exact_run_identity() {
    let (_source, handoff) = verified_fixture();
    handoff.verify().unwrap();
    let encoded = serde_json::to_vec_pretty(&handoff).unwrap();
    let decoded: VerifiedRunHandoff = serde_json::from_slice(&encoded).unwrap();
    decoded.verify().unwrap();

    let target = tempfile::tempdir().unwrap();
    let store = ApiRunStore::new(target.path());
    store.import_verified(&decoded.run).unwrap();
    assert_eq!(store.load(&decoded.run.id).unwrap(), decoded.run);
    store.import_verified(&decoded.run).unwrap();
}

#[test]
fn verified_handoff_rejects_patch_policy_and_verification_tampering() {
    let (_source, handoff) = verified_fixture();

    let mut patch = handoff.clone();
    patch.patch.push_str("# tampered\n");
    assert!(patch.verify().is_err());

    let mut policy = handoff.clone();
    policy.run.policy_digest = "different".into();
    assert!(policy.verify().is_err());

    let mut verification = handoff;
    verification.verification.verified = false;
    assert!(verification.verify().is_err());
}
