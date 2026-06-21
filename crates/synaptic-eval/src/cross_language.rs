//! Calibration for the cross-language edge layer (FFI / subprocess / HTTP / gRPC
//! / pyo3). These edges are INFERRED -- regex-driven, best-effort -- so the value
//! is knowing how grounded they are, not just how many there are. This is a
//! single-graph metric (no git history): it reports, per cross-language relation,
//! the counts plus two precision proxies that track how often the inference
//! actually closed a coupling.
//!
//! Proxy 1 -- *service connectivity*: of the service-boundary nodes (an HTTP
//! route, gRPC service, or pyo3 module), the fraction that are two-sided, i.e.
//! have BOTH a consumer (`calls_service` in) and a producer (`handled_by` out). A
//! two-sided boundary is almost certainly a real coupling; a half-open one is a
//! client to an out-of-repo service, a server with no in-repo client, or detector
//! noise. Tracking the ratio across releases calibrates detector precision.
//!
//! Proxy 2 -- *invocation resolution*: of the `invokes` (subprocess) edges, the
//! fraction whose target resolved to an in-repo file rather than staying an
//! external command stub.
//!
//! Calibration is advisory: this measures, it does not retune.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use synaptic_core::{GraphData, NodeId};

use crate::scoring::pct;

/// Cross-language relations emitted by the extract/graph layer.
const CROSS_LANGUAGE_RELATIONS: &[&str] =
    &["invokes", "binds_native", "calls_service", "handled_by"];

/// `_node_type`s of the synthetic boundary hubs a service consumer and producer
/// can meet at.
const SERVICE_BOUNDARY_TYPES: &[&str] = &["route", "grpc_service", "pyo3_module"];

/// A single-graph calibration of the cross-language edge layer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossLanguageReport {
    /// Edge count per cross-language relation.
    pub relation_counts: BTreeMap<String, usize>,
    /// Total cross-language edges.
    pub total_edges: usize,
    /// Service-boundary nodes (HTTP route / gRPC service / pyo3 module).
    pub service_boundaries: usize,
    /// Service boundaries that are two-sided (a consumer in AND a producer out).
    pub service_two_sided: usize,
    /// `invokes` (subprocess) edges.
    pub invocations_total: usize,
    /// `invokes` edges whose target resolved to an in-repo file.
    pub invocations_resolved: usize,
    /// `binds_native` (FFI) edges.
    pub ffi_bindings: usize,
}

impl CrossLanguageReport {
    /// Of the service-boundary nodes, the percentage that are two-sided. A
    /// precision proxy: higher means more boundaries are fully-closed couplings.
    pub fn service_connectivity_pct(&self) -> u8 {
        pct(self.service_two_sided, self.service_boundaries)
    }

    /// Of the subprocess invocations, the percentage retargeted to an in-repo
    /// file (vs left as an external command stub).
    pub fn invocation_resolution_pct(&self) -> u8 {
        pct(self.invocations_resolved, self.invocations_total)
    }

    /// One-line human summary.
    pub fn summary(&self) -> String {
        format!(
            "cross-language: {} edge(s); service boundaries {}/{} two-sided ({}%); \
             invocations {}/{} resolved ({}%); {} FFI binding(s)",
            self.total_edges,
            self.service_two_sided,
            self.service_boundaries,
            self.service_connectivity_pct(),
            self.invocations_resolved,
            self.invocations_total,
            self.invocation_resolution_pct(),
            self.ffi_bindings,
        )
    }
}

/// Compute the cross-language calibration for one built graph.
pub fn calibrate_cross_language(graph: &GraphData) -> CrossLanguageReport {
    // Real in-repo nodes carry a source file; boundary stubs do not.
    let has_source: HashSet<&NodeId> = graph
        .nodes
        .iter()
        .filter(|n| !n.source_file.is_empty())
        .map(|n| &n.id)
        .collect();

    let mut report = CrossLanguageReport::default();
    // A boundary is two-sided when it has BOTH a consumer (it is the target of a
    // `calls_service`) and a producer (it is the source of a `handled_by`) -- the
    // directions are tracked specifically, not just any in/out edge.
    let mut has_consumer: HashSet<&NodeId> = HashSet::new();
    let mut has_producer: HashSet<&NodeId> = HashSet::new();

    for e in &graph.links {
        if !CROSS_LANGUAGE_RELATIONS.contains(&e.relation.as_str()) {
            continue;
        }
        *report
            .relation_counts
            .entry(e.relation.clone())
            .or_insert(0) += 1;
        report.total_edges += 1;
        match e.relation.as_str() {
            "calls_service" => {
                has_consumer.insert(&e.target);
            }
            "handled_by" => {
                has_producer.insert(&e.source);
            }
            "invokes" => {
                report.invocations_total += 1;
                if has_source.contains(&e.target) {
                    report.invocations_resolved += 1;
                }
            }
            "binds_native" => report.ffi_bindings += 1,
            _ => {}
        }
    }

    for n in &graph.nodes {
        let is_boundary = n
            .extra
            .get("_node_type")
            .and_then(|v| v.as_str())
            .is_some_and(|t| SERVICE_BOUNDARY_TYPES.contains(&t));
        if !is_boundary {
            continue;
        }
        report.service_boundaries += 1;
        if has_consumer.contains(&n.id) && has_producer.contains(&n.id) {
            report.service_two_sided += 1;
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};
    use synaptic_core::{Confidence, Edge, FileType, Node};

    fn file_node(id: &str, label: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: format!("{id}.rs"),
            source_location: None,
            community: None,
            repo: None,
            extra: Map::new(),
        }
    }

    fn boundary(id: &str, label: &str, node_type: &str) -> Node {
        let mut extra = Map::new();
        extra.insert("_node_type".into(), json!(node_type));
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: String::new(),
            source_location: None,
            community: None,
            repo: None,
            extra,
        }
    }

    fn edge(src: &str, tgt: &str, rel: &str) -> Edge {
        Edge {
            source: NodeId(src.into()),
            target: NodeId(tgt.into()),
            relation: rel.into(),
            confidence: Confidence::Inferred,
            source_file: "a".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    fn graph(nodes: Vec<Node>, links: Vec<Edge>) -> GraphData {
        GraphData {
            nodes,
            links,
            ..Default::default()
        }
    }

    #[test]
    fn two_sided_route_counts_as_connected() {
        // client -> route (calls_service), route -> handler (handled_by): the route
        // boundary is two-sided.
        let g = graph(
            vec![
                file_node("client", "fetch()"),
                file_node("handler", "list()"),
                boundary("r", "/api/users", "route"),
            ],
            vec![
                edge("client", "r", "calls_service"),
                edge("r", "handler", "handled_by"),
            ],
        );
        let report = calibrate_cross_language(&g);
        assert_eq!(report.service_boundaries, 1);
        assert_eq!(report.service_two_sided, 1, "route has consumer + producer");
        assert_eq!(report.service_connectivity_pct(), 100);
        assert_eq!(report.total_edges, 2);
    }

    #[test]
    fn one_sided_route_is_not_connected() {
        // A client calling an out-of-repo service: route has a consumer but no
        // in-repo handler.
        let g = graph(
            vec![
                file_node("client", "fetch()"),
                boundary("r", "/ext/api", "route"),
            ],
            vec![edge("client", "r", "calls_service")],
        );
        let report = calibrate_cross_language(&g);
        assert_eq!(report.service_boundaries, 1);
        assert_eq!(report.service_two_sided, 0, "no producer side");
        assert_eq!(report.service_connectivity_pct(), 0);
    }

    #[test]
    fn invocation_resolution_distinguishes_in_repo_from_external() {
        // One invokes resolved to an in-repo file, one left as an external command
        // stub.
        let g = graph(
            vec![
                file_node("deploy", "deploy()"),
                file_node("tool_rs", "tool.rs"),
                boundary("cmd_git", "git", "command"),
            ],
            vec![
                edge("deploy", "tool_rs", "invokes"), // resolved
                edge("deploy", "cmd_git", "invokes"), // external
            ],
        );
        let report = calibrate_cross_language(&g);
        assert_eq!(report.invocations_total, 2);
        assert_eq!(report.invocations_resolved, 1, "only the in-repo target");
        assert_eq!(report.invocation_resolution_pct(), 50);
    }

    #[test]
    fn relation_counts_and_ffi_tallied() {
        let g = graph(
            vec![
                file_node("f", "f()"),
                boundary("lib", "libmath", "native_library"),
            ],
            vec![
                edge("f", "lib", "binds_native"),
                edge("f", "lib", "binds_native"),
            ],
        );
        let report = calibrate_cross_language(&g);
        assert_eq!(report.ffi_bindings, 2);
        assert_eq!(report.relation_counts.get("binds_native"), Some(&2));
        // native_library is not a service boundary, so it is not counted there.
        assert_eq!(report.service_boundaries, 0);
    }

    #[test]
    fn empty_graph_is_all_zero() {
        let report = calibrate_cross_language(&graph(vec![], vec![]));
        assert_eq!(report.total_edges, 0);
        assert_eq!(report.service_connectivity_pct(), 0);
        assert_eq!(report.invocation_resolution_pct(), 0);
    }
}
