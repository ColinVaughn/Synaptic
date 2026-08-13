use synaptic_api::{
    ApiChangeEvent, ApiInventory, ApiMaintenanceConfig, ApplicabilityState, BriefBudget,
    EvidenceSpan, SourceArtifact, VendorRegistry, build_repair_brief, evaluate_relevance,
};
use synaptic_core::GraphData;
use synaptic_graph::KnowledgeGraph;

fn registry() -> VendorRegistry {
    VendorRegistry::new(
        ApiMaintenanceConfig::parse(
            r#"
schema = 1
require_resolved_version = true
[[vendors]]
id = "acme"
packages = ["npm:acme-sdk"]
hosts = ["api.acme.example"]
auto_repair_confidence = 0.9
"#,
        )
        .unwrap(),
    )
    .unwrap()
}

fn event() -> ApiChangeEvent {
    serde_json::from_value(serde_json::json!({
        "version": 1,
        "id": "api_event_acme_123",
        "vendor": "acme",
        "release": "v2",
        "occurred_at": 42,
        "source": {
            "uri": "https://acme.example/openapi.json",
            "revision": "v2",
            "content_digest": "abc",
            "fetched_at": 42,
            "adapter_version": 1,
            "evidence_kind": "openapi"
        },
        "changes": [{
            "change_id": "change_1",
            "kind": "operation_removed",
            "affected_versions": {"requirement": ">=1.0.0, <2.0.0"},
            "old_operation": {
                "id": "api_operation:acme:create",
                "vendor": "acme",
                "protocol": "https",
                "method": "POST",
                "canonical_path": "/v1/widgets"
            },
            "old_sdk_symbols": [], "new_sdk_symbols": [],
            "migration_summary": "create moved to /v2/widgets",
            "evidence": [], "confidence": 1.0
        }]
    }))
    .unwrap()
}

fn graph(version: Option<&str>, include_usage: bool) -> GraphData {
    let mut links = vec![serde_json::json!({
        "source":"test:create_widget", "target":"fn:create_widget", "relation":"calls",
        "confidence":"EXTRACTED", "source_file":"tests/client.test.ts"
    })];
    if include_usage {
        links.push(serde_json::json!({
            "source":"fn:create_widget", "target":"api_operation:acme:create", "relation":"uses_api",
            "confidence":"EXTRACTED", "confidence_score":1.0, "source_file":"src/client.ts",
            "source_location":"2:3", "binding_basis":"sdk_symbol", "api_vendor":"acme",
            "sdk_package":"npm:acme-sdk", "sdk_member_chain":"widgets.create",
            "installed_sdk_version":version
        }));
    }
    serde_json::from_value(serde_json::json!({
        "directed": true,
        "nodes": [
            {"id":"api_operation:acme:create", "label":"POST /v1/widgets", "file_type":"concept", "source_file":"", "_node_type":"api_operation", "vendor":"acme"},
            {"id":"fn:create_widget", "label":"create_widget", "file_type":"code", "source_file":"src/client.ts", "kind":"function"},
            {"id":"test:create_widget", "label":"create_widget test", "file_type":"code", "source_file":"tests/client.test.ts", "kind":"function", "_is_test":true},
            {"id":"fn:unrelated", "label":"unrelated", "file_type":"code", "source_file":"src/unrelated.ts", "kind":"function", "community":99}
        ],
        "links": links
    }))
    .unwrap()
}

#[test]
fn version_usage_confidence_and_scope_gates_fail_closed() {
    let registry = registry();
    let inventory = ApiInventory::default();
    let relevant = evaluate_relevance(
        &event(),
        &registry,
        &inventory,
        &graph(Some("1.5.0"), true),
        &["src/".into()],
    );
    assert_eq!(relevant.state, ApplicabilityState::Applicable);
    assert_eq!(relevant.bindings.len(), 1);

    let unknown = evaluate_relevance(
        &event(),
        &registry,
        &inventory,
        &graph(None, true),
        &["src/".into()],
    );
    assert_eq!(unknown.state, ApplicabilityState::ReviewRequired);
    let unused = evaluate_relevance(
        &event(),
        &registry,
        &inventory,
        &graph(Some("1.5.0"), false),
        &[],
    );
    assert_eq!(unused.state, ApplicabilityState::NotApplicable);
    let out_of_range = evaluate_relevance(
        &event(),
        &registry,
        &inventory,
        &graph(Some("2.1.0"), true),
        &[],
    );
    assert_eq!(out_of_range.state, ApplicabilityState::NotApplicable);
}

#[test]
fn repair_brief_is_bounded_to_usage_wrappers_and_tests() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::create_dir_all(root.path().join("tests")).unwrap();
    std::fs::write(root.path().join("src/client.ts"), "const password=\"source-secret\";\nexport function create_widget() {\n  return sdk.widgets.create({ key: \"sk_live_fixture_secret\" });\n}\n").unwrap();
    std::fs::write(
        root.path().join("tests/client.test.ts"),
        "test('create', () => create_widget());\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("src/unrelated.ts"),
        "export const unrelated = true;\n",
    )
    .unwrap();
    let data = graph(Some("1.5.0"), true);
    let assessment =
        evaluate_relevance(&event(), &registry(), &ApiInventory::default(), &data, &[]);
    let kg = KnowledgeGraph::from_graph_data(data);
    let event = event();
    let memory = [synaptic_api::MemoryEvidence {
        kind: "pitfall".into(),
        summary: "migration token=memory-secret".into(),
        source: "fixture".into(),
        digest: "memory-digest".into(),
    }];
    let brief = build_repair_brief(synaptic_api::RepairBriefRequest {
        repository_root: root.path(),
        repository_identity: "repo-acme",
        base_sha: "base123",
        event: &event,
        assessment: &assessment,
        graph: &kg,
        memory: &memory,
        budget: &BriefBudget {
            max_files: 5,
            max_source_bytes: 4_000,
            max_impact_nodes: 20,
            max_evidence_chars: 1_000,
        },
    })
    .unwrap();

    assert!(brief.allowed_files.contains(&"src/client.ts".into()));
    assert!(brief.allowed_files.contains(&"tests/client.test.ts".into()));
    assert!(!brief.allowed_files.contains(&"src/unrelated.ts".into()));
    assert_eq!(brief.required_tests, vec!["tests/client.test.ts"]);
    assert!(
        brief
            .source_slices
            .iter()
            .any(|slice| slice.file == "src/client.ts")
    );
    let serialized = serde_json::to_string(&brief).unwrap();
    assert!(!serialized.contains("source-secret"));
    assert!(!serialized.contains("sk_live_fixture_secret"));
    assert!(!serialized.contains("memory-secret"));
    assert!(serialized.contains("[REDACTED]"));
    assert!(serde_json::to_vec(&brief).unwrap().len() < 20_000);
    assert_eq!(brief.official_evidence, Vec::<EvidenceSpan>::new());
    assert_eq!(
        brief.event.source,
        SourceArtifact {
            uri: "https://acme.example/openapi.json".into(),
            revision: "v2".into(),
            etag: None,
            last_modified: None,
            content_digest: "abc".into(),
            fetched_at: 42,
            adapter_version: 1,
            evidence_kind: "openapi".into()
        }
    );
}
