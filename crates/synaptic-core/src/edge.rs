use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::confidence::Confidence;
use crate::id::NodeId;

/// A directed relationship between two nodes. The required fields are the ones
/// in `REQUIRED_EDGE_FIELDS`. `_src`/`_tgt` build-layer direction markers are
/// intentionally NOT typed here (they are a petgraph-build concern stripped on
/// export); if present on input they land in `extra`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub source: NodeId,
    pub target: NodeId,
    pub relation: String,
    pub confidence: Confidence,
    pub source_file: String,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub confidence_score: Option<f32>,
    #[serde(default = "default_weight")]
    pub weight: f32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub context: Option<String>,
    /// True for federated cross-repo edges; omitted when false.
    #[serde(skip_serializing_if = "is_false", default)]
    pub cross_repo: bool,

    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_weight() -> f32 {
    1.0
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Edge {
        Edge {
            source: NodeId("a".into()),
            target: NodeId("b".into()),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            source_file: "src/a.py".into(),
            source_location: Some("L10".into()),
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    #[test]
    fn required_keys_and_relation_string() {
        let json = serde_json::to_value(sample()).unwrap();
        let obj = json.as_object().unwrap();
        for k in ["source", "target", "relation", "confidence", "source_file"] {
            assert!(obj.contains_key(k), "missing {k}");
        }
        assert_eq!(obj["relation"], serde_json::json!("calls"));
        assert_eq!(obj["confidence"], serde_json::json!("EXTRACTED"));
        assert_eq!(obj["weight"], serde_json::json!(1.0));
    }

    #[test]
    fn omits_false_cross_repo_and_unset_options() {
        let obj = serde_json::to_value(sample()).unwrap();
        let obj = obj.as_object().unwrap().clone();
        assert!(!obj.contains_key("cross_repo")); // false -> omitted
        assert!(!obj.contains_key("confidence_score"));
        assert!(!obj.contains_key("context"));
    }

    #[test]
    fn weight_defaults_to_one_when_absent() {
        let raw = serde_json::json!({
            "source": "a", "target": "b", "relation": "imports",
            "confidence": "INFERRED", "source_file": "src/a.py",
            "confidence_score": 0.8
        });
        let e: Edge = serde_json::from_value(raw).unwrap();
        assert_eq!(e.weight, 1.0);
        assert_eq!(e.confidence, Confidence::Inferred);
        assert_eq!(e.confidence_score, Some(0.8));
    }

    #[test]
    fn direction_markers_land_in_extra() {
        let raw = serde_json::json!({
            "source": "a", "target": "b", "relation": "calls",
            "confidence": "EXTRACTED", "source_file": "src/a.py",
            "_src": "a", "_tgt": "b"
        });
        let e: Edge = serde_json::from_value(raw).unwrap();
        assert_eq!(e.extra.get("_src").unwrap(), "a");
        assert_eq!(e.extra.get("_tgt").unwrap(), "b");
    }
}
