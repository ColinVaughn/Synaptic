use std::fs;

use serde_json::{Map, json};
use synaptic_api::{
    API_OPERATION_NODE_TYPE, API_VENDOR_NODE_TYPE, ApiMaintenanceConfig, Dependency,
    DependencyScope, Ecosystem, PackageCoordinate, VendorRegistry, bind_direct_http_usages,
    bind_repository_api_usages, bind_sdk_dependencies, bind_sdk_usages,
};
use synaptic_core::{Confidence, Edge, FileType, Node, NodeId, make_id};

fn registry(source: &str) -> VendorRegistry {
    VendorRegistry::new(ApiMaintenanceConfig::parse(source).unwrap()).unwrap()
}

fn node(id: &str, label: &str, node_type: Option<&str>) -> Node {
    let mut extra = Map::new();
    if let Some(node_type) = node_type {
        extra.insert("_node_type".into(), json!(node_type));
    }
    Node {
        id: NodeId(id.into()),
        label: label.into(),
        file_type: FileType::Code,
        source_file: if node_type == Some("route") {
            String::new().into()
        } else {
            "src/client.ts".into()
        },
        source_location: None,
        community: None,
        repo: None,
        extra,
        ..Default::default()
    }
}

fn http_edge(
    source: &str,
    route: &str,
    method: &str,
    scheme: &str,
    host: &str,
    path: &str,
) -> Edge {
    let mut extra = Map::new();
    extra.insert("http_method".into(), json!(method));
    extra.insert("http_scheme".into(), json!(scheme));
    extra.insert("http_authority".into(), json!(host));
    extra.insert("http_path".into(), json!(path));
    Edge {
        source: NodeId(source.into()),
        target: NodeId(route.into()),
        relation: "calls_service".into(),
        confidence: Confidence::Inferred,
        source_file: "src/client.ts".into(),
        source_location: Some("L7".into()),
        confidence_score: Some(0.5),
        weight: 1.0,
        context: Some(format!("{method} {host}")),
        cross_repo: false,
        extra,
    }
}

#[test]
fn direct_http_bindings_are_vendor_scoped_and_preserve_route_edges() {
    let registry = registry(
        r#"
schema = 1
[[vendors]]
id = "stripe"
hosts = ["api.stripe.com"]
[[vendors]]
id = "other_pay"
hosts = ["api.other.test"]
"#,
    );
    let mut nodes = vec![
        node("stripe_caller", "createStripeCharge()", None),
        node("other_caller", "createOtherCharge()", None),
        node("route_charge", "/v1/charges", Some("route")),
    ];
    let mut edges = vec![
        http_edge(
            "stripe_caller",
            "route_charge",
            "POST",
            "https",
            "api.stripe.com",
            "/v1/charges",
        ),
        http_edge(
            "other_caller",
            "route_charge",
            "POST",
            "https",
            "api.other.test",
            "/v1/charges",
        ),
    ];

    let report = bind_direct_http_usages(&mut nodes, &mut edges, &registry);

    assert_eq!(report.vendors, 2);
    assert_eq!(report.operations, 2);
    assert_eq!(report.usages, 2);
    assert_eq!(
        edges
            .iter()
            .filter(|edge| edge.relation == "calls_service")
            .count(),
        2,
        "the compatibility route edges remain in the graph"
    );
    let vendor_nodes: Vec<_> = nodes
        .iter()
        .filter(|node| node.extra.get("_node_type") == Some(&json!(API_VENDOR_NODE_TYPE)))
        .collect();
    assert_eq!(vendor_nodes.len(), 2);
    let operation_nodes: Vec<_> = nodes
        .iter()
        .filter(|node| node.extra.get("_node_type") == Some(&json!(API_OPERATION_NODE_TYPE)))
        .collect();
    assert_eq!(operation_nodes.len(), 2);
    assert_ne!(operation_nodes[0].id, operation_nodes[1].id);
    assert!(
        operation_nodes
            .iter()
            .all(|node| node.extra["canonical_path"] == "/v1/charges")
    );
    assert_eq!(
        edges
            .iter()
            .filter(|edge| edge.relation == "uses_api")
            .count(),
        2
    );
    assert_eq!(
        edges
            .iter()
            .filter(|edge| edge.relation == "provided_by")
            .count(),
        2
    );
}

#[test]
fn generated_client_metadata_binds_without_a_manual_member_rule() {
    let registry = registry(
        r#"
schema = 1
[[vendors]]
id = "acme"
packages = ["npm:@acme/generated-client"]
"#,
    );
    let mut call = http_edge("caller", "candidate", "CALL", "sdk", "acme", "/");
    call.relation = "calls_sdk".into();
    call.extra.clear();
    call.extra
        .insert("sdk_package".into(), json!("npm:@acme/generated-client"));
    call.extra
        .insert("sdk_member_chain".into(), json!("WidgetsApi.createWidget"));
    call.extra.insert(
        "generated_api".into(),
        json!({
            "vendor": "acme",
            "protocol": "https",
            "method": "POST",
            "path": "/v1/widgets",
            "operation_id": synaptic_api::ApiOperationAnchor::new(
                "acme", "https", "POST", "/v1/widgets"
            ).id
        }),
    );
    let mut nodes = vec![node("caller", "createWidget()", None)];
    let mut edges = vec![call];

    let report = bind_sdk_usages(&mut nodes, &mut edges, &registry);

    assert_eq!(report.usages, 1);
    let usage = edges
        .iter()
        .find(|edge| edge.relation == "uses_api")
        .expect("exact generated metadata binds");
    assert_eq!(usage.extra["binding_basis"], "generated_client");
    assert_eq!(usage.extra["sdk_package"], "npm:@acme/generated-client");
    assert_eq!(usage.extra["sdk_member_chain"], "WidgetsApi.createWidget");
}

#[test]
fn ambiguous_or_missing_authority_fails_closed_and_rebinding_is_idempotent() {
    let registry = registry(
        r#"
schema = 1
[[vendors]]
id = "one"
hosts = ["shared.test"]
[[vendors]]
id = "two"
hosts = ["shared.test"]
"#,
    );
    let mut nodes = vec![
        node("caller", "call()", None),
        node("route", "/v1/data", Some("route")),
    ];
    let ambiguous = http_edge("caller", "route", "GET", "https", "shared.test", "/v1/data");
    let mut relative = ambiguous.clone();
    relative.extra.remove("http_authority");
    relative.extra.remove("http_scheme");
    relative.context = Some("GET".into());
    let mut edges = vec![ambiguous, relative];

    let first = bind_direct_http_usages(&mut nodes, &mut edges, &registry);
    let second = bind_direct_http_usages(&mut nodes, &mut edges, &registry);

    assert_eq!(first.usages, 0);
    assert_eq!(first.ambiguous, 1);
    assert_eq!(second, first);
    assert!(edges.iter().all(|edge| edge.relation != "uses_api"));
    assert!(nodes.iter().all(|node| {
        !matches!(
            node.extra
                .get("_node_type")
                .and_then(|value| value.as_str()),
            Some(API_VENDOR_NODE_TYPE | API_OPERATION_NODE_TYPE)
        )
    }));
}

#[test]
fn matched_sdk_inventory_reuses_package_nodes_and_emits_sdk_for() {
    let registry = registry(
        r#"
schema = 1
[[vendors]]
id = "stripe"
packages = ["npm:stripe"]
"#,
    );
    let package_id = NodeId(make_id(&["npm", "stripe"]));
    let mut package = node(&package_id.0, "stripe", None);
    package.source_file = Default::default();
    let mut nodes = vec![package];
    let mut edges = Vec::new();
    let mut dependency = Dependency::new(
        PackageCoordinate::new(Ecosystem::Npm, "stripe"),
        "package.json",
        DependencyScope::Runtime,
    );
    dependency.declared_requirement = Some("^18".into());
    dependency.resolved_version = Some("18.3.0".into());

    let report = bind_sdk_dependencies(&mut nodes, &mut edges, &registry, &[dependency]);

    assert_eq!(report.sdk_packages, 1);
    assert_eq!(nodes.iter().filter(|node| node.id == package_id).count(), 1);
    let package = nodes.iter().find(|node| node.id == package_id).unwrap();
    assert_eq!(package.extra["api_vendor"], "stripe");
    assert_eq!(package.extra["ecosystem"], "npm");
    assert_eq!(package.extra["resolved_version"], "18.3.0");
    let edge = edges
        .iter()
        .find(|edge| edge.relation == "sdk_for")
        .expect("package-to-vendor inventory edge");
    assert_eq!(edge.source, package_id);
    assert_eq!(edge.target.0, "api_vendor:stripe");
    assert!(nodes.iter().any(|node| node.id == edge.target));
}

#[test]
fn sdk_member_rule_binds_call_to_operation_with_installed_version() {
    let registry = registry(
        r#"
schema = 1
[[vendors]]
id = "stripe"
packages = ["npm:stripe"]
hosts = ["api.stripe.com"]
[[vendors.sdk_bindings]]
package = "npm:stripe"
member = "customers.create"
method = "POST"
path = "/v1/customers"
"#,
    );
    let mut candidate = node(
        "sdk_candidate",
        "npm:stripe#customers.create",
        Some("sdk_call_candidate"),
    );
    candidate.source_file = Default::default();
    candidate
        .extra
        .insert("sdk_package".into(), json!("npm:stripe"));
    candidate
        .extra
        .insert("sdk_member_chain".into(), json!("customers.create"));
    let mut call = http_edge("caller", "sdk_candidate", "CALL", "sdk", "stripe", "/");
    call.relation = "calls_sdk".into();
    call.context = Some("npm:stripe customers.create".into());
    call.extra.clear();
    call.extra.insert("sdk_package".into(), json!("npm:stripe"));
    call.extra
        .insert("sdk_member_chain".into(), json!("customers.create"));
    let mut nodes = vec![node("caller", "createCustomer()", None), candidate];
    let mut edges = vec![call];
    let mut dependency = Dependency::new(
        PackageCoordinate::new(Ecosystem::Npm, "stripe"),
        "package.json",
        DependencyScope::Runtime,
    );
    dependency.resolved_version = Some("18.3.0".into());
    bind_sdk_dependencies(&mut nodes, &mut edges, &registry, &[dependency]);

    let report = bind_sdk_usages(&mut nodes, &mut edges, &registry);

    assert_eq!(report.usages, 1);
    assert_eq!(report.operations, 1);
    let operation = nodes
        .iter()
        .find(|node| node.extra.get("_node_type") == Some(&json!(API_OPERATION_NODE_TYPE)))
        .unwrap();
    assert_eq!(operation.extra["method"], "POST");
    assert_eq!(operation.extra["canonical_path"], "/v1/customers");
    let usage = edges
        .iter()
        .find(|edge| edge.relation == "uses_api")
        .unwrap();
    assert_eq!(usage.source.0, "caller");
    assert_eq!(usage.target, operation.id);
    assert_eq!(usage.extra["binding_basis"], "sdk_symbol");
    assert_eq!(usage.extra["sdk_member_chain"], "customers.create");
    assert_eq!(usage.extra["installed_sdk_version"], "18.3.0");
    assert!(usage.extra["evidence_digest"].as_str().unwrap().len() >= 32);
}

#[test]
fn go_subpackage_sdk_call_inherits_its_module_version() {
    let registry = registry(
        r#"
schema = 1
[[vendors]]
id = "github"
packages = ["go:github.com/google/go-github/v89"]
[[vendors.sdk_bindings]]
package = "go:github.com/google/go-github/v89/github"
member = "Repositories.Get"
method = "GET"
path = "/repos/{owner}/{repo}"
"#,
    );
    let sdk_package = "go:github.com/google/go-github/v89/github";
    let mut candidate = node(
        "sdk_candidate",
        "go-github#Repositories.Get",
        Some("sdk_call_candidate"),
    );
    candidate.source_file = Default::default();
    candidate
        .extra
        .insert("sdk_package".into(), json!(sdk_package));
    candidate
        .extra
        .insert("sdk_member_chain".into(), json!("Repositories.Get"));
    let mut call = http_edge("caller", "sdk_candidate", "CALL", "sdk", "github", "/");
    call.relation = "calls_sdk".into();
    call.extra.clear();
    call.extra.insert("sdk_package".into(), json!(sdk_package));
    call.extra
        .insert("sdk_member_chain".into(), json!("Repositories.Get"));
    let mut nodes = vec![node("caller", "getRepository()", None), candidate];
    let mut edges = vec![call];
    let mut dependency = Dependency::new(
        PackageCoordinate::new(Ecosystem::Go, "github.com/google/go-github/v89"),
        "go.mod",
        DependencyScope::Runtime,
    );
    dependency.resolved_version = Some("v89.0.1".into());
    bind_sdk_dependencies(&mut nodes, &mut edges, &registry, &[dependency]);

    let report = bind_sdk_usages(&mut nodes, &mut edges, &registry);

    assert_eq!(report.usages, 1);
    let usage = edges
        .iter()
        .find(|edge| edge.relation == "uses_api")
        .unwrap();
    assert_eq!(usage.extra["installed_sdk_version"], "v89.0.1");
}

#[test]
fn import_namespace_rule_binds_to_the_real_dependency_coordinate_and_version() {
    let registry = registry(
        r#"
schema = 1
[[vendors]]
id = "stripe"
packages = ["maven:com.stripe:stripe-java"]
[[vendors.sdk_bindings]]
package = "maven:com.stripe:stripe-java"
imports = ["com.stripe", "com.stripe.net"]
member = "StripeClient.customers.create"
method = "POST"
path = "/v1/customers"
"#,
    );
    let mut candidate = node(
        "sdk_candidate",
        "com.stripe#StripeClient.customers.create",
        Some("sdk_call_candidate"),
    );
    candidate.source_file = Default::default();
    candidate
        .extra
        .insert("sdk_ecosystem".into(), json!("maven"));
    candidate
        .extra
        .insert("sdk_import".into(), json!("com.stripe.net"));
    candidate.extra.insert(
        "sdk_member_chain".into(),
        json!("StripeClient.customers.create"),
    );
    let mut call = http_edge("caller", "sdk_candidate", "CALL", "sdk", "stripe", "/");
    call.relation = "calls_sdk".into();
    call.extra.clear();
    call.extra.insert("sdk_ecosystem".into(), json!("maven"));
    call.extra
        .insert("sdk_import".into(), json!("com.stripe.net"));
    call.extra.insert(
        "sdk_member_chain".into(),
        json!("StripeClient.customers.create"),
    );
    let mut nodes = vec![node("caller", "createCustomer()", None), candidate];
    let mut edges = vec![call];
    let mut dependency = Dependency::new(
        PackageCoordinate::new(Ecosystem::Maven, "com.stripe:stripe-java"),
        "pom.xml",
        DependencyScope::Runtime,
    );
    dependency.resolved_version = Some("29.3.0".into());
    bind_sdk_dependencies(&mut nodes, &mut edges, &registry, &[dependency]);

    let report = bind_sdk_usages(&mut nodes, &mut edges, &registry);

    assert_eq!(report.usages, 1);
    assert_eq!(report.ambiguous, 0);
    let usage = edges
        .iter()
        .find(|edge| edge.relation == "uses_api")
        .unwrap();
    assert_eq!(usage.extra["sdk_package"], "maven:com.stripe:stripe-java");
    assert_eq!(usage.extra["sdk_import"], "com.stripe.net");
    assert_eq!(usage.extra["installed_sdk_version"], "29.3.0");
}

#[test]
fn ambiguous_import_namespace_rules_fail_closed() {
    let registry = registry(
        r#"
schema = 1
[[vendors]]
id = "one"
[[vendors.sdk_bindings]]
package = "maven:one:sdk"
imports = ["shared.sdk"]
member = "Client.create"
method = "POST"
path = "/one"
[[vendors]]
id = "two"
[[vendors.sdk_bindings]]
package = "maven:two:sdk"
imports = ["shared.sdk"]
member = "Client.create"
method = "POST"
path = "/two"
"#,
    );
    let mut call = http_edge("caller", "sdk_candidate", "CALL", "sdk", "shared", "/");
    call.relation = "calls_sdk".into();
    call.extra.clear();
    call.extra.insert("sdk_ecosystem".into(), json!("maven"));
    call.extra.insert("sdk_import".into(), json!("shared.sdk"));
    call.extra
        .insert("sdk_member_chain".into(), json!("Client.create"));
    let mut nodes = vec![node("caller", "create()", None)];
    let mut edges = vec![call];

    let report = bind_sdk_usages(&mut nodes, &mut edges, &registry);

    assert_eq!(report.usages, 0);
    assert_eq!(report.ambiguous, 1);
    assert!(edges.iter().all(|edge| edge.relation != "uses_api"));
}

#[test]
fn native_source_namespace_can_bind_to_a_vcpkg_dependency() {
    let registry = registry(
        r#"
schema = 1
[[vendors]]
id = "stripe"
packages = ["vcpkg:stripe"]
[[vendors.sdk_bindings]]
package = "vcpkg:stripe"
imports = ["stripe"]
member = "Client.create"
method = "POST"
path = "/v1/customers"
"#,
    );
    let mut call = http_edge("caller", "sdk_candidate", "CALL", "sdk", "stripe", "/");
    call.relation = "calls_sdk".into();
    call.extra.clear();
    call.extra.insert("sdk_ecosystem".into(), json!("conan"));
    call.extra.insert("sdk_import".into(), json!("stripe"));
    call.extra
        .insert("sdk_member_chain".into(), json!("Client.create"));
    let mut nodes = vec![node("caller", "create()", None)];
    let mut edges = vec![call];
    let mut dependency = Dependency::new(
        PackageCoordinate::new(Ecosystem::Vcpkg, "stripe"),
        "vcpkg.json",
        DependencyScope::Runtime,
    );
    dependency.resolved_version = Some("1.2.3".into());
    bind_sdk_dependencies(&mut nodes, &mut edges, &registry, &[dependency]);

    let report = bind_sdk_usages(&mut nodes, &mut edges, &registry);

    assert_eq!(report.usages, 1);
    let usage = edges
        .iter()
        .find(|edge| edge.relation == "uses_api")
        .unwrap();
    assert_eq!(usage.extra["sdk_package"], "vcpkg:stripe");
    assert_eq!(usage.extra["installed_sdk_version"], "1.2.3");
}

#[test]
fn same_named_packages_in_different_ecosystems_keep_distinct_versions() {
    let registry = registry(
        r#"
schema = 1
[[vendors]]
id = "ruby"
packages = ["gem:stripe"]
[[vendors]]
id = "julia"
packages = ["julia:stripe"]
"#,
    );
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut gem = Dependency::new(
        PackageCoordinate::new(Ecosystem::Gem, "stripe"),
        "Gemfile",
        DependencyScope::Runtime,
    );
    gem.resolved_version = Some("13.1.0".into());
    let mut julia = Dependency::new(
        PackageCoordinate::new(Ecosystem::Julia, "stripe"),
        "Project.toml",
        DependencyScope::Runtime,
    );
    julia.resolved_version = Some("0.4.0".into());

    let report = bind_sdk_dependencies(&mut nodes, &mut edges, &registry, &[gem, julia]);

    assert_eq!(report.sdk_packages, 2);
    let packages = nodes
        .iter()
        .filter_map(|node| {
            Some((
                node.extra.get("package")?.as_str()?,
                node.extra.get("resolved_version")?.as_str()?,
            ))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(packages.get("gem:stripe"), Some(&"13.1.0"));
    assert_eq!(packages.get("julia:stripe"), Some(&"0.4.0"));
}

#[test]
fn repository_binding_uses_the_nearest_manifest_version_for_each_source() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("apps/nested")).unwrap();
    fs::write(repo.path().join("Gemfile"), "gem 'stripe', '~> 13'\n").unwrap();
    fs::write(
        repo.path().join("Gemfile.lock"),
        "GEM\n  specs:\n    stripe (13.1.0)\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("apps/nested/Gemfile"),
        "gem 'stripe', '~> 18'\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("apps/nested/Gemfile.lock"),
        "GEM\n  specs:\n    stripe (18.2.0)\n",
    )
    .unwrap();
    let registry = registry(
        r#"
schema = 1
[[vendors]]
id = "stripe"
packages = ["gem:stripe"]
[[vendors.sdk_bindings]]
package = "gem:stripe"
imports = ["stripe"]
member = "Customer.create"
method = "POST"
path = "/v1/customers"
"#,
    );
    let mut nodes = vec![
        node("root_caller", "root()", None),
        node("nested_caller", "nested()", None),
    ];
    nodes[0].source_file = "src/root.rb".into();
    nodes[1].source_file = "apps/nested/app.rb".into();
    let sdk_edge = |source: &str, file: &str| {
        let mut edge = http_edge(source, "candidate", "CALL", "sdk", "stripe", "/");
        edge.relation = "calls_sdk".into();
        edge.source_file = file.into();
        edge.extra.clear();
        edge.extra.insert("sdk_package".into(), json!("gem:stripe"));
        edge.extra
            .insert("sdk_member_chain".into(), json!("Customer.create"));
        edge
    };
    let mut edges = vec![
        sdk_edge("root_caller", "src/root.rb"),
        sdk_edge("nested_caller", "apps/nested/app.rb"),
    ];

    bind_repository_api_usages(repo.path(), &mut nodes, &mut edges, &registry).unwrap();

    let versions = edges
        .iter()
        .filter(|edge| edge.relation == "uses_api")
        .map(|edge| {
            (
                edge.source_file.as_str(),
                edge.extra["installed_sdk_version"]
                    .as_str()
                    .unwrap_or_default(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(versions.get("src/root.rb"), Some(&"13.1.0"));
    assert_eq!(versions.get("apps/nested/app.rb"), Some(&"18.2.0"));
}

#[test]
fn repository_binding_fails_closed_for_unscoped_conflicting_versions() {
    let repo = tempfile::tempdir().unwrap();
    for (directory, version) in [("apps/a", "13.1.0"), ("apps/b", "18.2.0")] {
        fs::create_dir_all(repo.path().join(directory)).unwrap();
        fs::write(
            repo.path().join(directory).join("Gemfile"),
            "gem 'stripe'\n",
        )
        .unwrap();
        fs::write(
            repo.path().join(directory).join("Gemfile.lock"),
            format!("GEM\n  specs:\n    stripe ({version})\n"),
        )
        .unwrap();
    }
    let registry = registry(
        r#"
schema = 1
[[vendors]]
id = "stripe"
packages = ["gem:stripe"]
[[vendors.sdk_bindings]]
package = "gem:stripe"
member = "Customer.create"
method = "POST"
path = "/v1/customers"
"#,
    );
    let mut caller = node("caller", "run()", None);
    caller.source_file = "tools/run.rb".into();
    let mut call = http_edge("caller", "candidate", "CALL", "sdk", "stripe", "/");
    call.relation = "calls_sdk".into();
    call.source_file = "tools/run.rb".into();
    call.extra.clear();
    call.extra.insert("sdk_package".into(), json!("gem:stripe"));
    call.extra
        .insert("sdk_member_chain".into(), json!("Customer.create"));
    let mut nodes = vec![caller];
    let mut edges = vec![call];

    bind_repository_api_usages(repo.path(), &mut nodes, &mut edges, &registry).unwrap();

    let usage = edges
        .iter()
        .find(|edge| edge.relation == "uses_api")
        .expect("usage still binds without claiming a version");
    assert!(
        !usage.extra.contains_key("installed_sdk_version"),
        "conflicting versions must fail closed: {:?}",
        usage.extra
    );
}
