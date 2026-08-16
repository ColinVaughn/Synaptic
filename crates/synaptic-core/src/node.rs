use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::file_type::FileType;
use crate::id::NodeId;
use crate::node_kind::{KindValue, NodeKind, Origin, OriginKind, Visibility};
use crate::signature::Signature;
use crate::span::Span;

/// A graph node. The required fields are the ones in `REQUIRED_NODE_FIELDS`.
/// Optional fields are omitted from `graph.json` when unset so output stays in
/// the node-link format. `extra` captures any additional keys (`_node_type`,
/// `source_url`, …) so round-trips are lossless. The one exception is
/// `norm_label`, which the writer derives from `label` on export and the reader
/// discards again; see `DERIVED_NORM_LABEL_KEY`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub label: String,
    pub file_type: FileType,
    pub source_file: crate::Interned,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub community: Option<u32>,
    /// Federation namespace tag; absent for single-repo graphs.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub repo: Option<String>,

    // Hot metadata, held as typed fields rather than `serde_json::Value`s inside
    // `extra`. Measured at 2,220 bytes/node when these lived in the map -- 51% of
    // all node memory -- because `serde_json::Map` is a `BTreeMap` whose leaf is
    // a fixed 11-slot array (~616 B) however few keys it holds, and every nested
    // object (`span`, `signature`, and each `param`) allocated its own. A 16-byte
    // `Span` cost 698 B, a 44x amplification.
    //
    // Read and written through the accessors below, never directly, so the
    // storage stays an implementation detail. They serialize as plain top-level
    // keys and are omitted when unset, exactly as the `extra`-backed form did:
    // `graph.json` is byte-identical.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub kind: Option<KindValue>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub visibility: Option<Visibility>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub span: Option<Span>,
    /// Boxed: a `Signature` is 72 bytes and only ~46% of nodes carry one, so
    /// inlining it charged every node for the majority that has none.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub signature: Option<Box<Signature>>,
    /// `_is_test`: set only by extractors that see a language test signal the
    /// path heuristic cannot (an inline Rust `#[test]`). `Option<bool>` rather
    /// than `bool` so an explicit `false` round-trips instead of being silently
    /// dropped on re-serialization; the niche makes it 1 byte either way.
    #[serde(rename = "_is_test", skip_serializing_if = "Option::is_none", default)]
    pub marked_test: Option<bool>,
    /// `_origin`: which layer produced this node. See [`Origin`].
    #[serde(rename = "_origin", skip_serializing_if = "Option::is_none", default)]
    pub origin: Option<Origin>,

    #[serde(flatten, deserialize_with = "deserialize_extra")]
    pub extra: Map<String, Value>,
}

/// `norm_label`: a lowercased copy of `label` that the JSON writer adds on export
/// (see `wiki/Output-Formats.md`). Nothing reads it back -- every consumer that
/// wants a search key recomputes one from `label` -- but because it is present on
/// every node, materializing it hands every node a one-key `BTreeMap`, and the
/// smallest `BTreeMap` allocation is a fixed ~630-byte 11-slot leaf regardless of
/// how few keys it holds. On a 569k-node graph that is 342 MiB of scaffolding
/// allocated only to be dropped again by `KnowledgeGraph::from_graph_data`.
const DERIVED_NORM_LABEL_KEY: &str = "norm_label";

/// Deserialize the flattened metadata map, skipping keys that the export layer
/// derives from typed fields. The value is consumed as `IgnoredAny` so a skipped
/// key costs no allocation at all, and the map stays empty (and therefore
/// allocation-free) for the common node that carries nothing else.
///
/// This is a read-side filter only: [`crate::Node`] still serializes whatever is
/// in `extra`, and the JSON writer still emits `norm_label`, so `graph.json` is
/// byte-identical across a load/store round trip.
fn deserialize_extra<'de, D>(deserializer: D) -> Result<Map<String, Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ExtraVisitor;

    impl<'de> serde::de::Visitor<'de> for ExtraVisitor {
        type Value = Map<String, Value>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a map of additional node metadata")
        }

        fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut out = Map::new();
            while let Some(key) = access.next_key::<String>()? {
                if key == DERIVED_NORM_LABEL_KEY {
                    access.next_value::<serde::de::IgnoredAny>()?;
                    continue;
                }
                let value = access.next_value()?;
                out.insert(key, value);
            }
            Ok(out)
        }
    }

    deserializer.deserialize_map(ExtraVisitor)
}

/// Keys for the metadata still carried inside `extra`. Because `extra` is
/// `#[serde(flatten)]`, these serialize to `graph.json` as plain top-level node
/// keys, identical to the typed fields above, and round-trip losslessly. The
/// hot four (`kind`, `visibility`, `span`, `signature`) graduated to typed
/// fields; these are scalars or rare, so the map is the cheaper home.
const DYNAMIC_SITES_KEY: &str = "dynamic_sites";
const DYNAMICALLY_REFERENCED_KEY: &str = "dynamically_referenced";

impl Node {
    /// The node's kind (class/function/method/...), if the extractor set one.
    ///
    /// `None` when the key is absent *or* carries another layer's vocabulary
    /// (see [`KindValue`]) -- matching the old `extra`-backed accessor, which
    /// dropped anything that failed to parse as a `NodeKind`.
    pub fn kind(&self) -> Option<NodeKind> {
        match &self.kind {
            Some(KindValue::Known(k)) => Some(*k),
            _ => None,
        }
    }

    /// Set the node's kind.
    pub fn set_kind(&mut self, kind: NodeKind) {
        self.kind = Some(KindValue::Known(kind));
    }

    /// The node's declared visibility, if known.
    pub fn visibility(&self) -> Option<Visibility> {
        self.visibility
    }

    /// Set the node's visibility.
    pub fn set_visibility(&mut self, visibility: Visibility) {
        self.visibility = Some(visibility);
    }

    /// The node's source span, if the extractor captured one.
    pub fn span(&self) -> Option<Span> {
        self.span
    }

    /// Set the node's source span.
    pub fn set_span(&mut self, span: Span) {
        self.span = Some(span);
    }

    /// The node's `_origin` tag, if set.
    pub fn origin(&self) -> Option<&str> {
        self.origin.as_ref().map(Origin::as_str)
    }

    /// Set the node's `_origin` tag.
    pub fn set_origin(&mut self, origin: &str) {
        self.origin = Some(Origin::from(origin));
    }

    /// True when this node came from a parsed syntax tree. Load-bearing for
    /// ghost-remap, which prefers AST nodes over synthesized duplicates.
    pub fn is_ast_origin(&self) -> bool {
        matches!(self.origin, Some(Origin::Known(OriginKind::Ast)))
    }

    /// Lines of code, derived from the span.
    pub fn loc(&self) -> Option<u32> {
        self.span().map(|s| s.line_count())
    }

    /// The node's captured signature (params + return type), if the extractor
    /// recorded one. Only set for function/method nodes whose grammar exposes
    /// parameters.
    pub fn signature(&self) -> Option<Signature> {
        self.signature.as_deref().cloned()
    }

    /// Set the node's signature.
    pub fn set_signature(&mut self, signature: Signature) {
        self.signature = Some(Box::new(signature));
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
        self.marked_test.unwrap_or(false)
    }

    /// Mark this node as test code (set only when true, to keep `graph.json`
    /// terse for the common non-test case).
    pub fn set_test(&mut self, v: bool) {
        self.marked_test = Some(v);
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
            ..Default::default()
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
        n.source_file = String::new().into();
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
        assert!(
            !v["signature"]["params"][1]
                .as_object()
                .unwrap()
                .contains_key("type_ref")
        );
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

    /// The hot metadata must serialize as PLAIN TOP-LEVEL KEYS and be absent
    /// when unset, whether it is stored in `extra` or in typed fields. This is
    /// the whole contract of the typed-field migration: `graph.json` may not
    /// move by a byte.
    #[test]
    fn hot_metadata_serializes_as_top_level_keys() {
        use crate::signature::{Param, Signature};

        let mut n = sample();
        n.set_kind(NodeKind::Class);
        n.set_visibility(Visibility::Public);
        n.set_span(Span {
            start_line: 1,
            start_col: 1,
            end_line: 9,
            end_col: 2,
        });
        n.set_signature(Signature {
            params: vec![Param {
                name: "a".into(),
                type_ref: Some("int".into()),
            }],
            return_type: Some("Result".into()),
            raw: "(a: int) -> Result".into(),
        });

        let v = serde_json::to_value(&n).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj["kind"], serde_json::json!("class"));
        assert_eq!(obj["visibility"], serde_json::json!("public"));
        assert_eq!(obj["span"]["end_line"], serde_json::json!(9));
        assert_eq!(
            obj["signature"]["params"][0]["name"],
            serde_json::json!("a")
        );
        assert_eq!(obj["signature"]["return_type"], serde_json::json!("Result"));
        assert!(!obj.contains_key("extra"), "extra stays flattened");

        let back: Node = serde_json::from_value(v).unwrap();
        assert_eq!(back.kind(), Some(NodeKind::Class));
        assert_eq!(back.visibility(), Some(Visibility::Public));
        assert_eq!(back.loc(), Some(9));
        assert_eq!(back.signature().unwrap().raw, "(a: int) -> Result");
    }

    /// Unset hot metadata is omitted entirely, so a plain node's JSON is
    /// unchanged by the migration.
    #[test]
    fn unset_hot_metadata_is_omitted() {
        let v = serde_json::to_value(sample()).unwrap();
        let obj = v.as_object().unwrap();
        for key in ["kind", "visibility", "span", "signature"] {
            assert!(!obj.contains_key(key), "{key} omitted when unset");
        }
    }

    /// A `graph.json` written before the migration (metadata arriving through
    /// the flattened `extra` map) must still load into the typed accessors, and
    /// genuinely unknown keys must still land in `extra`.
    #[test]
    fn legacy_extra_backed_nodes_still_deserialize() {
        let raw = serde_json::json!({
            "id": "auth",
            "label": "auth.py",
            "file_type": "code",
            "source_file": "src/auth.py",
            "kind": "function",
            "visibility": "public",
            "span": {"start_line": 1, "start_col": 1, "end_line": 4, "end_col": 2},
            "_origin": "ast",
            "source_url": "https://example.invalid/auth"
        });
        let node: Node = serde_json::from_value(raw).unwrap();
        assert_eq!(node.kind(), Some(NodeKind::Function));
        assert_eq!(node.visibility(), Some(Visibility::Public));
        assert_eq!(node.loc(), Some(4));
        assert_eq!(node.origin(), Some("ast"));
        assert_eq!(
            node.extra.get("source_url").unwrap(),
            "https://example.invalid/auth"
        );
        // Hot keys are consumed by their fields, not left duplicated in extra.
        assert!(!node.extra.contains_key("kind"));
        assert!(!node.extra.contains_key("span"));
    }

    /// `_is_test` moves to a typed field but keeps its exact JSON shape: the key
    /// is `_is_test`, it is omitted when unset, and an explicit `false` (which
    /// `set_test(false)` can produce) round-trips rather than being dropped.
    #[test]
    fn marked_test_is_typed_and_keeps_its_json_shape() {
        let plain = sample();
        let v = serde_json::to_value(&plain).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("_is_test"),
            "omitted when unset"
        );
        assert!(!plain.marked_test());

        let mut flagged = sample();
        flagged.set_test(true);
        let v = serde_json::to_value(&flagged).unwrap();
        assert_eq!(v["_is_test"], serde_json::json!(true));
        assert!(!v.as_object().unwrap().contains_key("extra"));
        let back: Node = serde_json::from_value(v).unwrap();
        assert!(back.marked_test());
        assert!(
            !back.extra.contains_key("_is_test"),
            "consumed by the field"
        );

        let mut explicit_false = sample();
        explicit_false.set_test(false);
        let v = serde_json::to_value(&explicit_false).unwrap();
        assert_eq!(
            v["_is_test"],
            serde_json::json!(false),
            "false is preserved"
        );
        let back: Node = serde_json::from_value(v).unwrap();
        assert!(!back.marked_test());
    }

    /// A node with no metadata beyond the typed fields must carry an EMPTY extra
    /// map -- that is the whole point: an empty BTreeMap allocates nothing, while
    /// one holding a single key costs a fixed ~632-byte 11-slot leaf.
    #[test]
    fn a_plain_ast_node_has_an_empty_extra_map() {
        let raw = serde_json::json!({
            "id": "auth", "label": "run()", "file_type": "code",
            "source_file": "src/auth.py", "kind": "function",
            "_origin": "ast", "_is_test": true
        });
        let node: Node = serde_json::from_value(raw).unwrap();
        assert!(
            node.extra.is_empty(),
            "extra should be empty, held: {:?}",
            node.extra
        );
    }

    /// `norm_label` is a lowercased copy of `label` that the JSON writer adds on
    /// export and no reader consumes. Materializing it into `extra` gives every
    /// node in the graph a one-key `BTreeMap`, whose smallest allocation is a
    /// fixed ~630-byte 11-slot leaf. Dropping it during deserialization is what
    /// lets a plain code node keep the empty map that costs nothing.
    #[test]
    fn norm_label_is_dropped_during_deserialization() {
        let raw = serde_json::json!({
            "id": "auth",
            "label": "Auth.py",
            "file_type": "code",
            "source_file": "src/auth.py",
            "norm_label": "auth.py"
        });
        let node: Node = serde_json::from_value(raw).unwrap();
        assert!(
            node.extra.is_empty(),
            "norm_label must not reach extra, held: {:?}",
            node.extra
        );
    }

    /// Dropping `norm_label` must not disturb genuinely unknown keys sharing the
    /// same flattened map.
    #[test]
    fn dropping_norm_label_keeps_other_unknown_keys() {
        let raw = serde_json::json!({
            "id": "auth",
            "label": "auth.py",
            "file_type": "code",
            "source_file": "src/auth.py",
            "norm_label": "auth.py",
            "source_url": "https://example.invalid/auth"
        });
        let node: Node = serde_json::from_value(raw).unwrap();
        assert!(!node.extra.contains_key("norm_label"));
        assert_eq!(
            node.extra.get("source_url").unwrap(),
            "https://example.invalid/auth"
        );
    }

    #[test]
    fn unknown_keys_roundtrip_via_extra() {
        let raw = serde_json::json!({
            "id": "auth",
            "label": "auth.py",
            "file_type": "code",
            "source_file": "src/auth.py",
            "community": 3,
            "_origin": "ast",
            "source_url": "https://example.invalid/auth"
        });
        let node: Node = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(node.community, Some(3));
        assert_eq!(node.origin(), Some("ast"));
        // Genuinely unknown keys land in `extra` and survive the round trip.
        assert_eq!(
            node.extra.get("source_url").unwrap(),
            "https://example.invalid/auth"
        );

        let back = serde_json::to_value(&node).unwrap();
        let obj = back.as_object().unwrap();
        assert_eq!(obj["community"], serde_json::json!(3));
        assert_eq!(obj["_origin"], serde_json::json!("ast"));
        assert_eq!(
            obj["source_url"],
            serde_json::json!("https://example.invalid/auth")
        );
    }
}
