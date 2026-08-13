use synaptic_api::{
    BreakingChangeKind, SourceArtifact, VersionRange, diff_contracts, normalize_openapi,
};

fn source() -> SourceArtifact {
    SourceArtifact {
        uri: "fixture".into(),
        revision: "2".into(),
        etag: None,
        last_modified: None,
        content_digest: String::new(),
        fetched_at: 2,
        adapter_version: 1,
        evidence_kind: "openapi".into(),
    }
}

#[test]
fn explicit_rules_cover_renames_enums_authentication_and_webhooks() {
    let old = br#"{
      "openapi":"3.0.0",
      "paths":{"/v1/widgets":{"post":{"operationId":"oldName","security":[{"key":[]}],
        "requestBody":{"content":{"application/json":{"schema":{"type":"object","properties":{"old_field":{"type":"string"},"mode":{"type":"string","enum":["a","b"]}}}}}},
        "responses":{"200":{"content":{"application/json":{"schema":{"type":"object","properties":{"old_result":{"type":"string"},"state":{"type":"string","enum":["a"]}}}}}}}
      }}},
      "webhooks":{"widget.changed":{"post":{"operationId":"widgetChanged","requestBody":{"content":{"application/json":{"schema":{"type":"object","properties":{"payload":{"type":"string"}}}}}},"responses":{"200":{"description":"ok"}}}}}
    }"#;
    let new = br#"{
      "openapi":"3.0.0",
      "paths":{"/v1/widgets":{"post":{"operationId":"newName","security":[{"oauth":[]}],
        "requestBody":{"content":{"application/json":{"schema":{"type":"object","properties":{"new_field":{"type":"string"},"mode":{"type":"string","enum":["a"]}}}}}},
        "responses":{"200":{"content":{"application/json":{"schema":{"type":"object","properties":{"new_result":{"type":"string"},"state":{"type":"string","enum":["a","b"]}}}}}}}
      }}},
      "webhooks":{"widget.changed":{"post":{"operationId":"widgetChanged","requestBody":{"content":{"application/json":{"schema":{"type":"object","properties":{}}}}},"responses":{"200":{"description":"ok"}}}}}
    }"#;
    let event = diff_contracts(
        &normalize_openapi("acme", old).unwrap(),
        &normalize_openapi("acme", new).unwrap(),
        source(),
        VersionRange::any(),
    )
    .unwrap();
    let kinds = event
        .changes
        .iter()
        .map(|change| change.kind)
        .collect::<std::collections::BTreeSet<_>>();
    for expected in [
        BreakingChangeKind::OperationRenamed,
        BreakingChangeKind::RequestFieldRenamed,
        BreakingChangeKind::ResponseFieldRenamed,
        BreakingChangeKind::RequestEnumNarrowed,
        BreakingChangeKind::ResponseEnumChanged,
        BreakingChangeKind::AuthenticationOrVersionBehaviorChanged,
        BreakingChangeKind::WebhookChanged,
    ] {
        assert!(kinds.contains(&expected), "missing {expected:?}: {kinds:?}");
    }
}

#[test]
fn referenced_response_schemas_are_diffed() {
    let old = br##"{"openapi":"3.0.0","paths":{"/x":{"get":{"operationId":"getX","responses":{"200":{"$ref":"#/components/responses/X"}}}}},"components":{"responses":{"X":{"content":{"application/json":{"schema":{"$ref":"#/components/schemas/X"}}}}},"schemas":{"X":{"type":"object","properties":{"id":{"type":"string"}}}}}}"##;
    let new = br##"{"openapi":"3.0.0","paths":{"/x":{"get":{"operationId":"getX","responses":{"200":{"$ref":"#/components/responses/X"}}}}},"components":{"responses":{"X":{"content":{"application/json":{"schema":{"$ref":"#/components/schemas/X"}}}}},"schemas":{"X":{"type":"object","properties":{}}}}}"##;
    let event = diff_contracts(
        &normalize_openapi("acme", old).unwrap(),
        &normalize_openapi("acme", new).unwrap(),
        source(),
        VersionRange::any(),
    )
    .unwrap();
    assert!(
        event
            .changes
            .iter()
            .any(|change| change.kind == BreakingChangeKind::ResponseFieldRemoved)
    );
}
