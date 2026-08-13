use std::fs;
use std::path::{Path, PathBuf};

use synaptic_api::{
    ApiChangeEvent, GateOutcome, RelevanceAssessment, RepairBrief, SourceArtifact,
    VerificationReport, VersionRange, analyze_api_coverage, diff_contracts,
    import_behavioral_evidence, normalize_openapi, sanitize_release_text,
};
use synaptic_core::GraphData;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/api-maintenance")
}

fn required_properties(schema: &serde_json::Value, value: &serde_json::Value) {
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    for field in schema["required"].as_array().unwrap() {
        let field = field.as_str().unwrap();
        assert!(
            value.get(field).is_some(),
            "serialized artifact lacks required {field}"
        );
    }
}

#[test]
fn contract_and_hostile_release_fixture_replay_without_network() {
    let before = fs::read(root().join("contracts/payments-before.json")).unwrap();
    let after = fs::read(root().join("contracts/payments-after.yaml")).unwrap();
    let old = normalize_openapi("stripe", &before).unwrap();
    let new = normalize_openapi("stripe", &after).unwrap();
    let event = diff_contracts(
        &old,
        &new,
        SourceArtifact {
            uri: "fixture://payments-after".into(),
            revision: "2026-07-31".into(),
            etag: None,
            last_modified: None,
            content_digest: new.digest.clone(),
            fetched_at: 1,
            adapter_version: 1,
            evidence_kind: "openapi".into(),
        },
        VersionRange::parse(">=10.0.0,<12.0.0").unwrap(),
    )
    .unwrap();
    assert!(!event.changes.is_empty());
    assert!(
        event
            .changes
            .iter()
            .all(|change| !change.evidence.is_empty())
    );

    let hostile = fs::read_to_string(root().join("release-notes/hostile.html")).unwrap();
    let sanitized = sanitize_release_text(&hostile);
    assert!(!sanitized.to_ascii_lowercase().contains("<script"));
    assert!(!sanitized.contains("curl "));
    assert!(!sanitized.contains("BEGIN PRIVATE KEY"));
}

#[test]
fn versioned_json_schemas_cover_the_engine_artifacts() {
    let event: ApiChangeEvent =
        serde_json::from_slice(&fs::read(root().join("examples/api-change-event.json")).unwrap())
            .unwrap();
    let assessment: RelevanceAssessment = serde_json::from_slice(
        &fs::read(root().join("examples/relevance-assessment.json")).unwrap(),
    )
    .unwrap();
    let brief: RepairBrief =
        serde_json::from_slice(&fs::read(root().join("examples/repair-brief.json")).unwrap())
            .unwrap();
    let verification: VerificationReport = serde_json::from_slice(
        &fs::read(root().join("examples/verification-report.json")).unwrap(),
    )
    .unwrap();
    assert!(verification.verified);
    assert!(
        verification
            .gates
            .iter()
            .all(|gate| gate.outcome == GateOutcome::Passed)
    );
    let binding = serde_json::to_value(&assessment.bindings[0]).unwrap();
    let coverage =
        serde_json::to_value(analyze_api_coverage(&GraphData::default(), &[], None)).unwrap();
    let behavioral = serde_json::to_value(
        import_behavioral_evidence(
            "fixture://canary",
            br#"{"version":1,"environment":"test","window_start_unix_nano":1,"window_end_unix_nano":2,"observations":[]}"#,
        )
        .unwrap(),
    )
    .unwrap();

    for (schema_name, value) in [
        (
            "api-change-event.schema.json",
            serde_json::to_value(event).unwrap(),
        ),
        ("usage-binding.schema.json", binding),
        (
            "relevance-assessment.schema.json",
            serde_json::to_value(assessment).unwrap(),
        ),
        (
            "repair-brief.schema.json",
            serde_json::to_value(brief).unwrap(),
        ),
        (
            "verification-report.schema.json",
            serde_json::to_value(verification).unwrap(),
        ),
        ("api-coverage-report.schema.json", coverage),
        ("behavioral-evidence.schema.json", behavioral),
    ] {
        let schema: serde_json::Value = serde_json::from_slice(
            &fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("schemas")
                    .join(schema_name),
            )
            .unwrap(),
        )
        .unwrap();
        required_properties(&schema, &value);
    }
}
