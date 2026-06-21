//! Resolve a rename target by name and disambiguate when several definitions
//! share it. A target is always a *definition* node (a class/function/type/...),
//! never a bare reference: references are edges, not nodes, in this graph.

use serde::{Deserialize, Serialize};
use synaptic_core::{FileType, NodeKind, Span};
use synaptic_graph::KnowledgeGraph;

/// `foo()` -> `foo`, `.bar()` -> `bar`, case-insensitive. Mirrors the resolver's
/// label normalization (see `synaptic_graph::symbol_resolution`) so candidate
/// matching agrees with how calls were resolved into edges.
pub(crate) fn normalize(label: &str) -> String {
    label
        .trim()
        .trim_matches(|c| c == '(' || c == ')')
        .trim_start_matches('.')
        .to_lowercase()
}

/// A definition node that could be the rename target.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Candidate {
    pub id: String,
    pub label: String,
    pub kind: Option<String>,
    pub visibility: Option<String>,
    pub file: String,
    pub span: Option<Span>,
}

/// Declaration kinds a rename can target. Unenriched nodes (`None`) fall through
/// on the label match so long-tail languages without kind metadata still work;
/// only the obviously-not-a-declaration kinds are excluded.
fn is_definition(kind: Option<NodeKind>) -> bool {
    match kind {
        None => true,
        Some(k) => !matches!(k, NodeKind::Variable | NodeKind::Other),
    }
}

/// Every definition node whose normalized label matches `name`, sorted for
/// deterministic output.
pub fn find_candidates(kg: &KnowledgeGraph, name: &str) -> Vec<Candidate> {
    let want = normalize(name);
    let mut out: Vec<Candidate> = kg
        .nodes()
        .filter(|n| n.file_type == FileType::Code && !n.source_file.is_empty())
        .filter(|n| normalize(&n.label) == want)
        .filter(|n| is_definition(n.kind()))
        .map(|n| Candidate {
            id: n.id.0.clone(),
            label: n.label.clone(),
            kind: n.kind().map(|k| k.as_str().to_string()),
            visibility: n.visibility().map(|v| v.as_str().to_string()),
            file: n.source_file.clone(),
            span: n.span(),
        })
        .collect();
    out.sort_by(|a, b| (a.file.as_str(), a.id.as_str()).cmp(&(b.file.as_str(), b.id.as_str())));
    out
}

/// The outcome of applying `--id`/`--file` disambiguation to a candidate set.
pub enum Selection {
    One(Candidate),
    Ambiguous(Vec<Candidate>),
    None,
}

/// Narrow candidates by an exact node id and/or a file-path substring.
pub fn select_target(cands: &[Candidate], id: Option<&str>, file: Option<&str>) -> Selection {
    let filtered: Vec<Candidate> = cands
        .iter()
        .filter(|c| id.is_none_or(|i| c.id == i))
        .filter(|c| file.is_none_or(|f| c.file.contains(f)))
        .cloned()
        .collect();
    match filtered.len() {
        0 => Selection::None,
        1 => Selection::One(filtered.into_iter().next().unwrap()),
        _ => Selection::Ambiguous(filtered),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{GraphData, Node, NodeId};

    pub(crate) fn class_node(id: &str, label: &str, file: &str) -> Node {
        let mut n = Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: file.into(),
            source_location: Some("L1".into()),
            community: None,
            repo: None,
            extra: Map::new(),
        };
        n.set_kind(NodeKind::Class);
        n.set_span(Span {
            start_line: 1,
            start_col: 1,
            end_line: 4,
            end_col: 2,
        });
        n
    }

    /// External crates can only build a graph through `from_graph_data`.
    pub(crate) fn kg_from(nodes: Vec<Node>, directed: bool) -> KnowledgeGraph {
        KnowledgeGraph::from_graph_data(GraphData {
            directed,
            multigraph: false,
            graph: Map::new(),
            nodes,
            links: vec![],
            hyperedges: vec![],
            built_at_commit: None,
        })
    }

    fn graph_two_users() -> KnowledgeGraph {
        kg_from(
            vec![
                class_node("models::User", "User", "models.py"),
                class_node("other::User", "User", "other.py"),
            ],
            true,
        )
    }

    #[test]
    fn finds_all_definitions_by_label() {
        let kg = graph_two_users();
        let c = find_candidates(&kg, "User");
        assert_eq!(c.len(), 2);
        // case-insensitive + paren-insensitive
        assert_eq!(find_candidates(&kg, "user()").len(), 2);
    }

    #[test]
    fn file_substring_disambiguates() {
        let kg = graph_two_users();
        let c = find_candidates(&kg, "User");
        match select_target(&c, None, Some("models.py")) {
            Selection::One(t) => assert_eq!(t.file, "models.py"),
            _ => panic!("expected a single match"),
        }
        assert!(matches!(
            select_target(&c, None, None),
            Selection::Ambiguous(_)
        ));
        assert!(matches!(
            select_target(&c, Some("other::User"), None),
            Selection::One(_)
        ));
    }

    #[test]
    fn missing_name_is_empty() {
        let kg = graph_two_users();
        assert!(find_candidates(&kg, "Nonexistent").is_empty());
    }
}
