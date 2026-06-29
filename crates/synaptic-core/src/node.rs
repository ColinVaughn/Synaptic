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
const DYNAMIC_SITES_KEY: &str = "dynamic_sites";
const DYNAMICALLY_REFERENCED_KEY: &str = "dynamically_referenced";
const TEST_KEY: &str = "_is_test";

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

    /// Dynamic-dispatch sites recorded on this node (empty if none).
    pub fn dynamic_sites(&self) -> Vec<crate::dynamic::DynamicSite> {
        self.extra
            .get(DYNAMIC_SITES_KEY)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    /// Append a dynamic-dispatch site to this node.
    pub fn push_dynamic_site(&mut self, site: crate::dynamic::DynamicSite) {
        let mut sites = self.dynamic_sites();
        sites.push(site);
        self.extra.insert(
            DYNAMIC_SITES_KEY.to_string(),
            serde_json::to_value(sites).expect("DynamicSite serializes"),
        );
    }

    /// True when an evidence-link resolved a dynamic site's key to this node, so its
    /// reverse-impact may be reachable only dynamically.
    pub fn dynamically_referenced(&self) -> bool {
        self.extra
            .get(DYNAMICALLY_REFERENCED_KEY)
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Mark/unmark this node as reached by a dynamic evidence-link.
    pub fn set_dynamically_referenced(&mut self, v: bool) {
        self.extra
            .insert(DYNAMICALLY_REFERENCED_KEY.to_string(), serde_json::json!(v));
    }

    /// True if the extractor marked this node as test code via a language test
    /// signal the path heuristic cannot see -- a Rust inline `#[test]` /
    /// `#[cfg(test)] mod tests` function in a `src/` file. Consulted by
    /// [`Self::is_test`].
    pub fn marked_test(&self) -> bool {
        self.extra
            .get(TEST_KEY)
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Mark this node as test code (set only when true, to keep `graph.json`
    /// terse for the common non-test case).
    pub fn set_test(&mut self, v: bool) {
        self.extra
            .insert(TEST_KEY.to_string(), serde_json::json!(v));
    }

    /// True if this node lives in test code: either the extractor marked it (an
    /// inline `#[test]` / `#[cfg(test)]` function -- see [`Self::marked_test`]) or
    /// its source path matches the test convention (see [`crate::is_test_path`]).
    pub fn is_test(&self) -> bool {
        self.marked_test() || crate::is_test_path(&self.source_file)
    }

    /// True if this node represents a code symbol eligible for change-impact
    /// analysis: it lives in real code (`FileType::Code`) and is not a docs or
    /// config artifact (markdown heading -> `FileType::Document`; JSON config key
    /// or YAML/k8s/CI resource -> a config `_node_type`). Keeps impact output
    /// focused on code rather than prose and configuration.
    pub fn is_code_symbol(&self) -> bool {
        self.file_type == FileType::Code
            && !matches!(
                self.extra.get("_node_type").and_then(|v| v.as_str()),
                Some("config_key" | "config_resource")
            )
    }

    /// True if this node is an external stub: an import target / third-party
    /// package with no definition in any scanned repo (empty `source_file`). These
    /// exist only to anchor cross-repo / import edges; they are not symbols that
    /// belong to a subsystem, so listings like community membership exclude them.
    pub fn is_external_stub(&self) -> bool {
        self.source_file.is_empty()
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
    fn dynamic_sites_push_and_read_roundtrip() {
        use crate::dynamic::{DynamicKind, DynamicSite};
        let mut n = sample();
        assert!(n.dynamic_sites().is_empty());
        assert!(!n.dynamically_referenced());
        n.push_dynamic_site(DynamicSite {
            kind: DynamicKind::Reflection,
            line: 3,
            key: Some("ready".into()),
            snippet: "o['ready']()".into(),
        });
        n.set_dynamically_referenced(true);
        assert_eq!(n.dynamic_sites().len(), 1);
        assert_eq!(n.dynamic_sites()[0].key.as_deref(), Some("ready"));
        assert!(n.dynamically_referenced());
        // survives a serde roundtrip via flattened extra
        let json = serde_json::to_value(&n).unwrap();
        let back: Node = serde_json::from_value(json).unwrap();
        assert_eq!(back.dynamic_sites().len(), 1);
        assert!(back.dynamically_referenced());
    }

    #[test]
    fn external_stub_is_a_node_with_no_source_file() {
        let mut n = sample();
        assert!(!n.is_external_stub(), "a located node is not a stub");
        n.source_file = String::new();
        assert!(
            n.is_external_stub(),
            "empty source_file marks an import stub"
        );
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
    fn is_test_consults_extraction_flag() {
        // An inline Rust unit test lives in a src/ file the path heuristic reads
        // as production code; the extraction flag must still mark it as a test.
        let mut n = sample();
        n.source_file = "crates/synaptic-graph/src/graph.rs".into();
        assert!(!n.is_test(), "src path alone is not a test");
        n.set_test(true);
        assert!(n.is_test(), "the extraction flag marks it as a test");
        assert!(n.marked_test());
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
