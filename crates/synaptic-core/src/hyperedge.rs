use serde::{Deserialize, Serialize};

use crate::confidence::Confidence;
use crate::id::NodeId;

/// A many-node grouping. The on-disk shape is `{id, label, nodes}`;
/// `relation`/`confidence` are optional extensions omitted when unset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hyperedge {
    pub id: String,
    pub label: String,
    pub nodes: Vec<NodeId>,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub relation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub confidence: Option<Confidence>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_shape_has_expected_keys() {
        let h = Hyperedge {
            id: "he1".into(),
            label: "auth cluster".into(),
            nodes: vec![NodeId("a".into()), NodeId("b".into())],
            relation: None,
            confidence: None,
        };
        let json = serde_json::to_value(&h).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(
            obj.keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            ["id", "label", "nodes"]
                .into_iter()
                .map(String::from)
                .collect()
        );
        assert_eq!(obj["nodes"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn roundtrips_with_optional_fields() {
        let raw = serde_json::json!({
            "id": "he1", "label": "x", "nodes": ["a"],
            "relation": "co_located", "confidence": "INFERRED"
        });
        let h: Hyperedge = serde_json::from_value(raw).unwrap();
        assert_eq!(h.relation.as_deref(), Some("co_located"));
        assert_eq!(h.confidence, Some(Confidence::Inferred));
    }
}
