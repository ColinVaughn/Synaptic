use std::path::Path;
use std::sync::Mutex;

use synaptic_api::{
    failed_attempt_summary, run_repair_attempts, GateOutcome, GateResult, GeneratedPatch,
    PatchGenerator, PatchInspection, PatchPolicy, PatchVerifier, RepairBrief, RepairFailure,
    VerificationReport,
};

struct SequencedGenerator {
    patches: Mutex<Vec<GeneratedPatch>>,
    failures_seen: Mutex<Vec<RepairFailure>>,
}

impl PatchGenerator for SequencedGenerator {
    fn generate(
        &self,
        _brief: &RepairBrief,
        _worktree: &Path,
    ) -> Result<GeneratedPatch, synaptic_api::PatchGenerationError> {
        Ok(self.patches.lock().unwrap().remove(0))
    }

    fn retry(
        &self,
        _brief: &RepairBrief,
        _worktree: &Path,
        _prior_patch: &GeneratedPatch,
        failure: &RepairFailure,
    ) -> Result<GeneratedPatch, synaptic_api::PatchGenerationError> {
        self.failures_seen.lock().unwrap().push(failure.clone());
        Ok(self.patches.lock().unwrap().remove(0))
    }
}

struct SequencedVerifier(Mutex<Vec<VerificationReport>>);

impl PatchVerifier for SequencedVerifier {
    fn verify(
        &self,
        _worktree: &Path,
        _patch: &GeneratedPatch,
        _inspection: &PatchInspection,
    ) -> VerificationReport {
        self.0.lock().unwrap().remove(0)
    }
}

fn patch(path: &str, value: &str) -> GeneratedPatch {
    GeneratedPatch {
        unified_diff: format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-old\n+{value}\n"),
        rationale: "fixture".into(),
    }
}

fn report(outcome: GateOutcome) -> VerificationReport {
    VerificationReport::from_gates(vec![GateResult {
        gate: "tests".into(),
        outcome,
        detail: "fixture".into(),
        duration_ms: 0,
    }])
}

#[test]
fn retries_are_bounded_and_receive_only_prior_failure_context() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/client.ts"), "old\n").unwrap();
    let brief: RepairBrief = serde_json::from_value(serde_json::json!({
        "version":1,"id":"run_1","repository_identity":"repo","base_sha":"abc",
        "event":{"version":1,"id":"event","vendor":"acme","occurred_at":1,"source":{"uri":"x","revision":"1","content_digest":"d","fetched_at":1,"adapter_version":1,"evidence_kind":"openapi"},"changes":[]},
        "applicability":{"version":1,"event_id":"event","vendor":"acme","state":"applicable","reasons":[],"matched_change_ids":[],"bindings":[],"seed_node_ids":[],"observed_versions":[]},
        "usage_bindings":[],"impact":{"version":1,"seed_node_ids":[],"blast_radius":[],"blast_radius_total":0,"at_risk_tests":[]},
        "official_evidence":[],"source_slices":[],"memory":[],"dynamic_hazards":[],"allowed_files":["src/client.ts"],"required_tests":[],"verification":[]
    })).unwrap();
    let generator = SequencedGenerator {
        patches: Mutex::new(vec![
            patch("src/unrelated.ts", "bad"),
            patch("src/client.ts", "first"),
            patch("src/client.ts", "fixed"),
        ]),
        failures_seen: Mutex::new(Vec::new()),
    };
    let verifier = SequencedVerifier(Mutex::new(vec![
        report(GateOutcome::Failed),
        report(GateOutcome::Passed),
    ]));
    let outcome = run_repair_attempts(
        &brief,
        root.path(),
        &PatchPolicy {
            allowed_files: brief.allowed_files.clone(),
            max_files: 2,
            max_changed_lines: 20,
            ..PatchPolicy::default()
        },
        &generator,
        &verifier,
        3,
    )
    .unwrap();
    assert!(outcome.verified);
    assert_eq!(outcome.attempts.len(), 3);
    let summary = failed_attempt_summary(&outcome).expect("failed attempts should be retained");
    assert!(summary.contains("attempt 1"));
    assert!(summary.contains("patch policy"));
    assert!(summary.contains("attempt 2"));
    assert!(summary.contains("verification"));
    assert_eq!(generator.failures_seen.lock().unwrap().len(), 2);
    assert_eq!(outcome.final_patch.unwrap().rationale, "fixture");
}

#[test]
fn inconclusive_gate_never_becomes_verified() {
    assert!(!report(GateOutcome::Inconclusive).verified);
}
