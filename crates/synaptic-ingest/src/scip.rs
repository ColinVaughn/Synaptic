//! SCIP JSON ingestion (simplified subset).
//!
//! Consumes a **simplified SCIP-style JSON** document (not the official
//! protobuf) and converts it into graph nodes + edges. The accepted shape:
//!
//! ```text
//! documents[]:     { relative_path, language, symbols[] }
//! symbols[]:       { symbol, kind, display_name, documentation[],
//!                    relationships[], occurrences[] }
//! relationships[]: { symbol, is_reference, is_implementation,
//!                    is_type_definition, is_definition }
//! occurrences[]:   { range[], symbol, symbol_roles }
//! ```
//!
//! Two-pass design:
//!   1. Build a `symbol → node_id` index across every valid symbol in every
//!      document (a per-document index for same-file precedence + a global
//!      index for unambiguous cross-file resolution).
//!   2. Emit one node per indexed symbol, then emit relationship edges.
//!      Relationship targets resolve via the index when present; otherwise a
//!      stub `scip_external` node is added so an edge never dangles.
//!
//! Everything here comes from an external index, so it is **untrusted**: every
//! label goes through [`sanitize_label`] and every metadata map through
//! [`sanitize_metadata`].
//!
//! Node ids hash with blake3 (design §10.1). Ids are internal identifiers, so
//! determinism — not the specific digest — is what matters; blake3 is already a
//! workspace dependency.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};
use synaptic_core::{sanitize_label, sanitize_metadata, Confidence, Edge, FileType, Node, NodeId};

use crate::Ingested;

/// A symbol gathered in pass 1, carried into pass 2 for node/edge emission.
struct SymbolRecord {
    node_id: String,
    symbol_id: String,
    doc_path: String,
    /// The raw symbol object (guaranteed to be a JSON object).
    raw: Value,
}

/// Convert a SCIP-style JSON document into nodes and edges. `doc` is arbitrary
/// deserialized JSON — anything not matching the simplified shape (or not an
/// object) yields an empty result. `source_file`/`language` are fallbacks for
/// documents that omit `relative_path`/`language`.
pub fn ingest_scip_json(doc: &Value, source_file: &str, language: &str) -> Ingested {
    let mut out = Ingested::default();
    let mut seen_node_ids: HashSet<String> = HashSet::new();
    let mut seen_edges: HashSet<(String, String, String, Option<String>)> = HashSet::new();

    let Some(obj) = doc.as_object() else {
        return out;
    };
    let Some(documents) = obj.get("documents").and_then(Value::as_array) else {
        return out;
    };

    // pass 1: build symbol -> node_id indices.
    //   per_doc: (symbol_id, doc_path) -> node_id  (same-document precedence)
    //   global:  symbol_id             -> [node_id] (unambiguous cross-doc only)
    let mut per_doc_index: HashMap<(String, String), String> = HashMap::new();
    let mut global_index: HashMap<String, Vec<String>> = HashMap::new();
    let mut records: Vec<SymbolRecord> = Vec::new();
    let _ = language; // currently only `relative_path`/symbol fields drive output
    for document in documents {
        let Some(doc_obj) = document.as_object() else {
            continue;
        };
        let doc_path = coerce_str(doc_obj.get("relative_path"), source_file);
        let Some(symbols) = doc_obj.get("symbols").and_then(Value::as_array) else {
            continue;
        };
        for symbol in symbols {
            let Some(sym_obj) = symbol.as_object() else {
                continue;
            };
            let symbol_id = coerce_str(sym_obj.get("symbol"), "");
            if symbol_id.is_empty() {
                continue;
            }
            let node_id = make_scip_node_id(&symbol_id, &doc_path);
            per_doc_index
                .entry((symbol_id.clone(), doc_path.clone()))
                .or_insert_with(|| node_id.clone());
            // Dedupe within the global index: a symbol repeated in the SAME
            // document yields the same node_id and must not look ambiguous.
            let candidates = global_index.entry(symbol_id.clone()).or_default();
            if !candidates.contains(&node_id) {
                candidates.push(node_id.clone());
            }
            records.push(SymbolRecord {
                node_id,
                symbol_id,
                doc_path: doc_path.clone(),
                raw: symbol.clone(),
            });
        }
    }

    // pass 2: emit nodes + relationship edges.
    for record in &records {
        emit_symbol_node(record, &mut out.nodes, &mut seen_node_ids);
        emit_relationships(
            record,
            &per_doc_index,
            &global_index,
            &mut out.nodes,
            &mut out.edges,
            &mut seen_node_ids,
            &mut seen_edges,
        );
    }
    out
}

/// Append the canonical node for a SCIP symbol record (once per node id).
fn emit_symbol_node(record: &SymbolRecord, nodes: &mut Vec<Node>, seen: &mut HashSet<String>) {
    if seen.contains(&record.node_id) {
        return;
    }
    let Some(raw) = record.raw.as_object() else {
        return;
    };
    let kind = coerce_str(raw.get("kind"), "unknown");
    let display_name = coerce_str(raw.get("display_name"), "");
    let description = raw
        .get("documentation")
        .and_then(Value::as_array)
        .and_then(|d| d.first())
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let sourceline = first_occurrence_line(raw.get("occurrences"));
    let suffix = label_suffix(&record.symbol_id);
    let label = if !display_name.is_empty() {
        display_name
    } else if !suffix.is_empty() {
        suffix.to_string()
    } else {
        record.symbol_id.clone()
    };
    seen.insert(record.node_id.clone());
    nodes.push(make_scip_node(
        &record.node_id,
        &label,
        &record.doc_path,
        sourceline,
        scip_metadata(&record.symbol_id, &kind, &description),
    ));
}

/// Append edges (and stub nodes when needed) for a symbol's relationships.
#[allow(clippy::too_many_arguments)]
fn emit_relationships(
    record: &SymbolRecord,
    per_doc_index: &HashMap<(String, String), String>,
    global_index: &HashMap<String, Vec<String>>,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    seen_nodes: &mut HashSet<String>,
    seen_edges: &mut HashSet<(String, String, String, Option<String>)>,
) {
    let Some(raw) = record.raw.as_object() else {
        return;
    };
    let sourceline = first_occurrence_line(raw.get("occurrences"));
    let Some(relationships) = raw.get("relationships").and_then(Value::as_array) else {
        return;
    };
    for rel in relationships {
        let Some(rel_obj) = rel.as_object() else {
            continue;
        };
        let target_symbol = coerce_str(rel_obj.get("symbol"), "");
        if target_symbol.is_empty() {
            continue;
        }
        let target_node_id = match resolve_target(
            &target_symbol,
            &record.doc_path,
            per_doc_index,
            global_index,
        ) {
            Some(t) => t,
            None => {
                // External / ambiguous target: emit a stub so the edge keeps
                // a live endpoint, hosted under the source document's path.
                let stub_id = make_scip_node_id(&target_symbol, &record.doc_path);
                if !seen_nodes.contains(&stub_id) {
                    seen_nodes.insert(stub_id.clone());
                    let suffix = label_suffix(&target_symbol);
                    let label = if suffix.is_empty() {
                        target_symbol.as_str()
                    } else {
                        suffix
                    };
                    nodes.push(make_scip_node(
                        &stub_id,
                        label,
                        &record.doc_path,
                        0,
                        scip_metadata(&target_symbol, "external", ""),
                    ));
                }
                stub_id
            }
        };
        let relation = relation_for(rel_obj);
        let source_location = if sourceline > 0 {
            Some(format!("L{sourceline}"))
        } else {
            None
        };
        let key = (
            record.node_id.clone(),
            target_node_id.clone(),
            relation.to_string(),
            source_location.clone(),
        );
        if seen_edges.contains(&key) {
            continue;
        }
        seen_edges.insert(key);
        let mut meta = Map::new();
        meta.insert("scip_relationship".into(), rel.clone());
        let mut extra = Map::new();
        extra.insert("metadata".into(), Value::Object(sanitize_metadata(&meta)));
        edges.push(Edge {
            source: NodeId(record.node_id.clone()),
            target: NodeId(target_node_id),
            relation: relation.to_string(),
            confidence: Confidence::Extracted,
            confidence_score: Some(1.0),
            source_file: record.doc_path.clone(),
            source_location,
            weight: 1.0,
            context: Some("scip".to_string()),
            cross_repo: false,
            extra,
        });
    }
}

/// Resolve a relationship target to an emitted node id, or `None`.
///
/// Order: same-document `(symbol, doc)` → unique cross-document → `None`
/// (absent OR ambiguous — refuse to guess; the caller stubs it).
fn resolve_target(
    target_symbol: &str,
    source_doc_path: &str,
    per_doc_index: &HashMap<(String, String), String>,
    global_index: &HashMap<String, Vec<String>>,
) -> Option<String> {
    if let Some(t) = per_doc_index.get(&(target_symbol.to_string(), source_doc_path.to_string())) {
        return Some(t.clone());
    }
    match global_index.get(target_symbol) {
        Some(c) if c.len() == 1 => Some(c[0].clone()),
        _ => None,
    }
}

/// Pick the graph relation for a SCIP relationship dict. A flag counts only
/// when its value is exactly boolean `true` (guards against `"false"` strings
/// in untrusted JSON). Precedence: impl > type-def > def > ref.
fn relation_for(rel: &Map<String, Value>) -> &'static str {
    let is_true = |k: &str| rel.get(k) == Some(&Value::Bool(true));
    if is_true("is_implementation") {
        "scip_impl"
    } else if is_true("is_type_definition") {
        "scip_typed"
    } else if is_true("is_definition") {
        "scip_def"
    } else {
        "scip_ref"
    }
}

/// 1-based line number from the first occurrence's range, defensively (a
/// non-array, empty, or non-integer range yields 0). `Value::Bool` is a
/// distinct variant from `Value::Number`, so a `[true, …]` range can't slip
/// through as a line number.
fn first_occurrence_line(occurrences: Option<&Value>) -> i64 {
    let Some(arr) = occurrences.and_then(Value::as_array) else {
        return 0;
    };
    let Some(first) = arr.first().and_then(Value::as_object) else {
        return 0;
    };
    let Some(rng) = first.get("range").and_then(Value::as_array) else {
        return 0;
    };
    match rng.first().and_then(Value::as_i64) {
        Some(n) if n >= 0 => n,
        _ => 0,
    }
}

/// `value` if it's a string, else `default`.
fn coerce_str(value: Option<&Value>, default: &str) -> String {
    value.and_then(Value::as_str).unwrap_or(default).to_string()
}

/// Stable node id from a SCIP symbol: `scip_<sanitized-suffix>_<hex12>` (or
/// `scip_<hex12>` when the suffix sanitizes away). The hash is over
/// `"{source_file}:{symbol}"`, so the same symbol in different files gets
/// distinct ids.
fn make_scip_node_id(symbol: &str, source_file: &str) -> String {
    let raw = format!("{source_file}:{symbol}");
    let hex = blake3::hash(raw.as_bytes()).to_hex();
    let h12 = &hex[..12];
    let suffix = id_suffix(symbol);
    if suffix.is_empty() {
        format!("scip_{h12}")
    } else {
        format!("scip_{suffix}_{h12}")
    }
}

/// Id-safe suffix: last `#`-segment, non-`[A-Za-z0-9_]` → `_`, trimmed, lower.
fn id_suffix(symbol: &str) -> String {
    let last = symbol.rsplit('#').next().unwrap_or(symbol);
    let replaced: String = last
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    replaced.trim_matches('_').to_ascii_lowercase()
}

/// Human label suffix: the raw last `#`-segment (or the whole symbol).
fn label_suffix(symbol: &str) -> &str {
    symbol.rsplit('#').next().unwrap_or(symbol)
}

/// Build a SCIP node: `file_type=code`, label + metadata both sanitized,
/// `source_location` omitted when there's no occurrence line.
fn make_scip_node(
    id: &str,
    label: &str,
    source_file: &str,
    line: i64,
    meta: Map<String, Value>,
) -> Node {
    let mut extra = Map::new();
    extra.insert("metadata".into(), Value::Object(sanitize_metadata(&meta)));
    Node {
        id: NodeId(id.to_string()),
        label: sanitize_label(label),
        file_type: FileType::Code,
        source_file: source_file.to_string(),
        source_location: if line > 0 {
            Some(format!("L{line}"))
        } else {
            None
        },
        community: None,
        repo: None,
        extra,
    }
}

/// `{scip_symbol, scip_kind[, scip_description]}`.
fn scip_metadata(symbol_id: &str, kind: &str, description: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("scip_symbol".into(), json!(symbol_id));
    m.insert("scip_kind".into(), json!(kind));
    if !description.is_empty() {
        m.insert("scip_description".into(), json!(description));
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(documents: Value) -> Value {
        json!({ "documents": documents })
    }

    #[test]
    fn non_object_or_missing_documents_is_empty() {
        assert_eq!(
            ingest_scip_json(&json!("nope"), "", "python"),
            Ingested::default()
        );
        assert_eq!(
            ingest_scip_json(&json!({}), "", "python"),
            Ingested::default()
        );
        assert_eq!(
            ingest_scip_json(&json!({ "documents": "bad" }), "", "python"),
            Ingested::default()
        );
    }

    #[test]
    fn same_document_relationship_links_locally_no_stub() {
        // Two symbols in one file; A references B (defined in the same file).
        let d = doc(json!([{
            "relative_path": "a.py",
            "symbols": [
                { "symbol": "a.py:A#", "display_name": "A",
                  "relationships": [{ "symbol": "a.py:B#", "is_reference": true }] },
                { "symbol": "a.py:B#", "display_name": "B" }
            ]
        }]));
        let out = ingest_scip_json(&d, "", "python");
        // Two real nodes, no stub.
        assert_eq!(out.nodes.len(), 2, "{:?}", out.nodes);
        assert_eq!(out.edges.len(), 1);
        let e = &out.edges[0];
        assert_eq!(e.relation, "scip_ref");
        assert_eq!(e.context.as_deref(), Some("scip"));
        let b_id = make_scip_node_id("a.py:B#", "a.py");
        assert_eq!(e.target.0, b_id, "edge resolves to the in-file B node");
        assert!(out
            .nodes
            .iter()
            .all(|n| n.extra["metadata"]["scip_kind"] != json!("external")));
    }

    #[test]
    fn unique_cross_document_resolves_ambiguous_stubs() {
        // util.helper defined once resolves; dup.thing defined twice stubs.
        let d = doc(json!([
            { "relative_path": "main.py", "symbols": [{
                "symbol": "main.py:run#",
                "relationships": [
                    { "symbol": "util.py:helper#", "is_definition": true },
                    { "symbol": "dup:thing#", "is_implementation": true }
                ]
            }]},
            { "relative_path": "util.py", "symbols": [{ "symbol": "util.py:helper#" }] },
            { "relative_path": "d1.py", "symbols": [{ "symbol": "dup:thing#" }] },
            { "relative_path": "d2.py", "symbols": [{ "symbol": "dup:thing#" }] }
        ]));
        let out = ingest_scip_json(&d, "", "python");
        // helper resolves to util.py's node; thing is ambiguous, so it stubs in main.py.
        let helper_id = make_scip_node_id("util.py:helper#", "util.py");
        let thing_stub = make_scip_node_id("dup:thing#", "main.py");
        let def_edge = out.edges.iter().find(|e| e.relation == "scip_def").unwrap();
        assert_eq!(def_edge.target.0, helper_id);
        let impl_edge = out
            .edges
            .iter()
            .find(|e| e.relation == "scip_impl")
            .unwrap();
        assert_eq!(impl_edge.target.0, thing_stub);
        let stub = out.nodes.iter().find(|n| n.id.0 == thing_stub).unwrap();
        assert_eq!(stub.extra["metadata"]["scip_kind"], json!("external"));
    }

    #[test]
    fn absent_target_stubs_external_and_keeps_edge() {
        let d = doc(json!([{
            "relative_path": "x.py",
            "symbols": [{
                "symbol": "x.py:f#",
                "display_name": "f",
                "occurrences": [{ "range": [12, 0, 12, 4] }],
                "relationships": [{ "symbol": "extern:lib#g", "is_reference": true }]
            }]
        }]));
        let out = ingest_scip_json(&d, "", "python");
        assert_eq!(out.nodes.len(), 2, "real f + stub g");
        let stub = out.nodes.iter().find(|n| n.label == "g").unwrap();
        assert_eq!(stub.extra["metadata"]["scip_kind"], json!("external"));
        assert!(
            stub.source_location.is_none(),
            "stub has no occurrence line"
        );
        // The source node carries its occurrence line; so does its out-edge.
        let f = out.nodes.iter().find(|n| n.label == "f").unwrap();
        assert_eq!(f.source_location.as_deref(), Some("L12"));
        assert_eq!(out.edges[0].source_location.as_deref(), Some("L12"));
    }

    #[test]
    fn relation_precedence_and_strict_bool() {
        let mk = |rel: Value| {
            let d = doc(json!([{
                "relative_path": "a.py",
                "symbols": [{ "symbol": "a.py:A#", "relationships": [rel] }]
            }]));
            ingest_scip_json(&d, "", "python").edges[0].relation.clone()
        };
        // impl wins over the rest even when several flags are set.
        assert_eq!(
            mk(json!({ "symbol": "z#", "is_definition": true, "is_implementation": true })),
            "scip_impl"
        );
        assert_eq!(
            mk(json!({ "symbol": "z#", "is_type_definition": true })),
            "scip_typed"
        );
        assert_eq!(
            mk(json!({ "symbol": "z#", "is_definition": true })),
            "scip_def"
        );
        // A truthy string is not the boolean true, so it falls through to ref.
        assert_eq!(
            mk(json!({ "symbol": "z#", "is_definition": "false" })),
            "scip_ref"
        );
        assert_eq!(mk(json!({ "symbol": "z#" })), "scip_ref");
    }

    #[test]
    fn metadata_is_sanitized_and_edges_dedupe() {
        let d = doc(json!([{
            "relative_path": "a.py",
            "symbols": [{
                "symbol": "a.py:A#",
                "documentation": ["Reads <config> & runs"],
                "relationships": [
                    { "symbol": "a.py:A#", "is_reference": true },
                    { "symbol": "a.py:A#", "is_reference": true }
                ]
            }]
        }]));
        let out = ingest_scip_json(&d, "", "python");
        // Self-referential dup relationship collapses to one edge.
        assert_eq!(out.edges.len(), 1);
        // Docstring HTML escaped inside metadata.
        let a = &out.nodes[0];
        assert_eq!(
            a.extra["metadata"]["scip_description"],
            json!("Reads &lt;config&gt; &amp; runs")
        );
    }

    #[test]
    fn node_ids_are_deterministic_and_file_scoped() {
        assert_eq!(
            make_scip_node_id("p:A#run().", "f.py"),
            make_scip_node_id("p:A#run().", "f.py")
        );
        assert_ne!(
            make_scip_node_id("p:A#run().", "f.py"),
            make_scip_node_id("p:A#run().", "g.py")
        );
        assert!(make_scip_node_id("p:A#run().", "f.py").starts_with("scip_run_"));
        // Suffix that sanitizes to empty gives a bare hash id.
        assert!(make_scip_node_id("p:###", "f.py").starts_with("scip_"));
        assert!(!make_scip_node_id("p:###", "f.py").starts_with("scip__"));
    }
}

#[cfg(test)]
mod fuzz {
    use proptest::prelude::*;

    /// Non-object entries in a document's `symbols` array are skipped, not
    /// panicked on (regression for the `as_object().unwrap()` that the audit
    /// replaced with a `let Some(..) else` skip).
    #[test]
    fn skips_non_object_symbols() {
        let doc = serde_json::json!({
            "documents": [{
                "relative_path": "a.py",
                "symbols": [42, "str", null, [1, 2], { "symbol": "scip ok#" }]
            }]
        });
        let ing = super::ingest_scip_json(&doc, "", "python");
        // The four malformed entries are skipped without panicking, and the one
        // valid symbol still produces exactly one node (guards against both a
        // panic and accidentally dropping valid symbols).
        assert_eq!(ing.nodes.len(), 1);
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// Any string that happens to parse as JSON must not panic the ingester.
        #[test]
        fn ingest_scip_never_panics(s in ".{0,2048}") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                let _ = super::ingest_scip_json(&v, "", "python");
            }
        }

        /// Arbitrary JSON object shapes for `documents` likewise never panic.
        #[test]
        fn ingest_scip_arbitrary_docs_never_panics(n in 0u64..1000) {
            let doc = serde_json::json!({ "documents": n });
            let _ = super::ingest_scip_json(&doc, "", "python");
        }
    }
}
