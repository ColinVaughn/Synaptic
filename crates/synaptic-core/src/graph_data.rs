use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::edge::Edge;
use crate::hyperedge::Hyperedge;
use crate::node::Node;

/// The on-disk `graph.json` contract: NetworkX `node_link_data(G, edges="links")`
/// shape. Edges live under `links` on output; `edges` is accepted as an input
/// alias. `hyperedges` is always emitted (possibly empty), set unconditionally.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphData {
    #[serde(default)]
    pub directed: bool,
    #[serde(default)]
    pub multigraph: bool,
    #[serde(default)]
    pub graph: Map<String, Value>,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default, alias = "edges")]
    pub links: Vec<Edge>,
    #[serde(default)]
    pub hyperedges: Vec<Hyperedge>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub built_at_commit: Option<String>,
}

impl Default for GraphData {
    fn default() -> Self {
        GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: Vec::new(),
            links: Vec::new(),
            hyperedges: Vec::new(),
            built_at_commit: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "directed": false,
        "multigraph": false,
        "graph": {},
        "nodes": [
            {"id": "a", "label": "a.py", "file_type": "code", "source_file": "a.py"},
            {"id": "b", "label": "b.py", "file_type": "code", "source_file": "b.py"}
        ],
        "links": [
            {"source": "a", "target": "b", "relation": "imports",
             "confidence": "EXTRACTED", "source_file": "a.py", "weight": 1.0}
        ],
        "hyperedges": [],
        "built_at_commit": "abc123"
    }"#;

    #[test]
    fn parses_node_link_format() {
        let g: GraphData = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.links.len(), 1);
        assert_eq!(g.built_at_commit.as_deref(), Some("abc123"));
    }

    #[test]
    fn serializes_with_links_key_not_edges() {
        let g: GraphData = serde_json::from_str(SAMPLE).unwrap();
        let out = serde_json::to_value(&g).unwrap();
        let obj = out.as_object().unwrap();
        assert!(obj.contains_key("links"));
        assert!(!obj.contains_key("edges"));
        // hyperedges always present even when empty.
        assert!(obj.contains_key("hyperedges"));
        assert!(obj.contains_key("directed"));
        assert!(obj.contains_key("multigraph"));
        assert!(obj.contains_key("graph"));
    }

    #[test]
    fn accepts_edges_alias_on_input() {
        let raw = r#"{"nodes": [], "edges": []}"#;
        let g: GraphData = serde_json::from_str(raw).unwrap();
        assert_eq!(g.links.len(), 0);
    }

    #[test]
    fn built_at_commit_omitted_when_none() {
        let g = GraphData::default();
        let obj = serde_json::to_value(&g).unwrap();
        assert!(!obj.as_object().unwrap().contains_key("built_at_commit"));
    }
}
