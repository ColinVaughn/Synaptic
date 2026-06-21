//! Convert an LLM extraction [`Fragment`] (loose JSON node-link) into typed
//! `synaptic-core` nodes/edges. Concept nodes are tagged `_origin: "semantic"`
//! (not `"ast"`) so the build-stage ghost remap can collapse a semantic node
//! that duplicates an AST symbol onto the AST node.

use serde_json::{Map, Value};
use synaptic_core::{make_id, Confidence, Edge, FileType, Node, NodeId};
use synaptic_llm::Fragment;

const CORE_NODE_KEYS: &[&str] = &[
    "id",
    "label",
    "file_type",
    "source_file",
    "source_location",
    "community",
    "repo",
];

/// Convert a fragment to `(nodes, edges)`. Nodes/edges missing required fields
/// are skipped (robust to imperfect model output).
pub fn fragment_to_graph(frag: &Fragment) -> (Vec<Node>, Vec<Edge>) {
    let nodes = frag.nodes.iter().filter_map(value_to_node).collect();
    let edges = frag.edges.iter().filter_map(value_to_edge).collect();
    (nodes, edges)
}

fn str_field(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

fn value_to_node(v: &Value) -> Option<Node> {
    let id = str_field(v, "id")
        .filter(|s| !s.is_empty())
        .or_else(|| str_field(v, "label").map(|l| make_id(&[&l])))
        .filter(|s| !s.is_empty())?;
    let label = str_field(v, "label").unwrap_or_else(|| id.clone());
    let file_type = FileType::from_lenient(
        v.get("file_type")
            .and_then(Value::as_str)
            .unwrap_or("concept"),
    );

    // Tag semantic provenance and carry non-core scalar fields (source_url,
    // author, etc.) through to graph.json.
    let mut extra = Map::new();
    extra.insert("_origin".to_string(), Value::String("semantic".to_string()));
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if !CORE_NODE_KEYS.contains(&k.as_str()) && !val.is_null() {
                extra.insert(k.clone(), val.clone());
            }
        }
    }

    Some(Node {
        id: NodeId(id),
        label,
        file_type,
        source_file: str_field(v, "source_file").unwrap_or_default(),
        source_location: str_field(v, "source_location"),
        community: None,
        repo: None,
        extra,
    })
}

fn parse_confidence(s: Option<&str>) -> Confidence {
    match s.map(str::to_uppercase).as_deref() {
        Some("EXTRACTED") => Confidence::Extracted,
        Some("AMBIGUOUS") => Confidence::Ambiguous,
        _ => Confidence::Inferred,
    }
}

fn value_to_edge(v: &Value) -> Option<Edge> {
    let source = str_field(v, "source").filter(|s| !s.is_empty())?;
    let target = str_field(v, "target").filter(|s| !s.is_empty())?;
    Some(Edge {
        source: NodeId(source),
        target: NodeId(target),
        relation: str_field(v, "relation").unwrap_or_else(|| "conceptually_related_to".to_string()),
        confidence: parse_confidence(v.get("confidence").and_then(Value::as_str)),
        confidence_score: v
            .get("confidence_score")
            .and_then(Value::as_f64)
            .map(|f| f as f32),
        source_file: str_field(v, "source_file").unwrap_or_default(),
        source_location: str_field(v, "source_location"),
        weight: v
            .get("weight")
            .and_then(Value::as_f64)
            .map(|f| f as f32)
            .unwrap_or(1.0),
        context: str_field(v, "context"),
        cross_repo: false,
        extra: Map::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn converts_concept_node_and_edge() {
        let frag = Fragment::from_value(&json!({
            "nodes": [
                {"id": "doc_auth", "label": "Authentication", "file_type": "concept", "source_file": "doc.md"},
                {"id": "doc_session", "label": "Session", "file_type": "rationale", "source_file": "doc.md", "author": "x"}
            ],
            "edges": [
                {"source": "doc_auth", "target": "doc_session", "relation": "conceptually_related_to", "confidence": "INFERRED", "confidence_score": 0.7}
            ]
        }));
        let (nodes, edges) = fragment_to_graph(&frag);
        assert_eq!(nodes.len(), 2);
        let auth = &nodes[0];
        assert_eq!(auth.id, NodeId("doc_auth".into()));
        assert_eq!(auth.file_type, FileType::Concept);
        // Provenance tag, so it ghost-remaps instead of being treated as AST.
        assert_eq!(
            auth.extra.get("_origin").and_then(Value::as_str),
            Some("semantic")
        );
        // Non-core fields carried into extra.
        assert_eq!(
            nodes[1].extra.get("author").and_then(Value::as_str),
            Some("x")
        );
        assert_eq!(nodes[1].file_type, FileType::Rationale);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].confidence, Confidence::Inferred);
        assert_eq!(edges[0].confidence_score, Some(0.7));
    }

    #[test]
    fn semantic_concepts_flow_through_dedup_fuzzy_merge() {
        // Converted LLM concept output runs through synaptic-graph's fuzzy dedup
        // pass (a no-op on code-only graphs); near-identical concepts collapse to one.
        use std::collections::HashMap;
        use synaptic_graph::deduplicate_entities;
        let frag = Fragment::from_value(&json!({
            "nodes": [
                {"id": "a_alg", "label": "Distributed Consensus Algorithm", "file_type": "concept", "source_file": "a.md"},
                {"id": "b_alg", "label": "Distributed Consensos Algorithm", "file_type": "concept", "source_file": "b.md"}
            ],
            "edges": []
        }));
        let (nodes, edges) = fragment_to_graph(&frag);
        assert_eq!(nodes.len(), 2);
        let (deduped, _) = deduplicate_entities(nodes, edges, &HashMap::new());
        assert_eq!(
            deduped.len(),
            1,
            "near-duplicate concepts must merge via the fuzzy dedup pass"
        );
    }

    #[test]
    fn unknown_file_type_falls_back_to_concept_and_bad_rows_skipped() {
        let frag = Fragment::from_value(&json!({
            "nodes": [
                {"id": "n1", "label": "Thing", "file_type": "gizmo"},
                {"label": "no id but label"},
                {"nothing": true}
            ],
            "edges": [
                {"source": "n1"},
                {"source": "a", "target": "b"}
            ]
        }));
        let (nodes, edges) = fragment_to_graph(&frag);
        // n1 (concept) plus the label-only node (id derived); empty node skipped.
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].file_type, FileType::Concept);
        // Only the edge with both endpoints survives.
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].relation, "conceptually_related_to");
    }
}
