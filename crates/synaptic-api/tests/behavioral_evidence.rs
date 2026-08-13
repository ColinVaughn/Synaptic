use synaptic_api::{BehavioralOutcome, RuntimeSurfaceKind, import_behavioral_evidence};

#[test]
fn canary_and_error_evidence_is_sanitized_and_becomes_review_only() {
    let payload = serde_json::json!({
        "version": 1,
        "environment": "staging",
        "window_start_unix_nano": 100,
        "window_end_unix_nano": 200,
        "observations": [{
            "kind": "http",
            "protocol": "https",
            "method": "GET",
            "authority": "API.ACME.TEST",
            "path": "/users/123456?token=secret",
            "outcome": "server_error",
            "occurrences": 3
        }]
    });

    let report =
        import_behavioral_evidence("canary.json", &serde_json::to_vec(&payload).unwrap()).unwrap();
    assert!(report.complete_window);
    assert_eq!(report.observations[0].kind, RuntimeSurfaceKind::Http);
    assert_eq!(
        report.observations[0].outcome,
        BehavioralOutcome::ServerError
    );
    assert_eq!(report.observations[0].path.as_deref(), Some("/users/:id"));
    assert_eq!(report.review_candidates.len(), 1);
    let serialized = serde_json::to_string(&report).unwrap();
    assert!(!serialized.contains("secret"));

    let runtime = report.as_runtime_evidence();
    assert_eq!(runtime.observations.len(), 1);
    assert_eq!(runtime.observations[0].occurrences, 3);
}

#[test]
fn behavioral_import_rejects_payload_fields_and_invalid_windows() {
    let with_payload = br#"{
      "version":1,"environment":"test","window_start_unix_nano":1,"window_end_unix_nano":2,
      "observations":[{"kind":"rpc","protocol":"grpc","method":"CALL","service":"acme.Service","operation":"Get","outcome":"timeout","occurrences":2,"payload":"secret"}]
    }"#;
    assert!(import_behavioral_evidence("bad.json", with_payload).is_err());

    let invalid_window = br#"{
      "version":1,"environment":"test","window_start_unix_nano":3,"window_end_unix_nano":2,
      "observations":[]
    }"#;
    assert!(import_behavioral_evidence("bad-window.json", invalid_window).is_err());
}
