use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::file_type::FileType;
use crate::id::NodeId;
use crate::node_kind::{NodeKind, Visibility};
use crate::signature::Signature;
use crate::span::Span;

/// A graph node. The required fields are the ones in `REQUIRED_NODE_FIELDS`.
/// Optional fields are omitted from `graph.json` when unset so output stays in
/// the node-link format. `extra` captures any additional keys (`norm_label`,
/// `_origin`, `source_url`, …) so round-trips are lossless.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub label: String,
    pub file_type: FileType,
    pub source_file: String,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub community: Option<u32>,
    /// Federation namespace tag; absent for single-repo graphs.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub repo: Option<String>,

    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Keys used to carry the enrichment metadata inside `extra`. Stored there (not
/// as struct fields) so the ~80 existing `Node { .. }` construction sites stay
/// unchanged; the typed accessors below are the supported API. Because `extra`
/// is `#[serde(flatten)]`, these serialize to `graph.json` as plain top-level
/// node keys (`"kind"`, `"visibility"`, `"span"`), identical to typed fields,
/// and round-trip losslessly.
const KIND_KEY: &str = "kind";
const VISIBILITY_KEY: &str = "visibility";
const SPAN_KEY: &str = "span";
const SIGNATURE_KEY: &str = "signature";

impl Node {
    /// The node's kind (class/function/method/...), if the extractor set one.
    pub fn kind(&self) -> Option<NodeKind> {
        self.extra
            .get(KIND_KEY)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// Set the node's kind.
    pub fn set_kind(&mut self, kind: NodeKind) {
        self.extra.insert(
            KIND_KEY.to_string(),
            serde_json::to_value(kind).expect("NodeKind serializes"),
        );
    }

    /// The node's declared visibility, if known.
    pub fn visibility(&self) -> Option<Visibility> {
        self.extra
            .get(VISIBILITY_KEY)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// Set the node's visibility.
    pub fn set_visibility(&mut self, visibility: Visibility) {
        self.extra.insert(
            VISIBILITY_KEY.to_string(),
            serde_json::to_value(visibility).expect("Visibility serializes"),
        );
    }

    /// The node's source span, if the extractor captured one.
    pub fn span(&self) -> Option<Span> {
        self.extra
            .get(SPAN_KEY)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// Set the node's source span.
    pub fn set_span(&mut self, span: Span) {
        self.extra.insert(
            SPAN_KEY.to_string(),
            serde_json::to_value(span).expect("Span serializes"),
        );
    }

    /// Lines of code, derived from the span.
    pub fn loc(&self) -> Option<u32> {
        self.span().map(|s| s.line_count())
    }

    /// The node's captured signature (params + return type), if the extractor
    /// recorded one. Only set for function/method nodes whose grammar exposes
    /// parameters.
    pub fn signature(&self) -> Option<Signature> {
        self.extra
            .get(SIGNATURE_KEY)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// Set the node's signature.
    pub fn set_signature(&mut self, signature: Signature) {
        self.extra.insert(
            SIGNATURE_KEY.to_string(),
            serde_json::to_value(signature).expect("Signature serializes"),
        );
    }

    /// True if this node lives in test code (heuristic, by its source path; see
    /// [`crate::is_test_path`]).
    pub fn is_test(&self) -> bool {
        crate::is_test_path(&self.source_file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Node {
        Node {
            id: NodeId("auth".into()),
            label: "auth.py".into(),
            file_type: FileType::Code,
            source_file: "src/auth.py".into(),
            source_location: Some("L42".into()),
            community: None,
            repo: None,
            extra: Map::new(),
        }
    }

    #[test]
    fn omits_unset_optional_fields() {
        let json = serde_json::to_value(sample()).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.contains_key("source_location"));
        assert!(!obj.contains_key("community")); // None -> omitted
        assert!(!obj.contains_key("repo"));
        // Nodes carry no confidence key (confidence is an edge-level property).
        assert!(!obj.contains_key("confidence"));
    }

    #[test]
    fn required_keys_present_with_canonical_names() {
        let json = serde_json::to_value(sample()).unwrap();
        let obj = json.as_object().unwrap();
        for k in ["id", "label", "file_type", "source_file"] {
            assert!(obj.contains_key(k), "missing {k}");
        }
        assert_eq!(obj["file_type"], serde_json::json!("code"));
    }

    #[test]
    fn enrichment_accessors_roundtrip_and_omit_when_unset() {
        // Old-style node (no enrichment) reports None for all three.
        let n = sample();
        assert!(n.kind().is_none() && n.visibility().is_none() && n.span().is_none());
        assert!(n.loc().is_none());
        let obj = serde_json::to_value(&n).unwrap();
        assert!(!obj.as_object().unwrap().contains_key("kind"));
        assert!(!obj.as_object().unwrap().contains_key("span"));

        // Set enrichment, confirm it serializes as plain top-level keys.
        let mut e = sample();
        e.set_kind(NodeKind::Class);
        e.set_visibility(Visibility::Public);
        e.set_span(Span {
            start_line: 1,
            start_col: 1,
            end_line: 9,
            end_col: 2,
        });
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], serde_json::json!("class"));
        assert_eq!(v["visibility"], serde_json::json!("public"));
        assert_eq!(v["span"]["end_line"], serde_json::json!(9));

        // Round-trip back through serde restores the typed values.
        let back: Node = serde_json::from_value(v).unwrap();
        assert_eq!(back.kind(), Some(NodeKind::Class));
        assert_eq!(back.visibility(), Some(Visibility::Public));
        assert_eq!(back.loc(), Some(9));
    }

    #[test]
    fn signature_accessor_roundtrips_and_serializes_top_level() {
        use crate::signature::{Param, Signature};
        let mut n = sample();
        assert!(n.signature().is_none(), "unset signature reads as None");

        let sig = Signature {
            params: vec![
                Param {
                    name: "a".into(),
                    type_ref: Some("int".into()),
                },
                Param {
                    name: "b".into(),
                    type_ref: None,
                },
            ],
            return_type: Some("Result".into()),
            raw: "(a: int, b) -> Result".into(),
        };
        n.set_signature(sig.clone());
        assert_eq!(n.signature(), Some(sig));

        // Serializes as a plain top-level "signature" key (extra is flattened).
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["signature"]["params"][0]["name"], serde_json::json!("a"));
        assert_eq!(
            v["signature"]["params"][0]["type_ref"],
            serde_json::json!("int")
        );
        // An untyped param omits type_ref entirely.
        assert!(!v["signature"]["params"][1]
            .as_object()
            .unwrap()
            .contains_key("type_ref"));
        assert_eq!(v["signature"]["return_type"], serde_json::json!("Result"));

        // Round-trips back through serde to the typed value.
        let back: Node = serde_json::from_value(v).unwrap();
        assert_eq!(back.signature().unwrap().params.len(), 2);
        assert_eq!(back.signature().unwrap().raw, "(a: int, b) -> Result");
    }

    #[test]
    fn is_test_reflects_the_source_path() {
        let mut n = sample();
        assert!(!n.is_test(), "src/auth.py is production code");
        n.source_file = "tests/test_auth.py".into();
        assert!(n.is_test(), "a path under tests/ is test code");
    }

    #[test]
    fn unknown_keys_roundtrip_via_extra() {
        let raw = serde_json::json!({
            "id": "auth",
            "label": "auth.py",
            "file_type": "code",
            "source_file": "src/auth.py",
            "community": 3,
            "norm_label": "auth.py",
            "_origin": "ast"
        });
        let node: Node = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(node.community, Some(3));
        assert_eq!(node.extra.get("norm_label").unwrap(), "auth.py");
        assert_eq!(node.extra.get("_origin").unwrap(), "ast");
        // Re-serialize: typed + extra keys both present, no data lost.
        let back = serde_json::to_value(&node).unwrap();
        let obj = back.as_object().unwrap();
        assert_eq!(obj["community"], serde_json::json!(3));
        assert_eq!(obj["norm_label"], serde_json::json!("auth.py"));
        assert_eq!(obj["_origin"], serde_json::json!("ast"));
    }
}
