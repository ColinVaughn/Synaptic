use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use synaptic_core::{NodeId, Visibility};
use synaptic_graph::{KnowledgeGraph, find_import_cycles};

use crate::{ApiChangeEvent, RelevanceAssessment};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantCheck {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiInvariantReport {
    pub version: u32,
    pub passed: bool,
    pub checks: Vec<InvariantCheck>,
}

pub fn verify_api_invariants(
    before: &KnowledgeGraph,
    after: &KnowledgeGraph,
    event: &ApiChangeEvent,
    assessment: &RelevanceAssessment,
    full_incremental_equivalent: bool,
) -> ApiInvariantReport {
    let old_operations = event
        .changes
        .iter()
        .filter_map(|change| change.old_operation.as_ref())
        .map(|operation| NodeId(operation.id.clone()))
        .collect::<BTreeSet<_>>();
    let replacements = event
        .changes
        .iter()
        .filter_map(|change| {
            Some((
                NodeId(change.old_operation.as_ref()?.id.clone()),
                NodeId(change.new_operation.as_ref()?.id.clone()),
            ))
        })
        .collect::<BTreeSet<_>>();
    let affected_callers = assessment
        .bindings
        .iter()
        .map(|binding| NodeId(binding.caller_node_id.clone()))
        .collect::<BTreeSet<_>>();
    let affected_files = assessment
        .bindings
        .iter()
        .map(|binding| binding.source_file.as_str())
        .collect::<BTreeSet<_>>();

    let remaining_old = after
        .edges()
        .filter(|edge| {
            edge.relation == "uses_api"
                && old_operations.contains(&edge.target)
                && affected_callers.contains(&edge.source)
        })
        .count();
    let old_removed = check(
        "deprecated-bindings-removed",
        remaining_old == 0,
        format!("{remaining_old} deprecated binding(s) remain at affected callers"),
    );

    let expected_replacements = replacements
        .iter()
        .flat_map(|(old, new)| {
            assessment
                .bindings
                .iter()
                .filter(move |binding| binding.operation_node_id == old.0)
                .map(move |binding| (NodeId(binding.caller_node_id.clone()), new.clone()))
        })
        .collect::<BTreeSet<_>>();
    let missing_replacements = expected_replacements
        .iter()
        .filter(|(caller, new)| {
            !after.edges().any(|edge| {
                edge.relation == "uses_api" && &edge.source == caller && &edge.target == new
            })
        })
        .count();
    let replacement_present = check(
        "replacement-bindings-present",
        missing_replacements == 0,
        format!("{missing_replacements} affected caller/replacement binding(s) are missing"),
    );

    let before_vendor = vendor_usage_edges(before, &event.vendor);
    let after_vendor = vendor_usage_edges(after, &event.vendor);
    let preserved_expected = before_vendor
        .iter()
        .filter(|(_, target)| !old_operations.contains(target))
        .collect::<BTreeSet<_>>();
    let missing_preserved = preserved_expected
        .iter()
        .filter(|edge| !after_vendor.contains(edge))
        .count();
    let preserved = check(
        "non-migrated-bindings-preserved",
        missing_preserved == 0,
        format!("{missing_preserved} pre-existing non-migrated binding(s) disappeared"),
    );

    let allowed_additions = expected_replacements;
    let unrelated_additions = after_vendor
        .difference(&before_vendor)
        .filter(|edge| !allowed_additions.contains(*edge))
        .count();
    let unrelated = check(
        "no-unrelated-vendor-bindings",
        unrelated_additions == 0,
        format!("{unrelated_additions} unrelated vendor binding(s) were added"),
    );

    let before_located = before
        .nodes()
        .filter(|node| affected_files.contains(node.source_file.as_str()))
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    let lost_nodes = before_located
        .iter()
        .filter(|id| !after.contains_node(id))
        .count();
    let located = check(
        "affected-code-nodes-preserved",
        lost_nodes == 0,
        format!("{lost_nodes} located node(s) in affected files disappeared"),
    );

    let before_stubs = external_stubs(before, &affected_files);
    let after_stubs = external_stubs(after, &affected_files);
    let new_stubs = after_stubs.difference(&before_stubs).count();
    let stubs = check(
        "no-new-unresolved-stubs",
        new_stubs == 0,
        format!("{new_stubs} new unresolved external stub(s) appeared"),
    );

    let before_cycles = cycles(before);
    let after_cycles = cycles(after);
    let new_cycles = after_cycles.difference(&before_cycles).count();
    let cycles_check = check(
        "no-new-cycles",
        new_cycles == 0,
        format!("{new_cycles} new import cycle(s) appeared"),
    );

    let public_before = before
        .nodes()
        .filter(|node| {
            affected_files.contains(node.source_file.as_str())
                && node.visibility() == Some(Visibility::Public)
        })
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    let removed_public = public_before
        .iter()
        .filter(|id| !after.contains_node(id))
        .count();
    let public = check(
        "public-apis-preserved",
        removed_public == 0,
        format!("{removed_public} public API node(s) were removed"),
    );
    let parity = check(
        "full-incremental-parity",
        full_incremental_equivalent,
        if full_incremental_equivalent {
            "full and incremental extraction agree".into()
        } else {
            "full and incremental extraction differ".into()
        },
    );
    let checks = vec![
        old_removed,
        replacement_present,
        preserved,
        unrelated,
        located,
        stubs,
        cycles_check,
        public,
        parity,
    ];
    ApiInvariantReport {
        version: 1,
        passed: checks.iter().all(|item| item.passed),
        checks,
    }
}

fn vendor_usage_edges(graph: &KnowledgeGraph, vendor: &str) -> BTreeSet<(NodeId, NodeId)> {
    let operations = graph
        .nodes()
        .filter(|node| {
            node.extra
                .get("_node_type")
                .and_then(serde_json::Value::as_str)
                == Some("api_operation")
                && node.extra.get("vendor").and_then(serde_json::Value::as_str) == Some(vendor)
        })
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    graph
        .edges()
        .filter(|edge| edge.relation == "uses_api" && operations.contains(&edge.target))
        .map(|edge| (edge.source.clone(), edge.target.clone()))
        .collect()
}

fn external_stubs(graph: &KnowledgeGraph, files: &BTreeSet<&str>) -> BTreeSet<NodeId> {
    graph
        .nodes()
        .filter(|node| {
            files.contains(node.source_file.as_str())
                && node
                    .extra
                    .get("_node_type")
                    .and_then(serde_json::Value::as_str)
                    == Some("external_stub")
        })
        .map(|node| node.id.clone())
        .collect()
}

fn cycles(graph: &KnowledgeGraph) -> BTreeSet<Vec<String>> {
    find_import_cycles(graph, 20, 10_000)
        .into_iter()
        .map(|mut cycle| {
            cycle.cycle.sort();
            cycle.cycle
        })
        .collect()
}

fn check(name: &str, passed: bool, detail: String) -> InvariantCheck {
    InvariantCheck {
        name: name.into(),
        passed,
        detail,
    }
}
