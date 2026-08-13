use synaptic_api::{
    ApiChangeEvent, ApiUsageBinding, ApplicabilityState, BindingBasis, PatchPolicy,
    RelevanceAssessment, validate_patch, verify_api_invariants,
};
use synaptic_core::GraphData;
use synaptic_graph::KnowledgeGraph;

fn patch(path: &str, added: &str) -> String {
    format!(
        "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-old\n+{added}\n"
    )
}

#[test]
fn patch_policy_allows_bounded_graph_scope_and_rejects_security_escapes() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::create_dir_all(root.path().join(".github/workflows")).unwrap();
    std::fs::create_dir_all(root.path().join(".gitlab/ci")).unwrap();
    std::fs::write(root.path().join("src/client.ts"), "old\n").unwrap();
    std::fs::write(root.path().join(".github/workflows/ci.yml"), "old\n").unwrap();
    std::fs::write(root.path().join(".gitlab-ci.yml"), "old\n").unwrap();
    std::fs::write(root.path().join(".gitlab/ci/api.yml"), "old\n").unwrap();
    let policy = PatchPolicy {
        allowed_files: vec![
            "src/client.ts".into(),
            ".github/workflows/ci.yml".into(),
            ".gitlab-ci.yml".into(),
            ".gitlab/ci/api.yml".into(),
        ],
        max_files: 4,
        max_changed_lines: 20,
        ..PatchPolicy::default()
    };
    let inspection = validate_patch(root.path(), &patch("src/client.ts", "new"), &policy).unwrap();
    assert_eq!(inspection.changed_files, vec!["src/client.ts"]);

    for bad in [
        patch("../outside.ts", "new"),
        patch("src/unrelated.ts", "new"),
        patch(".github/workflows/ci.yml", "new"),
        patch(".gitlab-ci.yml", "new"),
        patch(".gitlab/ci/api.yml", "new"),
        patch("src/client.ts", "const key = 'sk_live_abcdefghijklmnop';"),
        format!("{}new mode 100755\n", patch("src/client.ts", "new")),
        format!("{}GIT binary patch\n", patch("src/client.ts", "new")),
    ] {
        assert!(
            validate_patch(root.path(), &bad, &policy).is_err(),
            "accepted {bad}"
        );
    }
}

fn event() -> ApiChangeEvent {
    serde_json::from_value(serde_json::json!({
        "version":1,"id":"event_1","vendor":"acme","occurred_at":1,
        "source":{"uri":"fixture","revision":"2","content_digest":"d","fetched_at":1,"adapter_version":1,"evidence_kind":"openapi"},
        "changes":[{
            "change_id":"c1","kind":"path_or_method_changed","affected_versions":{"requirement":"*"},
            "old_operation":{"id":"api_operation:acme:old","vendor":"acme","protocol":"https","method":"POST","canonical_path":"/v1/widgets"},
            "new_operation":{"id":"api_operation:acme:new","vendor":"acme","protocol":"https","method":"POST","canonical_path":"/v2/widgets"},
            "old_sdk_symbols":[],"new_sdk_symbols":[],"migration_summary":"move","evidence":[],"confidence":1.0
        }]
    })).unwrap()
}

fn graph(target: &str, include_wrapper: bool, unrelated: bool) -> KnowledgeGraph {
    let mut nodes = vec![
        serde_json::json!({"id":target,"label":target,"file_type":"concept","source_file":"","_node_type":"api_operation","vendor":"acme"}),
        serde_json::json!({"id":"fn:wrapper","label":"wrapper","file_type":"code","source_file":"src/client.ts","kind":"function","visibility":"public"}),
    ];
    let mut links = Vec::new();
    if include_wrapper {
        links.push(serde_json::json!({"source":"fn:wrapper","target":target,"relation":"uses_api","confidence":"EXTRACTED","source_file":"src/client.ts"}));
    }
    if unrelated {
        nodes.push(serde_json::json!({"id":"api_operation:acme:other","label":"other","file_type":"concept","source_file":"","_node_type":"api_operation","vendor":"acme"}));
        links.push(serde_json::json!({"source":"fn:wrapper","target":"api_operation:acme:other","relation":"uses_api","confidence":"EXTRACTED","source_file":"src/client.ts"}));
    }
    KnowledgeGraph::from_graph_data(
        serde_json::from_value::<GraphData>(
            serde_json::json!({"directed":true,"nodes":nodes,"links":links}),
        )
        .unwrap(),
    )
}

fn assessment() -> RelevanceAssessment {
    RelevanceAssessment {
        version: 1,
        event_id: "event_1".into(),
        vendor: "acme".into(),
        state: ApplicabilityState::Applicable,
        reasons: vec![],
        matched_change_ids: vec!["c1".into()],
        seed_node_ids: vec!["api_operation:acme:old".into()],
        observed_versions: vec!["1.0.0".into()],
        bindings: vec![ApiUsageBinding {
            vendor: "acme".into(),
            operation_node_id: "api_operation:acme:old".into(),
            caller_node_id: "fn:wrapper".into(),
            source_file: "src/client.ts".into(),
            source_location: None,
            sdk_package: None,
            sdk_member: None,
            sdk_version: Some("1.0.0".into()),
            api_version: None,
            basis: BindingBasis::AbsoluteUrlHost,
            confidence: 1.0,
        }],
    }
}

#[test]
fn graph_invariants_require_old_binding_removal_and_replacement_without_drift() {
    let before = graph("api_operation:acme:old", true, false);
    let good = graph("api_operation:acme:new", true, false);
    let report = verify_api_invariants(&before, &good, &event(), &assessment(), true);
    assert!(report.passed, "{report:?}");

    let hidden = graph("api_operation:acme:new", false, false);
    assert!(!verify_api_invariants(&before, &hidden, &event(), &assessment(), true).passed);
    let drift = graph("api_operation:acme:new", true, true);
    assert!(!verify_api_invariants(&before, &drift, &event(), &assessment(), true).passed);
    assert!(!verify_api_invariants(&before, &good, &event(), &assessment(), false).passed);
}

#[test]
fn each_caller_requires_only_the_replacement_for_its_matched_change() {
    let event: ApiChangeEvent = serde_json::from_value(serde_json::json!({
        "version":1,"id":"multi","vendor":"acme","occurred_at":1,
        "source":{"uri":"fixture","revision":"2","content_digest":"d","fetched_at":1,"adapter_version":1,"evidence_kind":"openapi"},
        "changes":[
            {"change_id":"one","kind":"path_or_method_changed","affected_versions":{"requirement":"*"},"old_operation":{"id":"api_operation:acme:old1","vendor":"acme","protocol":"https","method":"POST","canonical_path":"/old1"},"new_operation":{"id":"api_operation:acme:new1","vendor":"acme","protocol":"https","method":"POST","canonical_path":"/new1"},"old_sdk_symbols":[],"new_sdk_symbols":[],"migration_summary":"one","evidence":[],"confidence":1.0},
            {"change_id":"two","kind":"path_or_method_changed","affected_versions":{"requirement":"*"},"old_operation":{"id":"api_operation:acme:old2","vendor":"acme","protocol":"https","method":"POST","canonical_path":"/old2"},"new_operation":{"id":"api_operation:acme:new2","vendor":"acme","protocol":"https","method":"POST","canonical_path":"/new2"},"old_sdk_symbols":[],"new_sdk_symbols":[],"migration_summary":"two","evidence":[],"confidence":1.0}
        ]
    })).unwrap();
    let make_graph = |targets: [(&str, &str); 2]| {
        let operation_nodes = targets.iter().map(|(_, target)| serde_json::json!({"id":target,"label":target,"file_type":"concept","source_file":"","_node_type":"api_operation","vendor":"acme"})).collect::<Vec<_>>();
        let mut nodes = vec![
            serde_json::json!({"id":"caller:one","label":"one","file_type":"code","source_file":"src/one.ts","kind":"function"}),
            serde_json::json!({"id":"caller:two","label":"two","file_type":"code","source_file":"src/two.ts","kind":"function"}),
        ];
        nodes.extend(operation_nodes);
        let links = targets.iter().map(|(caller, target)| serde_json::json!({"source":caller,"target":target,"relation":"uses_api","confidence":"EXTRACTED","source_file":if *caller == "caller:one" {"src/one.ts"} else {"src/two.ts"}})).collect::<Vec<_>>();
        KnowledgeGraph::from_graph_data(
            serde_json::from_value::<GraphData>(
                serde_json::json!({"directed":true,"nodes":nodes,"links":links}),
            )
            .unwrap(),
        )
    };
    let before = make_graph([
        ("caller:one", "api_operation:acme:old1"),
        ("caller:two", "api_operation:acme:old2"),
    ]);
    let after = make_graph([
        ("caller:one", "api_operation:acme:new1"),
        ("caller:two", "api_operation:acme:new2"),
    ]);
    let binding = |caller: &str, operation: &str, file: &str| ApiUsageBinding {
        vendor: "acme".into(),
        operation_node_id: operation.into(),
        caller_node_id: caller.into(),
        source_file: file.into(),
        source_location: None,
        sdk_package: None,
        sdk_member: None,
        sdk_version: Some("1.0.0".into()),
        api_version: None,
        basis: BindingBasis::AbsoluteUrlHost,
        confidence: 1.0,
    };
    let assessment = RelevanceAssessment {
        version: 1,
        event_id: "multi".into(),
        vendor: "acme".into(),
        state: ApplicabilityState::Applicable,
        reasons: vec![],
        matched_change_ids: vec!["one".into(), "two".into()],
        bindings: vec![
            binding("caller:one", "api_operation:acme:old1", "src/one.ts"),
            binding("caller:two", "api_operation:acme:old2", "src/two.ts"),
        ],
        seed_node_ids: vec![
            "api_operation:acme:old1".into(),
            "api_operation:acme:old2".into(),
        ],
        observed_versions: vec!["1.0.0".into()],
    };
    let report = verify_api_invariants(&before, &after, &event, &assessment, true);
    assert!(report.passed, "{report:?}");
}
