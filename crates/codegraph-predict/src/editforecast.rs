//! Analytic hypothetical-graph forecast for a *described* edit, before any code
//! is written. Where `assess_edit` classifies which dependents break, this
//! extends it into the structural graph delta the edit would produce -- whether
//! the symbol's node disappears, how many edges that severs, and whether a public
//! API is removed from external view -- so an agent can see "what the graph will
//! look like" without applying anything. The empirical counterpart is the
//! sandbox (`codegraph speculate`); this is the pre-code, no-IO prediction.

use serde::{Deserialize, Serialize};

use codegraph_graph::KnowledgeGraph;

use crate::edit::{assess_edit, resolve_edit_target, EditDependent, EditKind};

/// The predicted graph delta of a described edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditForecast {
    pub symbol: String,
    pub kind: String,
    pub target_file: String,
    /// The edit removes the symbol's graph node (a deletion).
    pub removes_node: bool,
    /// Edges that would disappear (incident to a removed node); 0 otherwise.
    pub severed_edges: usize,
    /// The symbol is a public API this edit removes from external view
    /// (a deletion, or narrowing its visibility).
    pub removed_public_api: bool,
    /// Dependents that would break.
    pub breaks: Vec<EditDependent>,
    /// Dependents to re-check.
    pub review: Vec<EditDependent>,
    pub summary: String,
}

/// Predict the post-edit graph delta for `kind` on `symbol`. Returns `None` if
/// the symbol cannot be resolved unambiguously. Pure graph analysis: no git, no
/// build, nothing applied.
pub fn forecast_edit(
    kg: &KnowledgeGraph,
    symbol: &str,
    kind: EditKind,
    depth: usize,
) -> Option<EditForecast> {
    let impact = assess_edit(kg, symbol, kind, depth)?;
    let id = resolve_edit_target(kg, symbol)?;
    let is_public = kg
        .node(&id)
        .and_then(|n| n.visibility())
        .is_some_and(|v| v.as_str() == "public");

    let removes_node = kind == EditKind::Delete;
    let severed_edges = if removes_node { kg.degree(&id) } else { 0 };
    // A deletion removes the API outright; narrowing visibility removes it from
    // external view. A signature change keeps the API present (callers may break
    // but the symbol still exists), so it does not "remove" a public API.
    let removed_public_api = is_public && matches!(kind, EditKind::Delete | EditKind::Visibility);

    let mut parts = Vec::new();
    if removes_node {
        parts.push(format!(
            "removes the node and severs {severed_edges} edge(s)"
        ));
    }
    if removed_public_api {
        parts.push("removes a public API".to_string());
    }
    parts.push(format!(
        "{} dependent(s) break, {} to review",
        impact.breaks.len(),
        impact.review.len()
    ));
    let summary = format!("{} {}: {}", impact.kind, symbol, parts.join("; "));

    Some(EditForecast {
        symbol: symbol.to_string(),
        kind: impact.kind,
        target_file: impact.target_file,
        removes_node,
        severed_edges,
        removed_public_api,
        breaks: impact.breaks,
        review: impact.review,
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codegraph_core::{Confidence, Edge, FileType, GraphData, Node, NodeId, Visibility};
    use serde_json::Map;

    fn node(id: &str, label: &str, file: &str, vis: Option<Visibility>) -> Node {
        let mut n = Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: file.into(),
            source_location: Some("L1".into()),
            community: Some(0),
            repo: None,
            extra: Map::new(),
        };
        if let Some(v) = vis {
            n.set_visibility(v);
        }
        n
    }

    fn edge(s: &str, t: &str, r: &str) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: r.into(),
            confidence: Confidence::Extracted,
            source_file: "x".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    // Service (svc.py) is called by c1 (a.py), imported by m1 (b.py), referenced
    // by s1 (svc.py, same file). Service itself calls dep (dep.py).
    fn kg(public: bool) -> KnowledgeGraph {
        let vis = public.then_some(Visibility::Public);
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                node("svc", "Service", "svc.py", vis),
                node("c1", "caller", "a.py", None),
                node("m1", "importer", "b.py", None),
                node("s1", "sibling", "svc.py", None),
                node("dep", "dependency", "dep.py", None),
            ],
            links: vec![
                edge("c1", "svc", "calls"),
                edge("m1", "svc", "imports"),
                edge("s1", "svc", "references"),
                edge("svc", "dep", "calls"),
            ],
            hyperedges: vec![],
            built_at_commit: None,
        };
        KnowledgeGraph::from_graph_data(gd)
    }

    #[test]
    fn delete_removes_the_node_and_severs_its_edges() {
        let f = forecast_edit(&kg(false), "Service", EditKind::Delete, 5).unwrap();
        assert!(f.removes_node);
        // 3 incoming dependents + 1 outgoing dependency = 4 incident edges severed.
        assert_eq!(f.severed_edges, 4, "{f:?}");
        assert_eq!(f.breaks.len(), 3, "every dependent breaks: {f:?}");
        assert!(f.review.is_empty());
        assert!(f.summary.contains("severs 4 edge"), "{}", f.summary);
    }

    #[test]
    fn delete_of_a_public_symbol_flags_api_removal() {
        let f = forecast_edit(&kg(true), "Service", EditKind::Delete, 5).unwrap();
        assert!(f.removed_public_api, "{f:?}");
        assert!(f.summary.contains("public API"), "{}", f.summary);
    }

    #[test]
    fn signature_change_keeps_the_node_and_does_not_remove_the_api() {
        let f = forecast_edit(&kg(true), "Service", EditKind::Signature, 5).unwrap();
        assert!(!f.removes_node, "{f:?}");
        assert_eq!(f.severed_edges, 0);
        assert!(
            !f.removed_public_api,
            "a signature change does not remove the API"
        );
        // Callers/refs break; the import is routed to review.
        let review: Vec<&str> = f.review.iter().map(|d| d.label.as_str()).collect();
        assert_eq!(review, vec!["importer"]);
    }

    #[test]
    fn visibility_narrowing_of_a_public_symbol_removes_it_from_external_view() {
        let f = forecast_edit(&kg(true), "Service", EditKind::Visibility, 5).unwrap();
        assert!(!f.removes_node);
        assert!(f.removed_public_api, "{f:?}");
    }

    #[test]
    fn unknown_symbol_returns_none() {
        assert!(forecast_edit(&kg(false), "Nope", EditKind::Delete, 5).is_none());
    }
}
