use std::collections::{HashMap, HashSet};

use petgraph::graph::{Graph, NodeIndex};
use petgraph::visit::EdgeRef;
use petgraph::{Directed, Direction};
use synaptic_core::{Edge, GraphData, Hyperedge, Node, NodeId};

/// A built knowledge graph: a `petgraph` of `Node`s and `Edge`s plus the
/// `NodeId → NodeIndex` lookup, hyperedges, and provenance. Internally always
/// `Directed` (edge weights carry the logical `source`/`target`); `directed`
/// records whether undirected dedup semantics were applied at build time.
#[derive(Debug, Clone)]
pub struct KnowledgeGraph {
    graph: Graph<Node, Edge, Directed>,
    index: HashMap<NodeId, NodeIndex>,
    pub directed: bool,
    pub hyperedges: Vec<Hyperedge>,
    pub built_at_commit: Option<String>,
}

impl KnowledgeGraph {
    pub(crate) fn with_directed(directed: bool) -> Self {
        KnowledgeGraph {
            graph: Graph::new(),
            index: HashMap::new(),
            directed,
            hyperedges: Vec::new(),
            built_at_commit: None,
        }
    }

    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    pub fn contains_node(&self, id: &NodeId) -> bool {
        self.index.contains_key(id)
    }

    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.index.get(id).map(|&ix| &self.graph[ix])
    }

    /// Mutable access to a node by id.
    pub fn node_mut(&mut self, id: &NodeId) -> Option<&mut Node> {
        match self.index.get(id) {
            Some(&ix) => Some(&mut self.graph[ix]),
            None => None,
        }
    }

    /// Nodes in insertion order.
    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.graph.node_weights()
    }

    /// Edges in insertion order.
    pub fn edges(&self) -> impl Iterator<Item = &Edge> {
        self.graph.edge_weights()
    }

    /// Edges incident to `id` (either endpoint), each yielded once — O(degree)
    /// via the petgraph adjacency rather than a full edge scan. Empty when `id`
    /// is absent. A self-loop is yielded once (skipped on the incoming pass), so
    /// the set equals `edges().filter(|e| e.source == id || e.target == id)`.
    pub fn incident_edges(&self, id: &NodeId) -> impl Iterator<Item = &Edge> + '_ {
        self.index.get(id).into_iter().flat_map(move |&ix| {
            self.graph
                .edges_directed(ix, Direction::Outgoing)
                .chain(
                    self.graph
                        .edges_directed(ix, Direction::Incoming)
                        .filter(|e| e.source() != e.target()),
                )
                .map(|e| e.weight())
        })
    }

    /// Degree of `id` (incident edges, either endpoint), O(degree). 0 if absent.
    pub fn degree(&self, id: &NodeId) -> usize {
        self.incident_edges(id).count()
    }

    /// Distinct out-neighbours of `id` over the given relations (empty = all).
    pub fn fan_out(&self, id: &NodeId, relations: &[&str]) -> usize {
        let mut seen = HashSet::new();
        for e in self.incident_edges(id) {
            if &e.source == id
                && &e.target != id
                && (relations.is_empty() || relations.contains(&e.relation.as_str()))
            {
                seen.insert(&e.target);
            }
        }
        seen.len()
    }

    /// Distinct in-neighbours of `id` over the given relations (empty = all).
    pub fn fan_in(&self, id: &NodeId, relations: &[&str]) -> usize {
        let mut seen = HashSet::new();
        for e in self.incident_edges(id) {
            if &e.target == id
                && &e.source != id
                && (relations.is_empty() || relations.contains(&e.relation.as_str()))
            {
                seen.insert(&e.source);
            }
        }
        seen.len()
    }

    /// Nodes matching a predicate, in insertion order.
    pub fn filter_nodes<F: Fn(&Node) -> bool>(&self, pred: F) -> Vec<&Node> {
        self.nodes().filter(|n| pred(n)).collect()
    }

    /// Lines of code for `id` from its span, if present.
    pub fn loc(&self, id: &NodeId) -> Option<u32> {
        self.node(id).and_then(|n| n.loc())
    }

    /// Effective lines of code for `id`, folding members into a type's
    /// footprint. A class/struct/trait/enum/interface/protocol's own span covers
    /// only its declaration -- its methods live in separate nodes (a Rust `impl`
    /// block, a C# partial class, a Go receiver method), so the bare span
    /// undercounts the type's real size. This spans the type plus the members it
    /// reaches via `contains`/`method` edges in the same file. For a non-type
    /// node (function, file, ...) it equals [`Self::loc`].
    pub fn effective_loc(&self, id: &NodeId) -> Option<u32> {
        use synaptic_core::NodeKind::*;
        let node = self.node(id)?;
        let span = node.span()?;
        let is_type = matches!(
            node.kind(),
            Some(Class | Interface | Trait | Struct | Enum | Protocol)
        );
        if !is_type {
            return Some(span.line_count());
        }
        let mut start = span.start_line;
        let mut end = span.end_line;
        for e in self.incident_edges(id) {
            if &e.source != id
                || &e.target == id
                || !matches!(e.relation.as_str(), "contains" | "method")
            {
                continue;
            }
            if let Some(m) = self.node(&e.target) {
                if m.source_file == node.source_file {
                    if let Some(ms) = m.span() {
                        start = start.min(ms.start_line);
                        end = end.max(ms.end_line);
                    }
                }
            }
        }
        Some(end.saturating_sub(start) + 1)
    }

    /// Insert/overwrite a node (last write wins on duplicate id), preserving
    /// first-seen position. Returns the node's index.
    ///
    /// Exception: a **located** node (non-empty `source_file`) is never clobbered
    /// by an empty-`source_file` **stub** of the same id. Some extractors emit a
    /// stub whose id equals a real file's `file_node_id` (a .NET `ProjectReference`
    /// or a bash `source` target whose real file is also in the corpus); since
    /// nodes merge in path-sorted order, last-write-wins would otherwise drop the
    /// real node's `source_file`/`source_location`/label depending on file order.
    pub(crate) fn upsert_node(&mut self, node: Node) -> NodeIndex {
        if let Some(&ix) = self.index.get(&node.id) {
            let existing_located = !self.graph[ix].source_file.is_empty();
            let incoming_stub = node.source_file.is_empty();
            if !(existing_located && incoming_stub) {
                self.graph[ix] = node;
            }
            ix
        } else {
            let id = node.id.clone();
            let ix = self.graph.add_node(node);
            self.index.insert(id, ix);
            ix
        }
    }

    pub(crate) fn add_edge_raw(&mut self, src: NodeIndex, tgt: NodeIndex, edge: Edge) {
        self.graph.add_edge(src, tgt, edge);
    }

    pub(crate) fn index_of(&self, id: &NodeId) -> Option<NodeIndex> {
        self.index.get(id).copied()
    }

    /// Remove the given node ids (and any incident edges), preserving order.
    /// petgraph's `remove_node` invalidates indices, so we rebuild — safe and
    /// simple at build scale.
    pub(crate) fn remove_nodes(&mut self, remove: &std::collections::HashSet<NodeId>) {
        if remove.is_empty() {
            return;
        }
        let kept_nodes: Vec<Node> = self
            .graph
            .node_weights()
            .filter(|n| !remove.contains(&n.id))
            .cloned()
            .collect();
        let kept_edges: Vec<Edge> = self
            .graph
            .edge_weights()
            .filter(|e| !remove.contains(&e.source) && !remove.contains(&e.target))
            .cloned()
            .collect();

        let mut graph = Graph::new();
        let mut index = HashMap::new();
        for n in kept_nodes {
            let id = n.id.clone();
            let ix = graph.add_node(n);
            index.insert(id, ix);
        }
        for e in kept_edges {
            if let (Some(&s), Some(&t)) = (index.get(&e.source), index.get(&e.target)) {
                graph.add_edge(s, t, e);
            }
        }
        self.graph = graph;
        self.index = index;
    }

    /// Serialize to the `graph.json` node-link contract.
    pub fn to_graph_data(&self) -> GraphData {
        GraphData {
            directed: self.directed,
            multigraph: false,
            graph: serde_json::Map::new(),
            nodes: self.nodes().cloned().collect(),
            links: self.edges().cloned().collect(),
            hyperedges: self.hyperedges.clone(),
            built_at_commit: self.built_at_commit.clone(),
        }
    }

    /// Load a `KnowledgeGraph` from existing node-link data as-is (no remap or
    /// dedup — use [`crate::build_from_parts`] to assemble fresh extraction).
    pub fn from_graph_data(data: GraphData) -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::with_directed(data.directed);
        kg.hyperedges = data.hyperedges;
        kg.built_at_commit = data.built_at_commit;
        for node in data.nodes {
            kg.upsert_node(node);
        }
        for edge in data.links {
            if let (Some(s), Some(t)) = (kg.index_of(&edge.source), kg.index_of(&edge.target)) {
                kg.add_edge_raw(s, t, edge);
            }
        }
        kg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Confidence, FileType};

    fn node(id: &str, label: &str, sf: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: sf.into(),
            source_location: Some("L1".into()),
            community: None,
            repo: None,
            extra: Map::new(),
        }
    }

    fn edge(s: &str, t: &str, rel: &str) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: rel.into(),
            confidence: Confidence::Extracted,
            source_file: "a.py".into(),
            source_location: Some("L1".into()),
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    #[test]
    fn fan_in_out_filter_and_loc() {
        let mut a = node("a", "a", "a.rs");
        a.set_span(synaptic_core::Span {
            start_line: 1,
            start_col: 1,
            end_line: 10,
            end_col: 1,
        });
        let gd = GraphData {
            nodes: vec![a, node("b", "b", "b.rs"), node("c", "c", "c.rs")],
            links: vec![
                edge("a", "b", "calls"),
                edge("a", "c", "calls"),
                edge("c", "a", "calls"),
            ],
            ..Default::default()
        };
        let kg = KnowledgeGraph::from_graph_data(gd);
        assert_eq!(kg.fan_out(&NodeId("a".into()), &["calls"]), 2);
        assert_eq!(kg.fan_in(&NodeId("a".into()), &["calls"]), 1);
        assert_eq!(kg.fan_out(&NodeId("a".into()), &["imports"]), 0); // relation filter
        assert_eq!(kg.loc(&NodeId("a".into())), Some(10));
        assert_eq!(kg.loc(&NodeId("b".into())), None);
        assert_eq!(kg.filter_nodes(|n| n.source_file == "b.rs").len(), 1);
    }

    #[test]
    fn effective_loc_folds_type_members() {
        // A struct declared on lines 10-19 with a method spanning 20-200 in the
        // same file: its own loc is just the 10-line declaration, but its
        // effective loc covers the impl too. A method in a different file must
        // NOT inflate it.
        let mut s = node("s", "Big", "big.rs");
        s.set_kind(synaptic_core::NodeKind::Struct);
        s.set_span(synaptic_core::Span {
            start_line: 10,
            start_col: 1,
            end_line: 19,
            end_col: 1,
        });
        let mut m = node("m", ".method()", "big.rs");
        m.set_kind(synaptic_core::NodeKind::Method);
        m.set_span(synaptic_core::Span {
            start_line: 20,
            start_col: 1,
            end_line: 200,
            end_col: 1,
        });
        let mut other = node("o", ".elsewhere()", "other.rs");
        other.set_kind(synaptic_core::NodeKind::Method);
        other.set_span(synaptic_core::Span {
            start_line: 1,
            start_col: 1,
            end_line: 999,
            end_col: 1,
        });
        let gd = GraphData {
            nodes: vec![s, m, other],
            links: vec![edge("s", "m", "method"), edge("s", "o", "method")],
            ..Default::default()
        };
        let kg = KnowledgeGraph::from_graph_data(gd);
        // The struct's own loc is the declaration only.
        assert_eq!(kg.loc(&NodeId("s".into())), Some(10));
        // Effective loc folds the same-file method (10..=200) but not other.rs.
        assert_eq!(kg.effective_loc(&NodeId("s".into())), Some(191));
        // A non-type node's effective loc equals its own loc.
        assert_eq!(kg.effective_loc(&NodeId("m".into())), Some(181));
    }

    #[test]
    fn from_graph_data_round_trips_counts_and_lookup() {
        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![node("a", "a.py", "a.py"), node("b", "b.py", "b.py")],
            links: vec![edge("a", "b", "calls")],
            hyperedges: vec![],
            built_at_commit: Some("abc".into()),
        };
        let kg = KnowledgeGraph::from_graph_data(gd);
        assert_eq!(kg.node_count(), 2);
        assert_eq!(kg.edge_count(), 1);
        assert!(kg.contains_node(&NodeId("a".into())));
        assert_eq!(kg.node(&NodeId("b".into())).unwrap().label, "b.py");

        let back = kg.to_graph_data();
        assert_eq!(back.nodes.len(), 2);
        assert_eq!(back.links.len(), 1);
        assert_eq!(back.built_at_commit.as_deref(), Some("abc"));
        assert!(!back.directed);
    }

    #[test]
    fn degree_and_incident_edges_match_full_scan() {
        // a has out-edges a->b, a->c, a self-loop a->a, and an in-edge d->a.
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                node("a", "a.py", "a.py"),
                node("b", "b.py", "b.py"),
                node("c", "c.py", "c.py"),
                node("d", "d.py", "d.py"),
            ],
            links: vec![
                edge("a", "b", "calls"),
                edge("a", "c", "calls"),
                edge("d", "a", "calls"),
                edge("a", "a", "recurses"),
            ],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let kg = KnowledgeGraph::from_graph_data(gd);
        for id in ["a", "b", "c", "d"].map(|s| NodeId(s.into())) {
            // The old O(E) formula: count edges touching the node (self-loop once).
            let want = kg
                .edges()
                .filter(|e| e.source == id || e.target == id)
                .count();
            assert_eq!(kg.degree(&id), want, "degree({id:?})");
            assert_eq!(
                kg.incident_edges(&id).count(),
                want,
                "incident_edges({id:?}) count"
            );
            assert!(
                kg.incident_edges(&id)
                    .all(|e| e.source == id || e.target == id),
                "every incident edge touches {id:?}"
            );
        }
        // Absent node: empty / zero.
        let ghost = NodeId("ghost".into());
        assert_eq!(kg.degree(&ghost), 0);
        assert_eq!(kg.incident_edges(&ghost).count(), 0);
    }

    #[test]
    fn from_graph_data_drops_edges_to_unknown_endpoints() {
        let gd = GraphData {
            directed: false,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![node("a", "a.py", "a.py")],
            links: vec![edge("a", "ghost", "calls")],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let kg = KnowledgeGraph::from_graph_data(gd);
        assert_eq!(kg.node_count(), 1);
        assert_eq!(kg.edge_count(), 0);
    }
}
