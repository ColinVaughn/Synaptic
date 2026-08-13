use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::cargo::cargo_bin_cmd;
use serde::Deserialize;
use synaptic_api::{
    ApiChangeEvent, ApiInventory, ApiOperationAnchor, ApplicabilityState, SourceArtifact,
    evaluate_relevance, impact_from_nodes, inventory, load_optional_registry,
};
use synaptic_core::{GraphData, NodeId};
use synaptic_graph::KnowledgeGraph;

#[derive(Debug, Deserialize)]
struct Corpus {
    minimum_precision: f64,
    minimum_recall: f64,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    repository: String,
    vendor: String,
    method: String,
    path: String,
    affected_versions: String,
    applicability: String,
    expected_usages: Vec<ExpectedUsage>,
    #[serde(default)]
    forbidden_usage_files: Vec<String>,
    #[serde(default)]
    expected_test: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExpectedUsage {
    source_file: String,
    basis: String,
}

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/synaptic-api/tests/fixtures/api-maintenance")
}

fn copy_tree(source: &Path, target: &Path) {
    fs::create_dir_all(target).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        assert!(!kind.is_symlink(), "corpus must not contain symlinks");
        let destination = target.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), destination).unwrap();
        }
    }
}

fn usage_key(vendor: &str, method: &str, path: &str, file: &str, basis: &str) -> String {
    format!("{vendor}|{method}|{path}|{file}|{basis}")
}

#[test]
fn offline_corpus_meets_localization_applicability_and_impact_gates() {
    let root = corpus_root();
    let corpus: Corpus =
        serde_json::from_slice(&fs::read(root.join("expectations.json")).unwrap()).unwrap();
    assert!(
        corpus.cases.len() >= 5,
        "corpus must cover positive and negative cases"
    );

    let mut expected_total = 0_usize;
    let mut observed_total = 0_usize;
    let mut true_positives = 0_usize;
    let mut applicability_correct = 0_usize;

    for case in &corpus.cases {
        let temp = tempfile::tempdir().unwrap();
        copy_tree(&root.join("repos").join(&case.repository), temp.path());
        let output = cargo_bin_cmd!("synaptic")
            .args(["extract", "."])
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}: {}",
            case.repository,
            String::from_utf8_lossy(&output.stderr)
        );

        let graph: GraphData =
            serde_json::from_slice(&fs::read(temp.path().join("synaptic-out/graph.json")).unwrap())
                .unwrap();
        let expected = case
            .expected_usages
            .iter()
            .map(|usage| {
                usage_key(
                    &case.vendor,
                    &case.method,
                    &case.path,
                    &usage.source_file,
                    &usage.basis,
                )
            })
            .collect::<BTreeSet<_>>();
        let observed = graph
            .links
            .iter()
            .filter(|edge| edge.relation == "uses_api")
            .filter_map(|edge| {
                let operation = graph.nodes.iter().find(|node| node.id == edge.target)?;
                Some(usage_key(
                    operation.extra.get("vendor")?.as_str()?,
                    operation.extra.get("method")?.as_str()?,
                    operation.extra.get("canonical_path")?.as_str()?,
                    &edge.source_file.replace('\\', "/"),
                    edge.extra.get("binding_basis")?.as_str()?,
                ))
            })
            .collect::<BTreeSet<_>>();
        expected_total += expected.len();
        observed_total += observed.len();
        true_positives += expected.intersection(&observed).count();
        for forbidden in &case.forbidden_usage_files {
            assert!(
                graph
                    .links
                    .iter()
                    .filter(|edge| edge.relation == "uses_api")
                    .all(|edge| { edge.source_file.replace('\\', "/") != *forbidden }),
                "{} produced an unsafe binding in {forbidden}",
                case.repository
            );
        }

        let anchor = graph
            .nodes
            .iter()
            .find(|node| {
                node.extra.get("vendor").and_then(|value| value.as_str()) == Some(&case.vendor)
                    && node.extra.get("method").and_then(|value| value.as_str())
                        == Some(&case.method)
                    && node
                        .extra
                        .get("canonical_path")
                        .and_then(|value| value.as_str())
                        == Some(&case.path)
            })
            .map(|node| ApiOperationAnchor {
                id: node.id.0.clone(),
                vendor: case.vendor.clone(),
                protocol: "https".into(),
                method: case.method.clone(),
                canonical_path: case.path.clone(),
            })
            .unwrap_or_else(|| {
                ApiOperationAnchor::new(&case.vendor, "https", &case.method, &case.path)
            });
        let event: ApiChangeEvent = serde_json::from_value(serde_json::json!({
            "version": 1,
            "id": format!("corpus_event_{}", case.repository),
            "vendor": case.vendor,
            "occurred_at": 1,
            "source": SourceArtifact {
                uri: "fixture://contract-after".into(), revision: "2".into(),
                etag: None, last_modified: None, content_digest: "fixture".into(),
                fetched_at: 1, adapter_version: 1, evidence_kind: "openapi".into()
            },
            "changes": [{
                "change_id": "change_1", "kind": "operation_removed",
                "affected_versions": {"requirement": case.affected_versions},
                "old_operation": anchor, "old_sdk_symbols": [], "new_sdk_symbols": [],
                "migration_summary": "fixture migration", "evidence": [], "confidence": 1.0
            }]
        }))
        .unwrap();
        let registry = load_optional_registry(temp.path()).unwrap().unwrap();
        let observed_inventory =
            inventory(temp.path(), &registry).unwrap_or_else(|_| ApiInventory::default());
        let assessment = evaluate_relevance(&event, &registry, &observed_inventory, &graph, &[]);
        let expected_state = match case.applicability.as_str() {
            "applicable" => ApplicabilityState::Applicable,
            "review_required" => ApplicabilityState::ReviewRequired,
            "not_applicable" => ApplicabilityState::NotApplicable,
            state => panic!("unknown corpus applicability {state}"),
        };
        if assessment.state == expected_state {
            applicability_correct += 1;
        }
        assert_eq!(
            assessment.state, expected_state,
            "{}: {:?}",
            case.repository, assessment
        );
        if let Some(expected_test) = &case.expected_test {
            let impact = impact_from_nodes(
                &KnowledgeGraph::from_graph_data(graph.clone()),
                &assessment
                    .seed_node_ids
                    .iter()
                    .cloned()
                    .map(NodeId)
                    .collect::<Vec<_>>(),
                100,
            );
            assert!(
                impact
                    .at_risk_tests
                    .iter()
                    .any(|hit| &hit.file == expected_test),
                "{} did not select {expected_test}: {:?}",
                case.repository,
                impact.at_risk_tests
            );
        }
    }

    let precision = true_positives as f64 / observed_total.max(1) as f64;
    let recall = true_positives as f64 / expected_total.max(1) as f64;
    assert!(
        precision >= corpus.minimum_precision,
        "precision {precision:.3}"
    );
    assert!(recall >= corpus.minimum_recall, "recall {recall:.3}");
    assert_eq!(applicability_correct, corpus.cases.len());
}
