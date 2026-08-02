use serde_json::{json, Map};
use synaptic_api::{
    analyze_api_coverage, analyze_api_coverage_with_evidence, attach_api_coverage,
    ApiMaintenanceConfig, CoverageGapKind, CoverageState, Dependency, DependencyScope, Ecosystem,
    ExternalServiceEvidence, ExternalSurfaceKind, PackageCoordinate, SbomCompleteness,
    SbomDocumentEvidence, SbomEvidenceReport, VendorRegistry,
};
use synaptic_core::{
    Confidence, DynamicKind, DynamicSite, Edge, FileType, GraphData, Node, NodeId,
};

fn node(id: &str, file: &str) -> Node {
    Node {
        id: NodeId(id.into()),
        label: id.into(),
        file_type: FileType::Code,
        source_file: file.into(),
        source_location: None,
        community: None,
        repo: None,
        extra: Map::new(),
    }
}

#[test]
fn sbom_services_are_observed_and_incomplete_sboms_prevent_false_complete_coverage() {
    let graph = GraphData::default();
    let service = ExternalServiceEvidence::new(
        "bom.cdx.json",
        "payments",
        vec!["https://payments.example.test/v1".into()],
        Some(true),
    );
    let sbom = SbomEvidenceReport {
        version: SbomEvidenceReport::VERSION,
        documents: vec![SbomDocumentEvidence {
            source_file: "bom.cdx.json".into(),
            format: "cyclonedx".into(),
            spec_version: Some("1.6".into()),
            completeness: SbomCompleteness::Incomplete,
            component_count: 0,
            service_count: 1,
        }],
        services: vec![service],
    };

    let report = analyze_api_coverage_with_evidence(&graph, &[], None, &[], &sbom);
    assert!(!report.complete);
    assert!(!report.evidence_complete);
    assert!(report.observations.iter().any(|observation| {
        observation.kind == ExternalSurfaceKind::Service
            && observation.provider.as_deref() == Some("payments")
    }));
    assert!(report.evidence_windows.iter().any(|window| {
        window.evidence_kind == "sbom_inventory"
            && window.origin == "bom.cdx.json"
            && !window.complete
    }));
}

fn edge(source: &str, relation: &str, file: &str, extra: Map<String, serde_json::Value>) -> Edge {
    Edge {
        source: NodeId(source.into()),
        target: NodeId(format!("target:{source}:{relation}")),
        relation: relation.into(),
        confidence: Confidence::Extracted,
        source_file: file.into(),
        source_location: Some("12:3".into()),
        confidence_score: Some(0.95),
        weight: 1.0,
        context: None,
        cross_repo: false,
        extra,
    }
}

fn registry() -> VendorRegistry {
    VendorRegistry::new(
        ApiMaintenanceConfig::parse(
            r#"
schema = 1

[[vendors]]
id = "acme"
packages = ["npm:acme-sdk"]
hosts = ["api.acme.test"]

[[vendors.sdk_bindings]]
package = "npm:acme-sdk"
imports = ["acme-sdk"]
member = "widgets.create"
method = "POST"
path = "/v1/widgets"

[[vendors.sources]]
kind = "static_contract"
path = "contracts/acme-openapi.json"
affected_versions = "*"
"#,
        )
        .unwrap(),
    )
    .unwrap()
}

fn dependency() -> Dependency {
    Dependency {
        package: PackageCoordinate::new(Ecosystem::Npm, "acme-sdk"),
        declared_requirement: Some("^4.0.0".into()),
        resolved_version: Some("4.2.1".into()),
        scope: DependencyScope::Runtime,
        source_file: "package-lock.json".into(),
        purl: None,
    }
}

#[test]
fn unconfigured_http_and_sdk_calls_are_explicit_observed_gaps() {
    let mut http = Map::new();
    http.insert("http_method".into(), json!("POST"));
    http.insert("http_scheme".into(), json!("https"));
    http.insert("http_authority".into(), json!("api.unknown.test"));
    http.insert("http_path".into(), json!("/v2/items"));

    let mut sdk = Map::new();
    sdk.insert("sdk_ecosystem".into(), json!("npm"));
    sdk.insert("sdk_import".into(), json!("unknown-sdk"));
    sdk.insert("sdk_package".into(), json!("npm:unknown-sdk"));
    sdk.insert("sdk_member_chain".into(), json!("items.create"));

    let graph = GraphData {
        nodes: vec![node("http", "src/http.ts"), node("sdk", "src/sdk.ts")],
        links: vec![
            edge("http", "calls_service", "src/http.ts", http),
            edge("sdk", "calls_sdk", "src/sdk.ts", sdk),
        ],
        ..GraphData::default()
    };

    let report = analyze_api_coverage(&graph, &[], None);

    assert_eq!(report.raw_evidence, 2);
    assert_eq!(report.observations.len(), 2);
    assert!(!report.complete);
    assert!(report
        .observations
        .iter()
        .all(|observation| observation.state == CoverageState::Observed));
    assert!(report.observations.iter().any(|observation| {
        observation.kind == ExternalSurfaceKind::Http
            && observation.authority.as_deref() == Some("api.unknown.test")
            && observation
                .gaps
                .contains(&CoverageGapKind::ProviderIdentity)
            && observation.gaps.contains(&CoverageGapKind::ContractModel)
            && observation.gaps.contains(&CoverageGapKind::ChangeSource)
            && observation
                .gaps
                .contains(&CoverageGapKind::OperationBinding)
    }));
    assert!(report.observations.iter().any(|observation| {
        observation.kind == ExternalSurfaceKind::Sdk
            && observation.package.as_deref() == Some("npm:unknown-sdk")
            && observation.member.as_deref() == Some("items.create")
    }));
}

#[test]
fn configured_but_unbound_call_is_identified_not_silently_supported() {
    let mut sdk = Map::new();
    sdk.insert("sdk_ecosystem".into(), json!("npm"));
    sdk.insert("sdk_import".into(), json!("acme-sdk"));
    sdk.insert("sdk_package".into(), json!("npm:acme-sdk"));
    sdk.insert("sdk_member_chain".into(), json!("widgets.delete"));
    let graph = GraphData {
        nodes: vec![node("sdk", "src/sdk.ts")],
        links: vec![edge("sdk", "calls_sdk", "src/sdk.ts", sdk)],
        ..GraphData::default()
    };
    let registry = registry();

    let report = analyze_api_coverage(&graph, &[dependency()], Some(&registry));
    let observation = &report.observations[0];

    assert_eq!(observation.state, CoverageState::Identified);
    assert_eq!(observation.provider.as_deref(), Some("acme"));
    assert_eq!(observation.resolved_version.as_deref(), Some("4.2.1"));
    assert!(!observation
        .gaps
        .contains(&CoverageGapKind::ProviderIdentity));
    assert!(!observation.gaps.contains(&CoverageGapKind::ContractModel));
    assert!(!observation.gaps.contains(&CoverageGapKind::ChangeSource));
    assert!(observation
        .gaps
        .contains(&CoverageGapKind::OperationBinding));
    assert!(!report.complete);
}

#[test]
fn exact_existing_binding_reaches_bound_and_is_complete_for_coverage() {
    let mut sdk = Map::new();
    sdk.insert("sdk_ecosystem".into(), json!("npm"));
    sdk.insert("sdk_import".into(), json!("acme-sdk"));
    sdk.insert("sdk_package".into(), json!("npm:acme-sdk"));
    sdk.insert("sdk_member_chain".into(), json!("widgets.create"));
    let raw = edge("sdk", "calls_sdk", "src/sdk.ts", sdk);

    let mut bound_extra = raw.extra.clone();
    bound_extra.insert("api_vendor".into(), json!("acme"));
    bound_extra.insert(
        "operation_id".into(),
        json!("api_operation:acme:https:post:v1_widgets"),
    );
    bound_extra.insert("binding_basis".into(), json!("sdk_symbol"));
    bound_extra.insert("installed_sdk_version".into(), json!("4.2.1"));
    let mut bound = edge("sdk", "uses_api", "src/sdk.ts", bound_extra);
    bound.target = NodeId("api_operation:acme:https:post:v1_widgets".into());

    let graph = GraphData {
        nodes: vec![node("sdk", "src/sdk.ts")],
        links: vec![raw, bound],
        ..GraphData::default()
    };
    let registry = registry();

    let report = analyze_api_coverage(&graph, &[dependency()], Some(&registry));
    let observation = &report.observations[0];

    assert_eq!(observation.state, CoverageState::Bound);
    assert_eq!(
        observation.operation_id.as_deref(),
        Some("api_operation:acme:https:post:v1_widgets")
    );
    assert!(observation.gaps.is_empty(), "{:?}", observation.gaps);
    assert!(report.complete);
    assert_eq!(report.counts.get(&CoverageState::Bound), Some(&1));
}

#[test]
fn dynamic_dispatch_is_a_first_class_unresolved_observation() {
    let mut caller = node("dynamic", "src/plugin.py");
    caller.push_dynamic_site(DynamicSite {
        kind: DynamicKind::Reflection,
        line: 7,
        key: None,
        snippet: "getattr(client, operation)(payload)".into(),
    });
    let graph = GraphData {
        nodes: vec![caller],
        ..GraphData::default()
    };

    let first = analyze_api_coverage(&graph, &[], None);
    let second = analyze_api_coverage(&graph, &[], None);
    let observation = &first.observations[0];

    assert_eq!(observation.kind, ExternalSurfaceKind::DynamicDispatch);
    assert_eq!(observation.state, CoverageState::Observed);
    assert!(observation.gaps.contains(&CoverageGapKind::DynamicTarget));
    assert_eq!(
        first, second,
        "coverage IDs and order must be deterministic"
    );
    assert!(!first.complete);
}

#[test]
fn dependency_without_call_evidence_is_an_explicit_classification_gap() {
    let graph = GraphData::default();

    let report = analyze_api_coverage(&graph, &[dependency()], None);
    let observation = &report.observations[0];

    assert_eq!(report.raw_evidence, 1);
    assert_eq!(observation.kind, ExternalSurfaceKind::PackageDependency);
    assert_eq!(observation.package.as_deref(), Some("npm:acme-sdk"));
    assert_eq!(observation.resolved_version.as_deref(), Some("4.2.1"));
    assert_eq!(observation.state, CoverageState::Observed);
    assert_eq!(observation.gaps, vec![CoverageGapKind::UsageClassification]);
    assert!(!report.complete);
}

#[test]
fn development_dependency_is_an_explicit_inventory_negative_control() {
    let mut development = dependency();
    development.scope = DependencyScope::Development;

    let report = analyze_api_coverage(&GraphData::default(), &[development.clone()], None);

    assert!(report.complete);
    assert!(report.observations.is_empty());
    assert_eq!(report.development_dependencies, vec![development]);
}

#[test]
fn non_http_external_boundaries_are_observed_but_internal_ones_are_negative_controls() {
    let mut caller = node("caller", "src/client.rs");
    caller.source_location = Some("4:1".into());
    let mut grpc = node("grpc:greeter", "");
    grpc.extra
        .insert("_node_type".into(), json!("grpc_service"));
    let mut queue = node("queue:orders", "");
    queue
        .extra
        .insert("_node_type".into(), json!("queue_topic"));
    let handler = node("handler", "src/worker.rs");

    let mut grpc_call = edge("caller", "calls_service", "src/client.rs", Map::new());
    grpc_call.target = grpc.id.clone();
    grpc_call.context = Some("gRPC".into());
    let mut queue_call = edge("caller", "calls_service", "src/client.rs", Map::new());
    queue_call.target = queue.id.clone();
    queue_call.context = Some("kafka".into());
    let mut handled = edge("queue:orders", "handled_by", "src/worker.rs", Map::new());
    handled.target = handler.id.clone();

    let graph = GraphData {
        nodes: vec![caller, grpc, queue, handler],
        links: vec![grpc_call, queue_call, handled],
        ..GraphData::default()
    };
    let report = analyze_api_coverage(&graph, &[], None);

    assert_eq!(report.observations.len(), 1);
    assert_eq!(report.observations[0].kind, ExternalSurfaceKind::Rpc);
    assert_eq!(report.observations[0].protocol.as_deref(), Some("grpc"));
    assert_eq!(report.observations[0].identity, "grpc:greeter");
}

#[test]
fn unresolved_external_commands_and_native_bindings_are_observed() {
    let caller = node("caller", "src/runner.py");
    let mut command = node("command:vendorctl", "");
    command.extra.insert("_node_type".into(), json!("command"));
    let mut native = node("native:vendor", "");
    native
        .extra
        .insert("_node_type".into(), json!("native_symbol"));

    let mut invokes = edge("caller", "invokes", "src/runner.py", Map::new());
    invokes.target = command.id.clone();
    let mut binds = edge("caller", "binds_native", "src/runner.py", Map::new());
    binds.target = native.id.clone();
    let graph = GraphData {
        nodes: vec![caller, command, native],
        links: vec![invokes, binds],
        ..GraphData::default()
    };

    let report = analyze_api_coverage(&graph, &[], None);
    let kinds = report
        .observations
        .iter()
        .map(|observation| observation.kind)
        .collect::<std::collections::BTreeSet<_>>();

    assert_eq!(
        kinds,
        [ExternalSurfaceKind::Command, ExternalSurfaceKind::Native]
            .into_iter()
            .collect()
    );
}

#[test]
fn exact_digest_bound_waiver_resolves_policy_but_preserves_the_gap_evidence() {
    let mut http = Map::new();
    http.insert("http_method".into(), json!("GET"));
    http.insert("http_scheme".into(), json!("https"));
    http.insert("http_authority".into(), json!("api.legacy.test"));
    http.insert("http_path".into(), json!("/v1/status"));
    let graph = GraphData {
        nodes: vec![node("caller", "src/client.ts")],
        links: vec![edge("caller", "calls_service", "src/client.ts", http)],
        ..GraphData::default()
    };
    let initial = analyze_api_coverage(&graph, &[], None);
    let observed = &initial.observations[0];
    let config = format!(
        r#"
schema = 1

[[coverage.waivers]]
observation_id = "{}"
evidence_digest = "{}"
reason = "legacy endpoint is monitored by the owning infrastructure team"
"#,
        observed.id, observed.evidence_digest
    );
    let registry = VendorRegistry::new(ApiMaintenanceConfig::parse(&config).unwrap()).unwrap();

    let waived = analyze_api_coverage(&graph, &[], Some(&registry));

    assert!(waived.complete);
    assert!(waived.gaps.is_empty());
    assert_eq!(waived.waivers_applied, 1);
    assert!(waived.observations[0].waived);
    assert!(!waived.observations[0].gaps.is_empty());
    assert_eq!(
        waived.observations[0].waiver_reason.as_deref(),
        Some("legacy endpoint is monitored by the owning infrastructure team")
    );
}

#[test]
fn changed_evidence_invalidates_a_coverage_waiver() {
    let config = format!(
        r#"
schema = 1
[[coverage.waivers]]
observation_id = "external_surface_deadbeef"
evidence_digest = "{}"
reason = "temporary review"
"#,
        "a".repeat(64)
    );
    let registry = VendorRegistry::new(ApiMaintenanceConfig::parse(&config).unwrap()).unwrap();
    let mut http = Map::new();
    http.insert("http_method".into(), json!("GET"));
    http.insert("http_scheme".into(), json!("https"));
    http.insert("http_authority".into(), json!("api.changed.test"));
    http.insert("http_path".into(), json!("/"));
    let graph = GraphData {
        nodes: vec![node("caller", "src/client.ts")],
        links: vec![edge("caller", "calls_service", "src/client.ts", http)],
        ..GraphData::default()
    };

    let report = analyze_api_coverage(&graph, &[], Some(&registry));

    assert!(!report.complete);
    assert_eq!(report.waivers_applied, 0);
    assert!(!report.observations[0].waived);
}

#[test]
fn coverage_overlay_is_idempotent_and_never_creates_impact_edges() {
    let mut http = Map::new();
    http.insert("http_method".into(), json!("GET"));
    http.insert("http_scheme".into(), json!("https"));
    http.insert("http_authority".into(), json!("api.unknown.test"));
    http.insert("http_path".into(), json!("/v1/items"));
    let mut nodes = vec![node("caller", "src/client.ts")];
    let mut edges = vec![edge("caller", "calls_service", "src/client.ts", http)];

    let first = attach_api_coverage(&mut nodes, &mut edges, &[], None);
    let first_nodes = nodes.clone();
    let first_edges = edges.clone();
    let second = attach_api_coverage(&mut nodes, &mut edges, &[], None);

    assert_eq!(first, second);
    assert_eq!(nodes, first_nodes);
    assert_eq!(edges, first_edges);
    let observation = nodes
        .iter()
        .find(|node| {
            node.extra
                .get("_node_type")
                .and_then(|value| value.as_str())
                == Some("external_surface_observation")
        })
        .expect("coverage observation node");
    assert_eq!(observation.extra["coverage_state"], "observed");
    assert!(edges.iter().any(|edge| {
        edge.source == NodeId("caller".into())
            && edge.target == observation.id
            && edge.relation == "observes_external"
    }));
    assert!(edges.iter().all(|edge| edge.relation != "uses_api"));
    for node in nodes
        .iter()
        .filter(|node| node.extra.get("_node_type") == Some(&json!("external_surface_observation")))
    {
        assert!(!node.extra.contains_key("id"));
        assert!(!node.extra.contains_key("source_file"));
        assert!(!node.extra.contains_key("source_location"));
    }
    let encoded = serde_json::to_vec(&GraphData {
        nodes: nodes.clone(),
        links: edges.clone(),
        ..GraphData::default()
    })
    .unwrap();
    serde_json::from_slice::<GraphData>(&encoded).expect("coverage overlay round-trips strictly");
}
