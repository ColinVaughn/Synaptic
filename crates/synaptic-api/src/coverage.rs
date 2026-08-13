use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synaptic_core::{Confidence, Edge, FileType, GraphData, KindValue, Node, NodeId};

use crate::{
    BehavioralRegressionCandidate, Dependency, DependencyScope, ExternalServiceEvidence,
    RuntimeEvidenceReport, RuntimeSurfaceEvidence, RuntimeSurfaceKind, SbomCompleteness,
    SbomEvidenceReport, VendorMatch, VendorRegistry, VendorSource,
};

struct CoverageIndexes<'a> {
    nodes_by_id: HashMap<&'a str, &'a Node>,
    bindings_by_source: HashMap<&'a str, Vec<&'a Edge>>,
    handled_external_nodes: HashSet<&'a str>,
    nodes_by_file: HashMap<String, Vec<&'a Node>>,
    nodes_by_file_suffix: HashMap<String, Vec<&'a Node>>,
}

impl<'a> CoverageIndexes<'a> {
    fn new(nodes: &'a [Node], edges: &'a [Edge], index_source_files: bool) -> Self {
        let mut nodes_by_id = HashMap::with_capacity(nodes.len());
        for node in nodes {
            nodes_by_id.entry(node.id.0.as_str()).or_insert(node);
        }
        let mut nodes_by_file = HashMap::<String, Vec<&Node>>::new();
        let mut nodes_by_file_suffix = HashMap::<String, Vec<&Node>>::new();
        if index_source_files {
            for node in nodes {
                if node.source_file.is_empty() {
                    continue;
                }
                let normalized = normalized_source_file(&node.source_file);
                nodes_by_file
                    .entry(normalized.clone())
                    .or_default()
                    .push(node);
                visit_path_suffixes(&normalized, |suffix| {
                    nodes_by_file_suffix
                        .entry(suffix.to_string())
                        .or_default()
                        .push(node);
                });
            }
        }
        let mut bindings_by_source = HashMap::<&str, Vec<&Edge>>::new();
        let mut handled_external_nodes = HashSet::new();
        for edge in edges {
            match edge.relation.as_str() {
                "uses_api" => bindings_by_source
                    .entry(edge.source.0.as_str())
                    .or_default()
                    .push(edge),
                "handled_by" => {
                    handled_external_nodes.insert(edge.source.0.as_str());
                }
                _ => {}
            }
        }
        Self {
            nodes_by_id,
            bindings_by_source,
            handled_external_nodes,
            nodes_by_file,
            nodes_by_file_suffix,
        }
    }

    fn source_file_candidates(&self, source_file: &str) -> Vec<&'a Node> {
        let normalized = normalized_source_file(source_file);
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        if let Some(nodes) = self.nodes_by_file_suffix.get(&normalized) {
            for node in nodes {
                if seen.insert(node.id.0.as_str()) {
                    candidates.push(*node);
                }
            }
        }
        visit_path_suffixes(&normalized, |suffix| {
            if let Some(nodes) = self.nodes_by_file.get(suffix) {
                for node in nodes {
                    if seen.insert(node.id.0.as_str()) {
                        candidates.push(*node);
                    }
                }
            }
        });
        candidates
    }
}

fn normalized_source_file(source_file: &str) -> String {
    source_file.replace('\\', "/").to_ascii_lowercase()
}

fn visit_path_suffixes(path: &str, mut visit: impl FnMut(&str)) {
    let mut suffix = path;
    loop {
        visit(suffix);
        let Some((_, remainder)) = suffix.split_once('/') else {
            break;
        };
        if remainder.is_empty() {
            break;
        }
        suffix = remainder;
    }
}

/// Highest evidence-backed stage reached by one observed external surface.
///
/// The stages are deliberately not inferred from one another. In this first
/// version only an existing graph binding can reach `bound`; merely declaring a
/// contract source in configuration does not claim that it parsed successfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageState {
    Observed,
    Identified,
    Modeled,
    Monitored,
    Bound,
    RepairEligible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalSurfaceKind {
    Http,
    Sdk,
    Rpc,
    Message,
    WebSocket,
    Command,
    Native,
    Service,
    PackageDependency,
    DynamicDispatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageGapKind {
    ProviderIdentity,
    AmbiguousOwner,
    ContractModel,
    ChangeSource,
    OperationBinding,
    ResolvedVersion,
    DynamicTarget,
    UsageClassification,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageGap {
    pub observation_id: String,
    pub kind: CoverageGapKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalSurfaceObservation {
    pub version: u32,
    pub id: String,
    pub kind: ExternalSurfaceKind,
    pub identity: String,
    pub state: CoverageState,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub authority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub package: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub member: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub resolved_version: Option<String>,
    pub source_file: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_node_id: Option<String>,
    pub evidence_digest: String,
    pub gaps: Vec<CoverageGapKind>,
    #[serde(default)]
    pub waived: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub waiver_reason: Option<String>,
}

impl ExternalSurfaceObservation {
    pub const VERSION: u32 = 1;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiCoverageReport {
    pub version: u32,
    /// True only when every observation has all evidence required for a bound,
    /// monitored surface. It is not a claim that unexecuted dynamic behavior is
    /// absent from the program.
    pub complete: bool,
    pub raw_evidence: usize,
    pub dependency_inventory: usize,
    /// Development/build/test dependencies are explicit negative controls: they
    /// remain inspectable but do not create API coverage gaps by themselves.
    pub development_dependencies: Vec<Dependency>,
    pub waivers_applied: usize,
    /// True when every supplied evidence source identifies a bounded observation
    /// window. Static extraction is a complete repository snapshot; trace imports
    /// must provide both start and end timestamps.
    pub evidence_complete: bool,
    pub evidence_windows: Vec<EvidenceWindow>,
    #[serde(default)]
    pub behavioral_review_candidates: Vec<BehavioralRegressionCandidate>,
    pub counts: BTreeMap<CoverageState, usize>,
    pub observations: Vec<ExternalSurfaceObservation>,
    pub gaps: Vec<CoverageGap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceWindow {
    pub evidence_kind: String,
    pub environment: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_unix_nano: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_unix_nano: Option<u64>,
    pub complete: bool,
    pub origin: String,
}

impl ApiCoverageReport {
    pub const VERSION: u32 = 1;
}

pub const EXTERNAL_SURFACE_NODE_TYPE: &str = "external_surface_observation";
pub const OBSERVES_EXTERNAL_RELATION: &str = "observes_external";

/// Rebuild the non-impacting observation overlay and return its coverage ledger.
/// Only exact `uses_api` edges participate in API repair impact; this relation is
/// deliberately absent from the query layer's structural-impact relation set.
pub fn attach_api_coverage(
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    dependencies: &[Dependency],
    registry: Option<&VendorRegistry>,
) -> ApiCoverageReport {
    attach_api_coverage_with_evidence(
        nodes,
        edges,
        dependencies,
        registry,
        &SbomEvidenceReport::default(),
    )
}

pub fn attach_api_coverage_with_evidence(
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    dependencies: &[Dependency],
    registry: Option<&VendorRegistry>,
    sbom: &SbomEvidenceReport,
) -> ApiCoverageReport {
    let stale_ids = nodes
        .iter()
        .filter(|node| {
            node.extra.get("_node_type").and_then(Value::as_str) == Some(EXTERNAL_SURFACE_NODE_TYPE)
        })
        .map(|node| node.id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    nodes.retain(|node| !stale_ids.contains(&node.id));
    edges.retain(|edge| {
        edge.relation != OBSERVES_EXTERNAL_RELATION
            && !stale_ids.contains(&edge.source)
            && !stale_ids.contains(&edge.target)
    });

    let report = analyze_api_coverage_parts(nodes, edges, dependencies, registry, &[], sbom);
    let source_ids = nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    for observation in report
        .observations
        .iter()
        .filter(|observation| observation.kind != ExternalSurfaceKind::PackageDependency)
    {
        let id = NodeId(observation.id.clone());
        let mut extra = serde_json::to_value(observation)
            .expect("coverage observation serializes")
            .as_object()
            .cloned()
            .expect("coverage observation is an object");
        // `Node.extra` is flattened during serialization. Never duplicate typed
        // Node fields or strict graph decoders will reject the shard.
        for reserved in ["id", "source_file", "source_location"] {
            extra.remove(reserved);
        }
        // `kind` is a typed Node field too, but this layer writes its own
        // vocabulary through it (`sdk`, `http`, `dynamic_dispatch`, …), which is
        // why the field is a `KindValue` rather than a bare `NodeKind`. Move the
        // value onto the field instead of leaving a duplicate in `extra`; it
        // serializes to the same top-level `kind` key either way.
        let observation_kind = extra
            .remove("kind")
            .and_then(|v| v.as_str().map(str::to_string))
            .map(KindValue::Other);
        extra.insert("_node_type".into(), json!(EXTERNAL_SURFACE_NODE_TYPE));
        extra.insert("coverage_state".into(), json!(observation.state));
        nodes.push(Node {
            id: id.clone(),
            label: format!("{:?}: {}", observation.kind, observation.identity),
            file_type: FileType::Concept,
            source_file: String::new(),
            source_location: None,
            community: None,
            repo: None,
            kind: observation_kind,
            extra,
            ..Default::default()
        });
        let Some(source) = observation
            .source_node_id
            .as_ref()
            .map(|source| NodeId(source.clone()))
            .filter(|source| source_ids.contains(source))
        else {
            continue;
        };
        let mut edge_extra = serde_json::Map::new();
        edge_extra.insert("evidence_digest".into(), json!(observation.evidence_digest));
        edge_extra.insert("coverage_state".into(), json!(observation.state));
        edge_extra.insert("coverage_gaps".into(), json!(observation.gaps));
        edge_extra.insert("waived".into(), json!(observation.waived));
        edges.push(Edge {
            source,
            target: id,
            relation: OBSERVES_EXTERNAL_RELATION.into(),
            confidence: Confidence::Inferred,
            source_file: observation.source_file.clone(),
            source_location: observation.source_location.clone(),
            confidence_score: Some(0.70),
            weight: 0.0,
            context: Some(observation.identity.clone()),
            cross_repo: false,
            extra: edge_extra,
        });
    }
    report
}

/// Build a deterministic, configuration-independent census from the external
/// call evidence already present in a Synaptic graph. Configuration can promote
/// ownership and existing `uses_api` edges can prove an exact binding, but raw
/// calls remain visible when neither exists.
pub fn analyze_api_coverage(
    graph: &GraphData,
    dependencies: &[Dependency],
    registry: Option<&VendorRegistry>,
) -> ApiCoverageReport {
    analyze_api_coverage_with_runtime(graph, dependencies, registry, &[])
}

pub fn analyze_api_coverage_with_runtime(
    graph: &GraphData,
    dependencies: &[Dependency],
    registry: Option<&VendorRegistry>,
    runtime_reports: &[RuntimeEvidenceReport],
) -> ApiCoverageReport {
    analyze_api_coverage_with_evidence(
        graph,
        dependencies,
        registry,
        runtime_reports,
        &SbomEvidenceReport::default(),
    )
}

pub fn analyze_api_coverage_with_evidence(
    graph: &GraphData,
    dependencies: &[Dependency],
    registry: Option<&VendorRegistry>,
    runtime_reports: &[RuntimeEvidenceReport],
    sbom: &SbomEvidenceReport,
) -> ApiCoverageReport {
    analyze_api_coverage_parts(
        &graph.nodes,
        &graph.links,
        dependencies,
        registry,
        runtime_reports,
        sbom,
    )
}

fn analyze_api_coverage_parts(
    nodes: &[Node],
    edges: &[Edge],
    dependencies: &[Dependency],
    registry: Option<&VendorRegistry>,
    runtime_reports: &[RuntimeEvidenceReport],
    sbom: &SbomEvidenceReport,
) -> ApiCoverageReport {
    let mut observations = Vec::new();
    let index_source_files = runtime_reports
        .iter()
        .flat_map(|report| &report.observations)
        .any(|observation| observation.source_file.is_some());
    let indexes = CoverageIndexes::new(nodes, edges, index_source_files);

    for edge in edges.iter().filter(|edge| edge.relation == "calls_service") {
        if let Some(observation) = http_observation(edge, &indexes, registry) {
            observations.push(observation);
        } else if let Some(observation) = boundary_observation(edge, &indexes, registry) {
            observations.push(observation);
        }
    }
    let mut dependency_version_cache = HashMap::<(String, String), Option<String>>::new();
    for edge in edges.iter().filter(|edge| edge.relation == "calls_sdk") {
        if let Some(observation) = sdk_observation(
            edge,
            &indexes,
            dependencies,
            registry,
            &mut dependency_version_cache,
        ) {
            observations.push(observation);
        }
    }
    for node in nodes {
        observations.extend(dynamic_observations(node));
    }
    observations.extend(external_link_observations(edges, &indexes, registry));
    let runtime_min_observations = registry
        .map(|registry| registry.config().coverage.runtime_min_observations)
        .unwrap_or(2);
    let runtime_indexes = &indexes;
    observations.extend(runtime_reports.iter().flat_map(|report| {
        report.observations.iter().map(move |observation| {
            runtime_coverage_observation(
                report,
                observation,
                runtime_min_observations,
                runtime_indexes,
            )
        })
    }));
    observations.extend(sbom.services.iter().map(sbom_service_observation));
    let observed_sdk_packages = observations
        .iter()
        .filter(|observation| observation.kind == ExternalSurfaceKind::Sdk)
        .filter_map(|observation| observation.package.as_deref())
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>();
    observations.extend(
        dependencies
            .iter()
            .filter(|dependency| dependency.scope != DependencyScope::Development)
            .filter(|dependency| {
                !observed_sdk_packages.contains(dependency.package.to_string().as_str())
            })
            .map(|dependency| dependency_observation(dependency, registry)),
    );

    observations.sort_by(|left, right| left.id.cmp(&right.id));
    observations.dedup_by(|left, right| left.id == right.id);
    let mut waivers_applied = 0;
    if let Some(registry) = registry {
        let mut waivers_by_observation = HashMap::<&str, Vec<&crate::CoverageWaiver>>::new();
        for waiver in &registry.config().coverage.waivers {
            waivers_by_observation
                .entry(waiver.observation_id.as_str())
                .or_default()
                .push(waiver);
        }
        for observation in &mut observations {
            if let Some(waiver) = waivers_by_observation
                .get(observation.id.as_str())
                .into_iter()
                .flatten()
                .find(|waiver| {
                    waiver
                        .evidence_digest
                        .eq_ignore_ascii_case(&observation.evidence_digest)
                })
            {
                observation.waived = true;
                observation.waiver_reason = Some(waiver.reason.trim().to_string());
                waivers_applied += 1;
            }
        }
    }

    let mut counts = BTreeMap::new();
    let mut gaps = Vec::new();
    for observation in &observations {
        *counts.entry(observation.state).or_insert(0) += 1;
        if observation.waived {
            continue;
        }
        gaps.extend(observation.gaps.iter().copied().map(|kind| CoverageGap {
            observation_id: observation.id.clone(),
            kind,
        }));
    }
    gaps.sort_by(|left, right| {
        left.observation_id
            .cmp(&right.observation_id)
            .then_with(|| left.kind.cmp(&right.kind))
    });
    let mut development_dependencies = dependencies
        .iter()
        .filter(|dependency| dependency.scope == DependencyScope::Development)
        .cloned()
        .collect::<Vec<_>>();
    development_dependencies.sort_by(|left, right| {
        left.package
            .cmp(&right.package)
            .then_with(|| left.source_file.cmp(&right.source_file))
            .then_with(|| left.scope.cmp(&right.scope))
    });

    let mut evidence_windows = vec![EvidenceWindow {
        evidence_kind: "static_extraction".into(),
        environment: "repository".into(),
        start_unix_nano: None,
        end_unix_nano: None,
        complete: true,
        origin: "graph".into(),
    }];
    evidence_windows.extend(runtime_reports.iter().map(|report| {
        EvidenceWindow {
            evidence_kind: "runtime_trace".into(),
            environment: report
                .environment
                .clone()
                .unwrap_or_else(|| "unknown".into()),
            start_unix_nano: report.window_start_unix_nano,
            end_unix_nano: report.window_end_unix_nano,
            complete: report.complete_window,
            origin: report.origin.clone(),
        }
    }));
    evidence_windows.extend(sbom.documents.iter().map(|document| EvidenceWindow {
        evidence_kind: "sbom_inventory".into(),
        environment: "repository".into(),
        start_unix_nano: None,
        end_unix_nano: None,
        complete: document.completeness == SbomCompleteness::Complete,
        origin: document.source_file.clone(),
    }));
    evidence_windows.sort_by(|left, right| {
        left.evidence_kind
            .cmp(&right.evidence_kind)
            .then_with(|| left.environment.cmp(&right.environment))
            .then_with(|| left.origin.cmp(&right.origin))
    });
    let evidence_complete = evidence_windows.iter().all(|window| window.complete);

    ApiCoverageReport {
        version: ApiCoverageReport::VERSION,
        complete: gaps.is_empty() && evidence_complete,
        raw_evidence: observations.len(),
        dependency_inventory: dependencies.len(),
        development_dependencies,
        waivers_applied,
        evidence_complete,
        evidence_windows,
        behavioral_review_candidates: Vec::new(),
        counts,
        observations,
        gaps,
    }
}

fn sbom_service_observation(service: &ExternalServiceEvidence) -> ExternalSurfaceObservation {
    let endpoint = service.endpoints.first();
    let (protocol, authority, path) = endpoint.map_or((None, None, None), |endpoint| {
        let (protocol, remainder) = endpoint
            .split_once("://")
            .map_or((None, endpoint.as_str()), |(protocol, remainder)| {
                (Some(protocol.to_ascii_lowercase()), remainder)
            });
        let (authority, path) = remainder
            .split_once('/')
            .map_or((remainder, None), |(authority, path)| {
                (authority, Some(format!("/{path}")))
            });
        (protocol, Some(authority.to_string()), path)
    });
    let mut gaps = vec![
        CoverageGapKind::ContractModel,
        CoverageGapKind::ChangeSource,
        CoverageGapKind::OperationBinding,
        CoverageGapKind::ResolvedVersion,
    ];
    if service.name == "unknown" {
        gaps.push(CoverageGapKind::ProviderIdentity);
        gaps.sort();
    }
    ExternalSurfaceObservation {
        version: ExternalSurfaceObservation::VERSION,
        id: format!("external_surface_{}", &service.evidence_digest[..24]),
        kind: ExternalSurfaceKind::Service,
        identity: if service.endpoints.is_empty() {
            service.name.clone()
        } else {
            format!("{} {}", service.name, service.endpoints.join(","))
        },
        state: if service.endpoints.is_empty() {
            CoverageState::Observed
        } else {
            CoverageState::Identified
        },
        provider: (service.name != "unknown").then(|| service.name.clone()),
        protocol,
        method: None,
        authority,
        path,
        package: None,
        member: None,
        operation_id: None,
        resolved_version: None,
        source_file: service.source_file.clone(),
        source_location: None,
        source_node_id: None,
        evidence_digest: service.evidence_digest.clone(),
        gaps,
        waived: false,
        waiver_reason: None,
    }
}

fn runtime_coverage_observation(
    report: &RuntimeEvidenceReport,
    runtime: &RuntimeSurfaceEvidence,
    minimum_observations: usize,
    indexes: &CoverageIndexes<'_>,
) -> ExternalSurfaceObservation {
    let (kind, provider, identity) = match runtime.kind {
        RuntimeSurfaceKind::Http => {
            let provider = runtime.authority.clone();
            let identity = format!(
                "{} {}{}",
                runtime.method,
                provider.as_deref().unwrap_or("<computed-host>"),
                runtime.path.as_deref().unwrap_or("/")
            );
            (ExternalSurfaceKind::Http, provider, identity)
        }
        RuntimeSurfaceKind::Rpc => {
            let provider = runtime.service.clone();
            let identity = format!(
                "{} {}/{}",
                runtime.protocol,
                provider.as_deref().unwrap_or("<service>"),
                runtime.operation.as_deref().unwrap_or("<method>")
            );
            (ExternalSurfaceKind::Rpc, provider, identity)
        }
        RuntimeSurfaceKind::Message => {
            let provider = runtime.authority.clone();
            let identity = format!(
                "{} {} {}",
                runtime.protocol,
                runtime.method,
                provider.as_deref().unwrap_or("<destination>")
            );
            (ExternalSurfaceKind::Message, provider, identity)
        }
    };
    let promoted = runtime.occurrences >= minimum_observations;
    let mut gaps = vec![
        CoverageGapKind::ContractModel,
        CoverageGapKind::ChangeSource,
        CoverageGapKind::OperationBinding,
        CoverageGapKind::ResolvedVersion,
    ];
    if !promoted {
        gaps.push(CoverageGapKind::DynamicTarget);
        gaps.sort();
    }
    let source_node_id = runtime_source_node(runtime, indexes).map(|node| node.id.0.clone());
    ExternalSurfaceObservation {
        version: ExternalSurfaceObservation::VERSION,
        id: format!("external_surface_{}", &runtime.evidence_digest[..24]),
        kind,
        identity,
        state: if promoted {
            CoverageState::Identified
        } else {
            CoverageState::Observed
        },
        provider,
        protocol: Some(runtime.protocol.clone()),
        method: Some(runtime.method.clone()),
        authority: runtime.authority.clone(),
        path: runtime.path.clone(),
        package: None,
        member: runtime.operation.clone(),
        operation_id: None,
        resolved_version: None,
        source_file: runtime
            .source_file
            .clone()
            .unwrap_or_else(|| report.origin.clone()),
        source_location: runtime.source_line.map(|line| line.to_string()),
        source_node_id,
        evidence_digest: runtime.evidence_digest.clone(),
        gaps,
        waived: false,
        waiver_reason: None,
    }
}

fn runtime_source_node<'a>(
    runtime: &RuntimeSurfaceEvidence,
    indexes: &'a CoverageIndexes<'a>,
) -> Option<&'a Node> {
    let source_file = runtime.source_file.as_deref()?;
    let file_candidates = indexes.source_file_candidates(source_file);
    if let Some(function) = runtime.source_function.as_deref() {
        let function = function.trim();
        let matches = file_candidates
            .iter()
            .copied()
            .filter(|node| {
                node.label.eq_ignore_ascii_case(function)
                    || node
                        .extra
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| name.eq_ignore_ascii_case(function))
            })
            .collect::<Vec<_>>();
        if matches.len() == 1 {
            return matches.into_iter().next();
        }
    }
    if let Some(line) = runtime.source_line {
        let matches = file_candidates
            .iter()
            .copied()
            .filter(|node| {
                node.source_location
                    .as_deref()
                    .is_some_and(|location| source_location_contains(location, line))
            })
            .collect::<Vec<_>>();
        if matches.len() == 1 {
            return matches.into_iter().next();
        }
    }
    (file_candidates.len() == 1).then(|| file_candidates[0])
}

fn source_location_contains(location: &str, line: u32) -> bool {
    let first_number = |value: &str| {
        value
            .split(|character: char| !character.is_ascii_digit())
            .find(|part| !part.is_empty())
            .and_then(|part| part.parse::<u32>().ok())
    };
    let Some((start, end)) = location.split_once('-') else {
        return first_number(location) == Some(line);
    };
    match (first_number(start), first_number(end)) {
        (Some(start), Some(end)) => (start..=end).contains(&line),
        _ => false,
    }
}

fn boundary_observation(
    source: &Edge,
    indexes: &CoverageIndexes<'_>,
    registry: Option<&VendorRegistry>,
) -> Option<ExternalSurfaceObservation> {
    let target = indexes.nodes_by_id.get(source.target.0.as_str()).copied()?;
    if indexes
        .handled_external_nodes
        .contains(target.id.0.as_str())
    {
        return None;
    }
    let node_type = target.extra.get("_node_type").and_then(Value::as_str)?;
    let (kind, protocol) = match node_type {
        "grpc_service" => (ExternalSurfaceKind::Rpc, "grpc"),
        "queue_topic" => (
            ExternalSurfaceKind::Message,
            source.context.as_deref().unwrap_or("message"),
        ),
        "ws_endpoint" | "ws_message" => (ExternalSurfaceKind::WebSocket, "websocket"),
        "ipc_channel" => (ExternalSurfaceKind::Message, "ipc"),
        "packet_channel" => (ExternalSurfaceKind::Message, "packet"),
        _ => return None,
    };
    Some(generic_observation(
        source,
        kind,
        target.label.clone(),
        protocol.to_ascii_lowercase(),
        registry,
    ))
}

fn external_link_observations(
    edges: &[Edge],
    indexes: &CoverageIndexes<'_>,
    registry: Option<&VendorRegistry>,
) -> Vec<ExternalSurfaceObservation> {
    edges
        .iter()
        .filter_map(|edge| {
            let kind = match edge.relation.as_str() {
                "invokes" => ExternalSurfaceKind::Command,
                "binds_native" => ExternalSurfaceKind::Native,
                _ => return None,
            };
            let target = indexes.nodes_by_id.get(edge.target.0.as_str()).copied()?;
            if !target.source_file.is_empty() {
                return None;
            }
            let protocol = match kind {
                ExternalSurfaceKind::Command => "command",
                ExternalSurfaceKind::Native => "native",
                _ => unreachable!("external link kind is command or native"),
            };
            Some(generic_observation(
                edge,
                kind,
                target.label.clone(),
                protocol.into(),
                registry,
            ))
        })
        .collect()
}

fn generic_observation(
    source: &Edge,
    kind: ExternalSurfaceKind,
    identity: String,
    protocol: String,
    registry: Option<&VendorRegistry>,
) -> ExternalSurfaceObservation {
    let id = observation_id(kind, source, &identity);
    ExternalSurfaceObservation {
        version: ExternalSurfaceObservation::VERSION,
        id,
        kind,
        identity,
        state: CoverageState::Observed,
        provider: None,
        protocol: Some(protocol),
        method: source.context.clone(),
        authority: None,
        path: None,
        package: None,
        member: None,
        operation_id: None,
        resolved_version: None,
        source_file: source.source_file.clone(),
        source_location: source.source_location.clone(),
        source_node_id: Some(source.source.0.clone()),
        evidence_digest: evidence_digest(source),
        gaps: coverage_gaps(None, false, false, false, registry),
        waived: false,
        waiver_reason: None,
    }
}

fn dependency_observation(
    dependency: &Dependency,
    registry: Option<&VendorRegistry>,
) -> ExternalSurfaceObservation {
    let (provider, ambiguous) = match registry.map(|registry| registry.match_dependency(dependency))
    {
        Some(VendorMatch::Matched { vendor_id }) => (Some(vendor_id), false),
        Some(VendorMatch::Ambiguous { .. }) => (None, true),
        Some(VendorMatch::Unmatched) | None => (None, false),
    };
    let identity = dependency.package.to_string();
    let canonical = serde_json::to_vec(dependency).expect("dependency evidence is serializable");
    let evidence_digest = blake3::hash(&canonical).to_hex().to_string();
    let id_source = format!(
        "{}\0{}\0{}\0{}",
        identity, dependency.source_file, dependency.scope as u8, evidence_digest
    );
    let id_digest = blake3::hash(id_source.as_bytes()).to_hex().to_string();
    let mut gaps = vec![CoverageGapKind::UsageClassification];
    if ambiguous {
        gaps.push(CoverageGapKind::AmbiguousOwner);
        gaps.sort();
    }
    ExternalSurfaceObservation {
        version: ExternalSurfaceObservation::VERSION,
        id: format!("external_surface_dependency_{}", &id_digest[..24]),
        kind: ExternalSurfaceKind::PackageDependency,
        identity,
        state: if provider.is_some() {
            CoverageState::Identified
        } else {
            CoverageState::Observed
        },
        provider,
        protocol: None,
        method: None,
        authority: None,
        path: None,
        package: Some(dependency.package.to_string()),
        member: None,
        operation_id: None,
        resolved_version: dependency.resolved_version.clone(),
        source_file: dependency.source_file.clone(),
        source_location: None,
        source_node_id: None,
        evidence_digest,
        gaps,
        waived: false,
        waiver_reason: None,
    }
}

fn http_observation(
    source: &Edge,
    indexes: &CoverageIndexes<'_>,
    registry: Option<&VendorRegistry>,
) -> Option<ExternalSurfaceObservation> {
    let get = |key| source.extra.get(key).and_then(Value::as_str);
    let method = get("http_method")?.trim().to_ascii_uppercase();
    let protocol = get("http_scheme")?.trim().to_ascii_lowercase();
    let authority = get("http_authority")?
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let path = get("http_path")?.trim().to_string();
    if method.is_empty() || protocol.is_empty() || authority.is_empty() || path.is_empty() {
        return None;
    }

    let (mut provider, mut ambiguous) = (None, false);
    if let Some(registry) = registry {
        match registry.match_host(&authority) {
            VendorMatch::Matched { vendor_id } => provider = Some(vendor_id),
            VendorMatch::Ambiguous { .. } => ambiguous = true,
            VendorMatch::Unmatched => {}
        }
    }
    let bound = matching_binding(source, indexes, "absolute_url_host");
    if provider.is_none() {
        provider = bound.and_then(binding_provider);
    }
    let operation_id = bound.map(binding_operation_id);
    let identity = format!("{protocol}://{authority} {method} {path}");
    let id = observation_id(ExternalSurfaceKind::Http, source, &identity);
    let gaps = coverage_gaps(
        provider.as_deref(),
        ambiguous,
        operation_id.is_some(),
        false,
        registry,
    );

    Some(ExternalSurfaceObservation {
        version: ExternalSurfaceObservation::VERSION,
        id,
        kind: ExternalSurfaceKind::Http,
        identity,
        state: coverage_state(provider.is_some(), operation_id.is_some()),
        provider,
        protocol: Some(protocol),
        method: Some(method),
        authority: Some(authority),
        path: Some(path),
        package: None,
        member: None,
        operation_id,
        resolved_version: None,
        source_file: source.source_file.clone(),
        source_location: source.source_location.clone(),
        source_node_id: Some(source.source.0.clone()),
        evidence_digest: evidence_digest(source),
        gaps,
        waived: false,
        waiver_reason: None,
    })
}

fn sdk_observation(
    source: &Edge,
    indexes: &CoverageIndexes<'_>,
    dependencies: &[Dependency],
    registry: Option<&VendorRegistry>,
    dependency_version_cache: &mut HashMap<(String, String), Option<String>>,
) -> Option<ExternalSurfaceObservation> {
    let get = |key| source.extra.get(key).and_then(Value::as_str);
    let package = get("sdk_package")
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let import = get("sdk_import")
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let member = get("sdk_member_chain")?.trim().to_string();
    if member.is_empty() || (package.is_none() && import.is_none()) {
        return None;
    }

    let (mut provider, mut ambiguous) = (None, false);
    if let Some(registry) = registry {
        if let Some(package) = package.and_then(|value| value.parse().ok()) {
            match registry.match_package(&package) {
                VendorMatch::Matched { vendor_id } => provider = Some(vendor_id),
                VendorMatch::Ambiguous { .. } => ambiguous = true,
                VendorMatch::Unmatched => {}
            }
        }
        if provider.is_none()
            && !ambiguous
            && let (Some(import), Some(ecosystem)) = (
                import,
                get("sdk_ecosystem").and_then(|value| value.parse().ok()),
            )
        {
            let matches = registry.sdk_bindings_for_import(ecosystem, import, &member);
            match matches.as_slice() {
                [(vendor, _)] => provider = Some((*vendor).to_string()),
                [] => {}
                _ => ambiguous = true,
            }
        }
    }

    let bound = matching_binding(source, indexes, "sdk_symbol");
    if provider.is_none() {
        provider = bound.and_then(binding_provider);
    }
    let operation_id = bound.map(binding_operation_id);
    let package = package.map(str::to_string);
    let resolved_version = bound
        .and_then(|edge| {
            edge.extra
                .get("installed_sdk_version")
                .and_then(Value::as_str)
        })
        .map(str::to_string)
        .or_else(|| {
            package.as_deref().and_then(|package| {
                dependency_version_cache
                    .entry((package.to_string(), source.source_file.clone()))
                    .or_insert_with(|| {
                        crate::binding::installed_version_for_source(
                            package,
                            &source.source_file,
                            dependencies,
                        )
                        .map(str::to_string)
                    })
                    .clone()
            })
        });
    let package_or_import = package.as_deref().or(import)?;
    let identity = format!("{package_or_import}#{member}");
    let id = observation_id(ExternalSurfaceKind::Sdk, source, &identity);
    let mut gaps = coverage_gaps(
        provider.as_deref(),
        ambiguous,
        operation_id.is_some(),
        true,
        registry,
    );
    if resolved_version.is_none() {
        gaps.push(CoverageGapKind::ResolvedVersion);
        gaps.sort();
        gaps.dedup();
    }

    Some(ExternalSurfaceObservation {
        version: ExternalSurfaceObservation::VERSION,
        id,
        kind: ExternalSurfaceKind::Sdk,
        identity,
        state: coverage_state(provider.is_some(), operation_id.is_some()),
        provider,
        protocol: None,
        method: None,
        authority: None,
        path: None,
        package,
        member: Some(member),
        operation_id,
        resolved_version,
        source_file: source.source_file.clone(),
        source_location: source.source_location.clone(),
        source_node_id: Some(source.source.0.clone()),
        evidence_digest: evidence_digest(source),
        gaps,
        waived: false,
        waiver_reason: None,
    })
}

fn dynamic_observations(node: &Node) -> Vec<ExternalSurfaceObservation> {
    node.dynamic_sites()
        .into_iter()
        .map(|site| {
            let key = site.key.as_deref().unwrap_or("<computed>");
            let identity = format!("{}:{key}", site.kind.as_str());
            let source_location = Some(site.line.to_string());
            let source = format!(
                "{}\0{}\0{}\0{}\0{}",
                node.id.0, node.source_file, site.line, identity, site.snippet
            );
            let digest = blake3::hash(source.as_bytes()).to_hex().to_string();
            ExternalSurfaceObservation {
                version: ExternalSurfaceObservation::VERSION,
                id: format!("external_surface_dynamic_{}", &digest[..24]),
                kind: ExternalSurfaceKind::DynamicDispatch,
                identity,
                state: CoverageState::Observed,
                provider: None,
                protocol: None,
                method: None,
                authority: None,
                path: None,
                package: None,
                member: site.key,
                operation_id: None,
                resolved_version: None,
                source_file: node.source_file.clone(),
                source_location,
                source_node_id: Some(node.id.0.clone()),
                evidence_digest: digest,
                gaps: vec![CoverageGapKind::DynamicTarget],
                waived: false,
                waiver_reason: None,
            }
        })
        .collect()
}

fn matching_binding<'a>(
    source: &Edge,
    indexes: &'a CoverageIndexes<'a>,
    basis: &str,
) -> Option<&'a Edge> {
    indexes
        .bindings_by_source
        .get(source.source.0.as_str())?
        .iter()
        .copied()
        .find(|candidate| {
            candidate.relation == "uses_api"
                && candidate.source == source.source
                && candidate.source_file == source.source_file
                && candidate.source_location == source.source_location
                && candidate.extra.get("binding_basis").and_then(Value::as_str) == Some(basis)
                && binding_evidence_matches(source, candidate, basis)
        })
}

fn binding_evidence_matches(source: &Edge, binding: &Edge, basis: &str) -> bool {
    let same = |key| source.extra.get(key) == binding.extra.get(key);
    match basis {
        "absolute_url_host" => {
            same("http_method")
                && same("http_scheme")
                && same("http_authority")
                && same("http_path")
        }
        "sdk_symbol" => {
            same("sdk_member_chain")
                && (source.extra.get("sdk_package").is_none() || same("sdk_package"))
                && (source.extra.get("sdk_import").is_none() || same("sdk_import"))
        }
        _ => false,
    }
}

fn binding_provider(edge: &Edge) -> Option<String> {
    edge.extra
        .get("api_vendor")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn binding_operation_id(edge: &Edge) -> String {
    edge.extra
        .get("operation_id")
        .and_then(Value::as_str)
        .unwrap_or(&edge.target.0)
        .to_string()
}

fn coverage_state(identified: bool, bound: bool) -> CoverageState {
    if bound {
        CoverageState::Bound
    } else if identified {
        CoverageState::Identified
    } else {
        CoverageState::Observed
    }
}

fn coverage_gaps(
    provider: Option<&str>,
    ambiguous: bool,
    bound: bool,
    sdk: bool,
    registry: Option<&VendorRegistry>,
) -> Vec<CoverageGapKind> {
    let mut gaps = Vec::new();
    if ambiguous {
        gaps.push(CoverageGapKind::AmbiguousOwner);
    }
    if provider.is_none() {
        gaps.push(CoverageGapKind::ProviderIdentity);
    }
    let vendor =
        provider.and_then(|provider| registry.and_then(|registry| registry.vendor(provider)));
    if vendor.is_none_or(|vendor| !vendor.sources.iter().any(is_machine_model_source)) {
        gaps.push(CoverageGapKind::ContractModel);
    }
    if vendor.is_none_or(|vendor| vendor.sources.is_empty()) {
        gaps.push(CoverageGapKind::ChangeSource);
    }
    if !bound {
        gaps.push(CoverageGapKind::OperationBinding);
    }
    if !sdk {
        gaps.sort();
        gaps.dedup();
    }
    gaps
}

fn is_machine_model_source(source: &VendorSource) -> bool {
    matches!(
        source,
        VendorSource::OpenApi { .. }
            | VendorSource::PackageRelease { .. }
            | VendorSource::StaticContract { .. }
            | VendorSource::Webhook { .. }
    )
}

fn observation_id(kind: ExternalSurfaceKind, edge: &Edge, identity: &str) -> String {
    let source = format!(
        "{:?}\0{}\0{}\0{}\0{}\0{}",
        kind,
        edge.source.0,
        edge.source_file,
        edge.source_location.as_deref().unwrap_or(""),
        identity,
        evidence_digest(edge)
    );
    let digest = blake3::hash(source.as_bytes()).to_hex().to_string();
    format!("external_surface_{}", &digest[..24])
}

fn evidence_digest(edge: &Edge) -> String {
    let bytes = serde_json::to_vec(&(
        &edge.source,
        &edge.relation,
        &edge.source_file,
        &edge.source_location,
        &edge.context,
        &edge.extra,
    ))
    .expect("coverage evidence is serializable");
    blake3::hash(&bytes).to_hex().to_string()
}
