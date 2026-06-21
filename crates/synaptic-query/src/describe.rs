//! Human-readable node descriptions for tool routing.
//!
//! Composes a node's captured signature with its outgoing call targets into a
//! compact "takes X, returns Y, calls Z" summary, and also returns the
//! structured pieces so a caller can use either form. Graph-only: no source
//! read. Built to feed the MCP `describe_node` tool and tool-description
//! generation.

use std::collections::HashSet;

use synaptic_core::{NodeId, Signature};
use synaptic_graph::KnowledgeGraph;

/// Outgoing relations that count as "calls" in a description summary. Includes
/// the cross-language `invokes`/`calls_service` relations (Track B) so the
/// summary stays accurate once those edges exist.
const CALL_RELATIONS: &[&str] = &["calls", "invokes", "calls_service"];

/// Cap on callees listed in the one-line summary, so it stays compact enough for
/// model routing. The full list is always available in [`NodeDescription::callees`].
const MAX_SUMMARY_CALLEES: usize = 8;

/// A composed, graph-only description of a node: its signature, distinct call
/// targets, and a compact "takes X, returns Y, calls Z" summary string.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeDescription {
    pub id: NodeId,
    pub label: String,
    pub kind: Option<String>,
    pub signature: Option<Signature>,
    pub callees: Vec<String>,
    pub summary: String,
}

/// Describe `id`. Returns `None` if no such node exists in `kg`.
pub fn describe_node(kg: &KnowledgeGraph, id: &NodeId) -> Option<NodeDescription> {
    let node = kg.node(id)?;
    let kind = node.kind().map(|k| k.as_str().to_string());
    let signature = node.signature();

    // Distinct outgoing call targets, by label, sorted for deterministic output.
    let mut seen = HashSet::new();
    let mut callees: Vec<String> = Vec::new();
    for e in kg.incident_edges(id) {
        if &e.source == id && CALL_RELATIONS.contains(&e.relation.as_str()) {
            let label = kg
                .node(&e.target)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| e.target.0.clone());
            if seen.insert(label.clone()) {
                callees.push(label);
            }
        }
    }
    callees.sort();

    let summary = summarize(&node.label, kind.as_deref(), signature.as_ref(), &callees);
    Some(NodeDescription {
        id: id.clone(),
        label: node.label.clone(),
        kind,
        signature,
        callees,
        summary,
    })
}

fn summarize(
    label: &str,
    kind: Option<&str>,
    signature: Option<&Signature>,
    callees: &[String],
) -> String {
    let head = match kind {
        Some(k) => format!("{k} {label}"),
        None => label.to_string(),
    };
    let mut parts: Vec<String> = Vec::new();

    if let Some(sig) = signature {
        let takes = if sig.params.is_empty() {
            "takes no arguments".to_string()
        } else {
            let ps: Vec<String> = sig
                .params
                .iter()
                .map(|p| match &p.type_ref {
                    Some(t) => format!("{}: {}", p.name, t),
                    None => p.name.clone(),
                })
                .collect();
            format!("takes ({})", ps.join(", "))
        };
        parts.push(takes);
        if let Some(rt) = &sig.return_type {
            parts.push(format!("returns {rt}"));
        }
    }

    if !callees.is_empty() {
        let shown = callees
            .iter()
            .take(MAX_SUMMARY_CALLEES)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let mut c = format!("calls [{shown}]");
        if callees.len() > MAX_SUMMARY_CALLEES {
            c.push_str(&format!(" (+{} more)", callees.len() - MAX_SUMMARY_CALLEES));
        }
        parts.push(c);
    }

    if parts.is_empty() {
        head
    } else {
        format!("{head} {}", parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Edge, GraphData, Node, NodeId, NodeKind, Param, Signature};
    use synaptic_graph::KnowledgeGraph;

    fn fn_node(id: &str, label: &str, sig: Option<Signature>) -> Node {
        let mut n = Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: synaptic_core::FileType::Code,
            source_file: "m.py".into(),
            source_location: Some("L1".into()),
            community: None,
            repo: None,
            extra: Map::new(),
        };
        n.set_kind(NodeKind::Function);
        if let Some(s) = sig {
            n.set_signature(s);
        }
        n
    }

    fn edge(s: &str, t: &str, rel: &str) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: rel.into(),
            confidence: synaptic_core::Confidence::Extracted,
            source_file: "m.py".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    #[test]
    fn describes_signature_and_callees() {
        let sig = Signature {
            params: vec![Param {
                name: "name".into(),
                type_ref: Some("str".into()),
            }],
            return_type: Some("str".into()),
            raw: "def greet(name: str) -> str".into(),
        };
        let kg = KnowledgeGraph::from_graph_data(GraphData {
            nodes: vec![
                fn_node("greet", "greet()", Some(sig)),
                fn_node("parse", "parse()", None),
                fn_node("validate", "validate()", None),
            ],
            links: vec![
                edge("greet", "validate", "calls"),
                edge("greet", "parse", "calls"),
            ],
            ..Default::default()
        });

        let d = describe_node(&kg, &NodeId("greet".into())).expect("node exists");
        assert_eq!(d.label, "greet()");
        assert_eq!(d.kind.as_deref(), Some("function"));
        // Callees are distinct, sorted for determinism.
        assert_eq!(
            d.callees,
            vec!["parse()".to_string(), "validate()".to_string()]
        );
        // Summary reads "takes ..., returns ..., calls [...]".
        assert!(d.summary.contains("takes (name: str)"), "{}", d.summary);
        assert!(d.summary.contains("returns str"), "{}", d.summary);
        assert!(
            d.summary.contains("calls [parse(), validate()]"),
            "{}",
            d.summary
        );
    }

    #[test]
    fn missing_node_is_none() {
        let kg = KnowledgeGraph::from_graph_data(GraphData::default());
        assert!(describe_node(&kg, &NodeId("nope".into())).is_none());
    }

    #[test]
    fn no_params_reads_cleanly() {
        let sig = Signature {
            params: vec![],
            return_type: None,
            raw: "def tick()".into(),
        };
        let kg = KnowledgeGraph::from_graph_data(GraphData {
            nodes: vec![fn_node("tick", "tick()", Some(sig))],
            links: vec![],
            ..Default::default()
        });
        let d = describe_node(&kg, &NodeId("tick".into())).unwrap();
        assert!(d.summary.contains("takes no arguments"), "{}", d.summary);
        assert!(d.callees.is_empty());
    }
}
