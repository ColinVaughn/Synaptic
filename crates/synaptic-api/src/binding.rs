use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use synaptic_core::{make_id, Confidence, Edge, FileType, Node, NodeId};

use crate::{
    scan_dependencies, ApiOperationAnchor, Dependency, InventoryError, VendorConfig, VendorMatch,
    VendorRegistry,
};

pub const API_VENDOR_NODE_TYPE: &str = "api_vendor";
pub const API_OPERATION_NODE_TYPE: &str = "api_operation";

/// Counts from rebuilding the direct-HTTP API overlay in a graph.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiBindingReport {
    pub vendors: usize,
    pub operations: usize,
    pub usages: usize,
    pub sdk_packages: usize,
    pub ambiguous: usize,
}

/// Rebuild the complete current API overlay from source-call evidence and
/// repository dependency inventory.
pub fn bind_repository_api_usages(
    root: &Path,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    registry: &VendorRegistry,
) -> Result<ApiBindingReport, InventoryError> {
    let dependencies = scan_dependencies(root)?;
    Ok(bind_repository_api_usages_with_dependencies(
        nodes,
        edges,
        registry,
        &dependencies,
    ))
}

/// Rebuild the API overlay from a dependency snapshot already collected by the caller.
pub fn bind_repository_api_usages_with_dependencies(
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    registry: &VendorRegistry,
    dependencies: &[Dependency],
) -> ApiBindingReport {
    let direct = bind_direct_http_usages(nodes, edges, registry);
    let sdk_dependencies = bind_sdk_dependencies(nodes, edges, registry, dependencies);
    let sdk_usages = bind_sdk_usages_with_dependencies(nodes, edges, registry, Some(dependencies));
    ApiBindingReport {
        vendors: nodes
            .iter()
            .filter(|node| node_type(node) == Some(API_VENDOR_NODE_TYPE))
            .count(),
        operations: sdk_usages.operations,
        usages: direct.usages + sdk_usages.usages,
        sdk_packages: sdk_dependencies.sdk_packages,
        ambiguous: direct.ambiguous + sdk_dependencies.ambiguous + sdk_usages.ambiguous,
    }
}

/// Rebuild vendor-aware API operation bindings from structured absolute-URL
/// evidence on `calls_service` edges.
///
/// Existing route nodes and edges are retained for backwards compatibility.
/// Previously generated direct-HTTP bindings are removed first, making this
/// safe to call after either a full extraction or an incremental merge.
pub fn bind_direct_http_usages(
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    registry: &VendorRegistry,
) -> ApiBindingReport {
    nodes.retain(|node| {
        !matches!(
            node_type(node),
            Some(API_VENDOR_NODE_TYPE | API_OPERATION_NODE_TYPE)
        )
    });
    edges.retain(|edge| !matches!(edge.relation.as_str(), "uses_api" | "provided_by"));

    let evidence: Vec<Edge> = edges
        .iter()
        .filter(|edge| edge.relation == "calls_service")
        .cloned()
        .collect();
    let mut vendor_nodes = BTreeMap::<String, Node>::new();
    let mut operation_nodes = BTreeMap::<NodeId, Node>::new();
    let mut provided = BTreeMap::<(NodeId, NodeId), Edge>::new();
    let mut usage_keys = BTreeSet::new();
    let mut usage_edges = Vec::new();
    let mut report = ApiBindingReport::default();

    for source_edge in evidence {
        let Some(http) = HttpEvidence::from_edge(&source_edge) else {
            continue;
        };
        let vendor_id = match registry.match_host(http.authority) {
            VendorMatch::Matched { vendor_id } => vendor_id,
            VendorMatch::Ambiguous { .. } => {
                report.ambiguous += 1;
                continue;
            }
            VendorMatch::Unmatched => continue,
        };
        let Some(vendor) = registry.vendor(&vendor_id) else {
            continue;
        };
        let anchor = ApiOperationAnchor::new(&vendor_id, http.scheme, http.method, http.path);
        let operation_id = NodeId(anchor.id.clone());
        let vendor_id_node = NodeId(format!("api_vendor:{vendor_id}"));

        vendor_nodes
            .entry(vendor_id.clone())
            .or_insert_with(|| vendor_node(vendor));
        operation_nodes
            .entry(operation_id.clone())
            .or_insert_with(|| operation_node(&anchor, Some(http.authority)));
        provided
            .entry((operation_id.clone(), vendor_id_node.clone()))
            .or_insert_with(|| provided_by_edge(&operation_id, &vendor_id_node, &vendor_id));

        let usage_key = (
            source_edge.source.clone(),
            operation_id.clone(),
            source_edge.context.clone(),
            source_edge.source_file.clone(),
            source_edge.source_location.clone(),
        );
        if usage_keys.insert(usage_key) {
            usage_edges.push(direct_uses_api_edge(source_edge, &anchor, &vendor_id));
            report.usages += 1;
        }
    }

    report.vendors = vendor_nodes.len();
    report.operations = operation_nodes.len();
    nodes.extend(vendor_nodes.into_values());
    nodes.extend(operation_nodes.into_values());
    edges.extend(provided.into_values());
    edges.extend(usage_edges);
    report
}

/// Resolve generic `calls_sdk` candidates through configured adapter rules.
/// Call this after dependency binding so installed versions can be attached.
pub fn bind_sdk_usages(
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    registry: &VendorRegistry,
) -> ApiBindingReport {
    bind_sdk_usages_with_dependencies(nodes, edges, registry, None)
}

fn bind_sdk_usages_with_dependencies(
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    registry: &VendorRegistry,
    dependencies: Option<&[Dependency]>,
) -> ApiBindingReport {
    let evidence = edges
        .iter()
        .filter(|edge| edge.relation == "calls_sdk")
        .cloned()
        .collect::<Vec<_>>();
    let mut report = ApiBindingReport::default();
    let mut usage_keys = edges
        .iter()
        .filter(|edge| edge.relation == "uses_api")
        .map(|edge| {
            (
                edge.source.clone(),
                edge.target.clone(),
                edge.source_file.clone(),
                edge.source_location.clone(),
            )
        })
        .collect::<BTreeSet<_>>();
    let mut provided = edges
        .iter()
        .filter(|edge| edge.relation == "provided_by")
        .map(|edge| (edge.source.clone(), edge.target.clone()))
        .collect::<BTreeSet<_>>();
    let mut node_ids = nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    let installed_versions = nodes
        .iter()
        .filter_map(|node| {
            Some((
                node.extra.get("package")?.as_str()?.to_string(),
                node.extra.get("resolved_version")?.as_str()?.to_string(),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let mut dependency_version_cache = HashMap::<(String, String), Option<String>>::new();

    for source_edge in evidence {
        let Some(member) = source_edge
            .extra
            .get("sdk_member_chain")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        let package_hint = source_edge.extra.get("sdk_package").and_then(Value::as_str);
        let generated = GeneratedApiEvidence::from_edge(&source_edge, registry);
        let (vendor_id, anchor, package_raw, binding_basis) = if let Some(generated) = generated {
            (
                generated.vendor_id,
                generated.anchor,
                generated.package,
                "generated_client",
            )
        } else {
            let mut resolved = None;
            if let Some(package_raw) = package_hint {
                if let Ok(package) = package_raw.parse::<crate::PackageCoordinate>() {
                    match registry.match_package(&package) {
                        VendorMatch::Matched { vendor_id } => {
                            if let Some(rule) = registry.sdk_binding(&vendor_id, &package, &member)
                            {
                                resolved = Some((vendor_id, rule));
                            }
                        }
                        VendorMatch::Ambiguous { .. } => {
                            report.ambiguous += 1;
                            continue;
                        }
                        VendorMatch::Unmatched => {}
                    }
                }
            }
            if resolved.is_none() {
                let Some(import) = source_edge.extra.get("sdk_import").and_then(Value::as_str)
                else {
                    continue;
                };
                let Some(ecosystem_raw) = source_edge
                    .extra
                    .get("sdk_ecosystem")
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                let Ok(ecosystem) = ecosystem_raw.parse() else {
                    continue;
                };
                let namespace_matches =
                    registry.sdk_bindings_for_import(ecosystem, import, &member);
                match namespace_matches.as_slice() {
                    [] => continue,
                    [(vendor_id, rule)] => resolved = Some(((*vendor_id).to_string(), *rule)),
                    _ => {
                        report.ambiguous += 1;
                        continue;
                    }
                }
            }
            let Some((vendor_id, rule)) = resolved else {
                continue;
            };
            (
                vendor_id.clone(),
                ApiOperationAnchor::new(&vendor_id, &rule.protocol, &rule.method, &rule.path),
                rule.package.to_string(),
                "sdk_symbol",
            )
        };
        let Some(vendor) = registry.vendor(&vendor_id) else {
            continue;
        };
        let operation_id = NodeId(anchor.id.clone());
        let vendor_node_id = NodeId(format!("api_vendor:{vendor_id}"));
        if node_ids.insert(vendor_node_id.clone()) {
            nodes.push(vendor_node(vendor));
        }
        if node_ids.insert(operation_id.clone()) {
            nodes.push(operation_node(
                &anchor,
                vendor.hosts.first().map(String::as_str),
            ));
        }
        if provided.insert((operation_id.clone(), vendor_node_id.clone())) {
            edges.push(provided_by_edge(&operation_id, &vendor_node_id, &vendor_id));
        }
        let usage_key = (
            source_edge.source.clone(),
            operation_id.clone(),
            source_edge.source_file.clone(),
            source_edge.source_location.clone(),
        );
        if usage_keys.insert(usage_key) {
            let installed = if let Some(dependencies) = dependencies {
                dependency_version_cache
                    .entry((package_raw.clone(), source_edge.source_file.clone()))
                    .or_insert_with(|| {
                        installed_version_for_source(
                            &package_raw,
                            &source_edge.source_file,
                            dependencies,
                        )
                        .map(str::to_string)
                    })
                    .as_deref()
            } else {
                installed_version_for_sdk_package(&package_raw, &installed_versions)
            };
            edges.push(sdk_uses_api_edge(
                source_edge,
                &anchor,
                &vendor_id,
                &package_raw,
                &member,
                installed,
                binding_basis,
            ));
            report.usages += 1;
        }
    }
    report.vendors = nodes
        .iter()
        .filter(|node| node_type(node) == Some(API_VENDOR_NODE_TYPE))
        .count();
    report.operations = nodes
        .iter()
        .filter(|node| node_type(node) == Some(API_OPERATION_NODE_TYPE))
        .count();
    report
}

struct GeneratedApiEvidence {
    vendor_id: String,
    package: String,
    anchor: ApiOperationAnchor,
}

impl GeneratedApiEvidence {
    fn from_edge(source: &Edge, registry: &VendorRegistry) -> Option<Self> {
        let generated = source.extra.get("generated_api")?.as_object()?;
        let vendor_id = generated
            .get("vendor")?
            .as_str()?
            .trim()
            .to_ascii_lowercase();
        let package = source.extra.get("sdk_package")?.as_str()?;
        let coordinate = package.parse::<crate::PackageCoordinate>().ok()?;
        match registry.match_package(&coordinate) {
            VendorMatch::Matched { vendor_id: matched } if matched == vendor_id => {}
            _ => return None,
        }
        let protocol = bounded_generated_coordinate(generated.get("protocol")?.as_str()?)?;
        let method = bounded_generated_coordinate(generated.get("method")?.as_str()?)?;
        let path = bounded_generated_coordinate(generated.get("path")?.as_str()?)?;
        let anchor = ApiOperationAnchor::new(&vendor_id, protocol, method, path);
        if generated
            .get("operation_id")
            .and_then(Value::as_str)
            .is_some_and(|operation_id| operation_id != anchor.id)
        {
            return None;
        }
        Some(Self {
            vendor_id,
            package: coordinate.to_string(),
            anchor,
        })
    }
}

fn bounded_generated_coordinate(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= 1_024 && !value.chars().any(char::is_control))
        .then_some(value)
}

pub(crate) fn installed_version_for_source<'a>(
    package: &str,
    source_file: &str,
    dependencies: &'a [Dependency],
) -> Option<&'a str> {
    let source_file = source_file.replace('\\', "/");
    let mut nearest_depth = None;
    let mut nearest = Vec::new();
    let mut compatible = Vec::new();
    for dependency in dependencies {
        if !sdk_package_matches_dependency(package, &dependency.package.to_string()) {
            continue;
        }
        compatible.push(dependency);
        let manifest = dependency.source_file.replace('\\', "/");
        let manifest_dir = manifest.rsplit_once('/').map_or("", |(dir, _)| dir);
        let in_scope = manifest_dir.is_empty()
            || source_file == manifest_dir
            || source_file
                .strip_prefix(manifest_dir)
                .is_some_and(|suffix| suffix.starts_with('/'));
        if !in_scope {
            continue;
        }
        let depth = manifest_dir
            .split('/')
            .filter(|part| !part.is_empty())
            .count();
        match nearest_depth {
            Some(current) if depth < current => continue,
            Some(current) if depth == current => nearest.push(dependency),
            _ => {
                nearest_depth = Some(depth);
                nearest.clear();
                nearest.push(dependency);
            }
        }
    }
    let candidates = if nearest.is_empty() {
        &compatible
    } else {
        &nearest
    };
    if candidates.is_empty()
        || candidates
            .iter()
            .any(|dependency| dependency.resolved_version.is_none())
    {
        return None;
    }
    let versions = candidates
        .iter()
        .filter_map(|dependency| dependency.resolved_version.as_deref())
        .collect::<BTreeSet<_>>();
    (versions.len() == 1).then(|| *versions.first().expect("one scoped version"))
}

fn sdk_package_matches_dependency(package: &str, dependency: &str) -> bool {
    package == dependency
        || (package.starts_with("go:")
            && dependency.starts_with("go:")
            && package
                .strip_prefix(dependency)
                .is_some_and(|suffix| suffix.starts_with('/')))
}

fn installed_version_for_sdk_package<'a>(
    package: &str,
    installed_versions: &'a BTreeMap<String, String>,
) -> Option<&'a str> {
    if let Some(version) = installed_versions.get(package) {
        return Some(version);
    }
    if !package.starts_with("go:") {
        return None;
    }
    installed_versions
        .iter()
        .filter(|(module, _)| {
            module.starts_with("go:")
                && package
                    .strip_prefix(module.as_str())
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
        .max_by_key(|(module, _)| module.len())
        .map(|(_, version)| version.as_str())
}

/// Join matched dependency inventory to reusable external package nodes.
/// Installed-but-unused SDKs receive `sdk_for` inventory edges but no operation
/// binding, keeping applicability distinct from usage.
pub fn bind_sdk_dependencies(
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    registry: &VendorRegistry,
    dependencies: &[Dependency],
) -> ApiBindingReport {
    edges.retain(|edge| edge.relation != "sdk_for");
    for node in nodes.iter_mut().filter(|node| node.source_file.is_empty()) {
        node.extra.remove("api_vendor");
        node.extra.remove("ecosystem");
        node.extra.remove("package");
        node.extra.remove("declared_requirement");
        node.extra.remove("resolved_version");
    }

    let mut report = ApiBindingReport::default();
    let mut package_vendors = BTreeSet::new();
    let mut sdk_edges = BTreeMap::<(NodeId, NodeId), Edge>::new();
    let mut node_positions = HashMap::<NodeId, usize>::with_capacity(nodes.len());
    let mut external_nodes_by_label = HashMap::<String, Vec<usize>>::new();
    for (index, node) in nodes.iter().enumerate() {
        node_positions.entry(node.id.clone()).or_insert(index);
        if node.source_file.is_empty() {
            external_nodes_by_label
                .entry(node.label.to_ascii_lowercase())
                .or_default()
                .push(index);
        }
    }
    for dependency in dependencies {
        let vendor_id = match registry.match_dependency(dependency) {
            VendorMatch::Matched { vendor_id } => vendor_id,
            VendorMatch::Ambiguous { .. } => {
                report.ambiguous += 1;
                continue;
            }
            VendorMatch::Unmatched => continue,
        };
        let Some(vendor) = registry.vendor(&vendor_id) else {
            continue;
        };
        let (package_id, package_index) = reusable_package_node(
            nodes,
            dependency,
            &mut node_positions,
            &mut external_nodes_by_label,
        );
        let vendor_id_node = NodeId(format!("api_vendor:{vendor_id}"));
        if !node_positions.contains_key(&vendor_id_node) {
            let node = vendor_node(vendor);
            let index = nodes.len();
            external_nodes_by_label
                .entry(node.label.to_ascii_lowercase())
                .or_default()
                .push(index);
            node_positions.insert(node.id.clone(), index);
            nodes.push(node);
        }
        let package = &mut nodes[package_index];
        package.extra.insert("_node_type".into(), json!("package"));
        package.extra.insert("api_vendor".into(), json!(vendor_id));
        package.extra.insert(
            "ecosystem".into(),
            json!(dependency.package.ecosystem.as_str()),
        );
        package
            .extra
            .insert("package".into(), json!(dependency.package.to_string()));
        if let Some(requirement) = &dependency.declared_requirement {
            package
                .extra
                .insert("declared_requirement".into(), json!(requirement));
        }
        if let Some(version) = &dependency.resolved_version {
            package
                .extra
                .insert("resolved_version".into(), json!(version));
        }

        package_vendors.insert((package_id.clone(), vendor_id.clone()));
        let edge = sdk_for_edge(&package_id, &vendor_id_node, &vendor_id, dependency);
        sdk_edges
            .entry((package_id, vendor_id_node))
            .and_modify(|current| current.merge_sites_from(&edge))
            .or_insert(edge);
    }
    report.sdk_packages = package_vendors.len();
    edges.extend(sdk_edges.into_values());
    report.vendors = nodes
        .iter()
        .filter(|node| node_type(node) == Some(API_VENDOR_NODE_TYPE))
        .count();
    report
}

fn reusable_package_node(
    nodes: &mut Vec<Node>,
    dependency: &Dependency,
    node_positions: &mut HashMap<NodeId, usize>,
    external_nodes_by_label: &mut HashMap<String, Vec<usize>>,
) -> (NodeId, usize) {
    let canonical_id = NodeId(make_id(&[
        dependency.package.ecosystem.as_str(),
        &dependency.package.name,
    ]));
    if let Some(&index) = node_positions.get(&canonical_id) {
        return (nodes[index].id.clone(), index);
    }
    let coordinate = dependency.package.to_string();
    let name_key = dependency.package.name.to_ascii_lowercase();
    let coordinate_key = coordinate.to_ascii_lowercase();
    let mut candidate = None;
    for key in [name_key.as_str(), coordinate_key.as_str()] {
        if let Some(indexes) = external_nodes_by_label.get(key) {
            for &index in indexes {
                let node = &nodes[index];
                if node
                    .extra
                    .get("package")
                    .and_then(Value::as_str)
                    .is_none_or(|package| package == coordinate)
                    && candidate.is_none_or(|current| index < current)
                {
                    candidate = Some(index);
                }
            }
        }
    }
    if let Some(index) = candidate {
        return (nodes[index].id.clone(), index);
    }
    let node = Node {
        id: canonical_id.clone(),
        label: dependency.package.name.clone(),
        file_type: FileType::Code,
        source_file: String::new(),
        source_location: None,
        community: None,
        repo: None,
        extra: Map::new(),
    };
    let index = nodes.len();
    external_nodes_by_label
        .entry(node.label.to_ascii_lowercase())
        .or_default()
        .push(index);
    node_positions.insert(canonical_id.clone(), index);
    nodes.push(node);
    (canonical_id, index)
}

fn sdk_for_edge(
    package_id: &NodeId,
    vendor_id_node: &NodeId,
    vendor_id: &str,
    dependency: &Dependency,
) -> Edge {
    let mut extra = Map::new();
    extra.insert("api_vendor".into(), json!(vendor_id));
    extra.insert("package".into(), json!(dependency.package.to_string()));
    if let Some(requirement) = &dependency.declared_requirement {
        extra.insert("declared_requirement".into(), json!(requirement));
    }
    if let Some(version) = &dependency.resolved_version {
        extra.insert("resolved_version".into(), json!(version));
    }
    Edge {
        source: package_id.clone(),
        target: vendor_id_node.clone(),
        relation: "sdk_for".into(),
        confidence: Confidence::Extracted,
        source_file: dependency.source_file.clone(),
        source_location: None,
        confidence_score: Some(Confidence::Extracted.default_score()),
        weight: 1.0,
        context: Some(dependency.package.to_string()),
        cross_repo: false,
        extra,
    }
}

fn node_type(node: &Node) -> Option<&str> {
    node.extra.get("_node_type").and_then(Value::as_str)
}

struct HttpEvidence<'a> {
    method: &'a str,
    scheme: &'a str,
    authority: &'a str,
    path: &'a str,
}

impl<'a> HttpEvidence<'a> {
    fn from_edge(edge: &'a Edge) -> Option<Self> {
        let get = |key| edge.extra.get(key).and_then(Value::as_str);
        let method = get("http_method")?;
        let scheme = get("http_scheme")?;
        let authority = get("http_authority")?;
        let path = get("http_path")?;
        if method.is_empty() || scheme.is_empty() || authority.is_empty() || path.is_empty() {
            return None;
        }
        Some(Self {
            method,
            scheme,
            authority,
            path,
        })
    }
}

fn vendor_node(vendor: &VendorConfig) -> Node {
    let mut extra = Map::new();
    extra.insert("_node_type".into(), json!(API_VENDOR_NODE_TYPE));
    extra.insert("vendor".into(), json!(vendor.id));
    extra.insert("hosts".into(), json!(vendor.hosts));
    extra.insert("packages".into(), json!(vendor.packages));
    Node {
        id: NodeId(format!("api_vendor:{}", vendor.id)),
        label: vendor.id.clone(),
        file_type: FileType::Concept,
        source_file: String::new(),
        source_location: None,
        community: None,
        repo: None,
        extra,
    }
}

fn operation_node(anchor: &ApiOperationAnchor, authority: Option<&str>) -> Node {
    let mut extra = Map::new();
    extra.insert("_node_type".into(), json!(API_OPERATION_NODE_TYPE));
    extra.insert("vendor".into(), json!(anchor.vendor));
    extra.insert("operation_id".into(), json!(anchor.id));
    extra.insert("protocol".into(), json!(anchor.protocol));
    extra.insert("method".into(), json!(anchor.method));
    extra.insert("canonical_path".into(), json!(anchor.canonical_path));
    if let Some(authority) = authority {
        extra.insert("authority".into(), json!(authority));
    }
    Node {
        id: NodeId(anchor.id.clone()),
        label: format!(
            "{} {} ({})",
            anchor.method, anchor.canonical_path, anchor.vendor
        ),
        file_type: FileType::Concept,
        source_file: String::new(),
        source_location: None,
        community: None,
        repo: None,
        extra,
    }
}

fn direct_uses_api_edge(source: Edge, anchor: &ApiOperationAnchor, vendor_id: &str) -> Edge {
    let evidence_digest = binding_evidence_digest(&source, &anchor.id);
    let mut extra = source.extra;
    extra.insert("api_vendor".into(), json!(vendor_id));
    extra.insert("operation_id".into(), json!(anchor.id));
    extra.insert("binding_basis".into(), json!("absolute_url_host"));
    extra.insert("adapter_version".into(), json!(1));
    extra.insert("evidence_digest".into(), json!(evidence_digest));
    Edge {
        source: source.source,
        target: NodeId(anchor.id.clone()),
        relation: "uses_api".into(),
        confidence: source.confidence,
        source_file: source.source_file,
        source_location: source.source_location,
        // A literal absolute URL corroborated by an enabled vendor host matcher,
        // HTTP method, and canonical path is substantially stronger than the
        // generic cross-language candidate from which it was derived.
        confidence_score: Some(0.99),
        weight: source.weight,
        context: source.context,
        cross_repo: source.cross_repo,
        extra,
    }
}

fn sdk_uses_api_edge(
    source: Edge,
    anchor: &ApiOperationAnchor,
    vendor_id: &str,
    package: &str,
    member: &str,
    installed_version: Option<&str>,
    binding_basis: &str,
) -> Edge {
    let evidence_digest = binding_evidence_digest(&source, &anchor.id);
    let mut extra = source.extra;
    extra.insert("api_vendor".into(), json!(vendor_id));
    extra.insert("operation_id".into(), json!(anchor.id));
    extra.insert("binding_basis".into(), json!(binding_basis));
    extra.insert("adapter_version".into(), json!(1));
    extra.insert("sdk_package".into(), json!(package));
    extra.insert("sdk_member_chain".into(), json!(member));
    if let Some(version) = installed_version {
        extra.insert("installed_sdk_version".into(), json!(version));
    }
    extra.insert("evidence_digest".into(), json!(evidence_digest));
    Edge {
        source: source.source,
        target: NodeId(anchor.id.clone()),
        relation: "uses_api".into(),
        confidence: source.confidence,
        source_file: source.source_file,
        source_location: source.source_location,
        // Package import + exact member chain + explicit adapter rule is an
        // automation-grade binding. Computed/dynamic member access never reaches
        // this constructor and remains unresolved.
        confidence_score: Some(0.98),
        weight: source.weight,
        context: Some(format!("{package} {member}")),
        cross_repo: source.cross_repo,
        extra,
    }
}

fn binding_evidence_digest(source: &Edge, operation_id: &str) -> String {
    let identity = format!(
        "{}\0{}\0{}\0{}\0{}",
        source.source.0,
        source.source_file,
        source.source_location.as_deref().unwrap_or(""),
        source.context.as_deref().unwrap_or(""),
        operation_id
    );
    blake3::hash(identity.as_bytes()).to_hex().to_string()
}

fn provided_by_edge(operation_id: &NodeId, vendor_node_id: &NodeId, vendor_id: &str) -> Edge {
    let mut extra = Map::new();
    extra.insert("api_vendor".into(), json!(vendor_id));
    extra.insert("binding_basis".into(), json!("configured_host"));
    Edge {
        source: operation_id.clone(),
        target: vendor_node_id.clone(),
        relation: "provided_by".into(),
        confidence: Confidence::Extracted,
        source_file: String::new(),
        source_location: None,
        confidence_score: Some(Confidence::Extracted.default_score()),
        weight: 1.0,
        context: None,
        cross_repo: false,
        extra,
    }
}
