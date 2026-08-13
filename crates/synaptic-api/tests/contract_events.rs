use std::fs;

use synaptic_api::{
    ApiEventStore, BreakingChangeKind, SourceArtifact, VersionRange, diff_contracts,
    normalize_openapi,
};

const OLD: &str = r#"{
  "openapi":"3.1.0",
  "paths":{
    "/v1/customers":{"post":{
      "operationId":"createCustomer",
      "requestBody":{"content":{"application/json":{"schema":{
        "type":"object","required":["email"],"properties":{
          "email":{"type":"string"},"nickname":{"type":"string"}
        }}}}},
      "responses":{"200":{"content":{"application/json":{"schema":{
        "type":"object","properties":{
          "id":{"type":"string"},"status":{"type":"string","enum":["active","inactive"]}
        }}}}}}
    }}
  }
}"#;

const NEW_YAML: &str = r#"
openapi: 3.1.0
paths:
  /v2/customers:
    post:
      operationId: createCustomer
      requestBody:
        content:
          application/json:
            schema:
              type: object
              required: [email, name]
              properties:
                email: {type: integer}
                name: {type: string}
                nickname: {type: string}
                optional_note: {type: string}
      responses:
        "200":
          content:
            application/json:
              schema:
                type: object
                properties:
                  id: {type: string}
"#;

fn source() -> SourceArtifact {
    SourceArtifact {
        uri: "https://contracts.example.test/payments.yaml".into(),
        revision: "release-2".into(),
        etag: Some("v2".into()),
        last_modified: None,
        content_digest: String::new(),
        fetched_at: 1_785_549_758,
        adapter_version: 1,
        evidence_kind: "openapi".into(),
    }
}

#[test]
fn openapi_json_and_yaml_normalize_and_diff_with_explicit_breaking_rules() {
    let old = normalize_openapi("stripe", OLD.as_bytes()).unwrap();
    let new = normalize_openapi("stripe", NEW_YAML.as_bytes()).unwrap();
    assert_eq!(old.operations.len(), 1);
    assert_eq!(new.operations.len(), 1);
    assert_ne!(old.digest, new.digest);

    let event = diff_contracts(
        &old,
        &new,
        source(),
        VersionRange::parse(">=1.0.0,<2.0.0").unwrap(),
    )
    .unwrap();
    let kinds = event
        .changes
        .iter()
        .map(|change| change.kind)
        .collect::<Vec<_>>();
    assert!(kinds.contains(&BreakingChangeKind::PathOrMethodChanged));
    assert!(kinds.contains(&BreakingChangeKind::RequiredRequestFieldAdded));
    assert!(kinds.contains(&BreakingChangeKind::RequestFieldTypeChanged));
    assert!(kinds.contains(&BreakingChangeKind::ResponseFieldRemoved));
    assert!(
        !event
            .changes
            .iter()
            .any(|change| change.migration_summary.contains("optional_note"))
    );
    assert!(
        event
            .changes
            .iter()
            .all(|change| !change.evidence.is_empty())
    );

    let replay = diff_contracts(
        &old,
        &new,
        source(),
        VersionRange::parse(">=1.0.0,<2.0.0").unwrap(),
    )
    .unwrap();
    assert_eq!(event, replay, "normalization and event IDs are byte-stable");
}

#[test]
fn immutable_event_store_is_idempotent_and_detects_tampering() {
    let old = normalize_openapi("stripe", OLD.as_bytes()).unwrap();
    let new = normalize_openapi("stripe", NEW_YAML.as_bytes()).unwrap();
    let event = diff_contracts(&old, &new, source(), VersionRange::any()).unwrap();
    let repo = tempfile::tempdir().unwrap();
    let store = ApiEventStore::new(repo.path());
    let first = store.put_event(&event).unwrap();
    let second = store.put_event(&event).unwrap();
    assert_eq!(first, second);
    assert_eq!(store.load_event(&event.id).unwrap(), event);

    fs::write(&first, b"{}\n").unwrap();
    let error = store.put_event(&event).unwrap_err().to_string();
    assert!(error.contains("integrity"), "{error}");
}

#[test]
fn source_lock_updates_are_race_safe() {
    let root = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(ApiEventStore::new(root.path()));
    let handles = (0..8)
        .map(|_| {
            let store = store.clone();
            std::thread::spawn(move || {
                store
                    .record_source(synaptic_api::SourceLockState {
                        vendor: "acme".into(),
                        source_uri: "fixture".into(),
                        revision: "1".into(),
                        content_digest: "abc".into(),
                        etag: None,
                        last_modified: None,
                        contract_digest: None,
                        checked_at: 1,
                    })
                    .unwrap();
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(
        store
            .source_state("acme", "fixture")
            .unwrap()
            .unwrap()
            .revision,
        "1"
    );
}
