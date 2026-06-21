use std::collections::HashSet;

use serde_json::Value;

use crate::error::CoreError;

const VALID_FILE_TYPES: [&str; 6] = ["code", "document", "paper", "image", "rationale", "concept"];
const VALID_CONFIDENCES: [&str; 3] = ["EXTRACTED", "INFERRED", "AMBIGUOUS"];
const REQUIRED_NODE_FIELDS: [&str; 4] = ["id", "label", "file_type", "source_file"];
const REQUIRED_EDGE_FIELDS: [&str; 5] =
    ["source", "target", "relation", "confidence", "source_file"];

/// Validate an extraction JSON value against the Synaptic schema. Returns one
/// message per violation; an empty vec means valid.
pub fn validate_extraction(data: &Value) -> Vec<String> {
    let Some(obj) = data.as_object() else {
        return vec!["Extraction must be a JSON object".to_string()];
    };

    let mut errors: Vec<String> = Vec::new();

    // nodes
    match obj.get("nodes") {
        None => errors.push("Missing required key 'nodes'".to_string()),
        Some(v) if !v.is_array() => errors.push("'nodes' must be a list".to_string()),
        Some(Value::Array(nodes)) => {
            for (i, node) in nodes.iter().enumerate() {
                let Some(n) = node.as_object() else {
                    errors.push(format!("Node {i} must be an object"));
                    continue;
                };
                let id_repr = n
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|s| format!("'{s}'"))
                    .unwrap_or_else(|| "'?'".to_string());
                for field in REQUIRED_NODE_FIELDS {
                    if !n.contains_key(field) {
                        errors.push(format!(
                            "Node {i} (id={id_repr}) missing required field '{field}'"
                        ));
                    }
                }
                if let Some(ft) = n.get("file_type").and_then(Value::as_str) {
                    if !VALID_FILE_TYPES.contains(&ft) {
                        errors.push(format!(
                            "Node {i} (id={id_repr}) has invalid file_type '{ft}' \
                             - must be one of {VALID_FILE_TYPES:?}"
                        ));
                    }
                }
            }
        }
        Some(_) => unreachable!("covered by the !is_array arm"),
    }

    // edges (accept "links" as a fallback for "edges")
    let edge_list = if obj.contains_key("edges") {
        obj.get("edges")
    } else {
        obj.get("links")
    };
    match edge_list {
        None => errors.push("Missing required key 'edges'".to_string()),
        Some(v) if !v.is_array() => errors.push("'edges' must be a list".to_string()),
        Some(Value::Array(edges)) => {
            let node_ids: HashSet<&str> = obj
                .get("nodes")
                .and_then(Value::as_array)
                .map(|ns| {
                    ns.iter()
                        .filter_map(|n| n.as_object()?.get("id")?.as_str())
                        .collect()
                })
                .unwrap_or_default();
            for (i, edge) in edges.iter().enumerate() {
                let Some(e) = edge.as_object() else {
                    errors.push(format!("Edge {i} must be an object"));
                    continue;
                };
                for field in REQUIRED_EDGE_FIELDS {
                    if !e.contains_key(field) {
                        errors.push(format!("Edge {i} missing required field '{field}'"));
                    }
                }
                if let Some(c) = e.get("confidence").and_then(Value::as_str) {
                    if !VALID_CONFIDENCES.contains(&c) {
                        errors.push(format!(
                            "Edge {i} has invalid confidence '{c}' \
                             - must be one of {VALID_CONFIDENCES:?}"
                        ));
                    }
                }
                if let Some(s) = e.get("source").and_then(Value::as_str) {
                    if !node_ids.is_empty() && !node_ids.contains(s) {
                        errors.push(format!("Edge {i} source '{s}' does not match any node id"));
                    }
                }
                if let Some(t) = e.get("target").and_then(Value::as_str) {
                    if !node_ids.is_empty() && !node_ids.contains(t) {
                        errors.push(format!("Edge {i} target '{t}' does not match any node id"));
                    }
                }
            }
        }
        Some(_) => unreachable!("covered by the !is_array arm"),
    }

    errors
}

/// Raise [`CoreError::Validation`] if the extraction has any violations.
pub fn assert_valid(data: &Value) -> crate::error::Result<()> {
    let errors = validate_extraction(data);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(CoreError::Validation { errors })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_doc() -> Value {
        json!({
            "nodes": [{"id": "a", "label": "A", "file_type": "code", "source_file": "a.py"}],
            "edges": [{"source": "a", "target": "a", "relation": "calls",
                       "confidence": "EXTRACTED", "source_file": "a.py"}]
        })
    }

    #[test]
    fn valid_doc_has_no_errors() {
        assert!(validate_extraction(&valid_doc()).is_empty());
    }

    #[test]
    fn non_object_is_rejected() {
        let errs = validate_extraction(&json!([1, 2, 3]));
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("must be a JSON object"));
    }

    #[test]
    fn missing_nodes_key_reported() {
        let errs = validate_extraction(&json!({"edges": []}));
        assert!(errs
            .iter()
            .any(|e| e.contains("Missing required key 'nodes'")));
    }

    #[test]
    fn missing_edges_and_links_reported() {
        let errs = validate_extraction(&json!({"nodes": []}));
        assert!(errs
            .iter()
            .any(|e| e.contains("Missing required key 'edges'")));
    }

    #[test]
    fn links_key_accepted_as_edges() {
        let doc = json!({
            "nodes": [{"id": "a", "label": "A", "file_type": "code", "source_file": "a.py"}],
            "links": []
        });
        assert!(validate_extraction(&doc).is_empty());
    }

    #[test]
    fn missing_required_node_field_reported() {
        let doc = json!({"nodes": [{"id": "a", "file_type": "code", "source_file": "a.py"}],
                         "edges": []});
        let errs = validate_extraction(&doc);
        assert!(errs
            .iter()
            .any(|e| e.contains("missing required field 'label'")));
    }

    #[test]
    fn invalid_file_type_reported() {
        let doc = json!({"nodes": [{"id": "a", "label": "A", "file_type": "banana",
                                    "source_file": "a.py"}], "edges": []});
        let errs = validate_extraction(&doc);
        assert!(errs.iter().any(|e| e.contains("invalid file_type")));
    }

    #[test]
    fn invalid_confidence_reported() {
        let doc = json!({
            "nodes": [{"id": "a", "label": "A", "file_type": "code", "source_file": "a.py"}],
            "edges": [{"source": "a", "target": "a", "relation": "calls",
                       "confidence": "MAYBE", "source_file": "a.py"}]
        });
        let errs = validate_extraction(&doc);
        assert!(errs.iter().any(|e| e.contains("invalid confidence")));
    }

    #[test]
    fn edge_endpoint_not_in_nodes_reported() {
        let doc = json!({
            "nodes": [{"id": "a", "label": "A", "file_type": "code", "source_file": "a.py"}],
            "edges": [{"source": "a", "target": "ghost", "relation": "calls",
                       "confidence": "EXTRACTED", "source_file": "a.py"}]
        });
        let errs = validate_extraction(&doc);
        assert!(errs
            .iter()
            .any(|e| e.contains("target 'ghost' does not match any node id")));
    }

    #[test]
    fn assert_valid_returns_err_on_invalid() {
        let res = assert_valid(&json!({"edges": []}));
        assert!(matches!(res, Err(CoreError::Validation { .. })));
    }

    #[test]
    fn assert_valid_ok_on_valid() {
        assert!(assert_valid(&valid_doc()).is_ok());
    }
}
