//! Named architectural patterns over the enriched graph. These are heuristics
//! built on node kind/visibility + edge topology; each documents what it matches.

use std::collections::{HashMap, HashSet};

use synaptic_core::{NodeId, NodeKind};
use synaptic_graph::KnowledgeGraph;

use crate::{QueryResult, SynqlError};

/// The built-in patterns: `(name, description)`.
pub fn list_patterns() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "god-class",
            "Large types (class/struct/trait/enum/interface) over 500 effective LOC \
             (declaration plus members) with more than 20 outgoing dependencies.",
        ),
        (
            "singleton",
            "Classes that hold or return an instance of their own type (self-reference).",
        ),
        (
            "factory",
            "Functions/methods returning an abstract type that has 2+ implementations.",
        ),
        (
            "observer",
            "Subject classes that hold a field of an interface implemented by 2+ types.",
        ),
        (
            "service-locator",
            "Classes accessed (called/referenced) from 3+ distinct communities.",
        ),
    ]
}

/// Run a named pattern, returning a single-column (`node`) result.
pub fn run_pattern(kg: &KnowledgeGraph, name: &str) -> Result<QueryResult, SynqlError> {
    match name {
        "god-class" => Ok(single(detect_god_class(kg))),
        "singleton" => Ok(single(detect_singleton(kg))),
        "factory" => Ok(single(detect_factory(kg))),
        "observer" => Ok(single(detect_observer(kg))),
        "service-locator" => {
            // The detector counts distinct source communities; without clustering
            // every node's community is None and it would silently return nothing.
            if kg.nodes().all(|n| n.community.is_none()) {
                return Err(SynqlError::Parse(
                    "service-locator needs community assignments; build the graph with \
                     `synaptic extract` (which clusters) first"
                        .to_string(),
                ));
            }
            Ok(single(detect_service_locator(kg)))
        }
        _ => Err(SynqlError::Parse(format!(
            "unknown pattern '{name}'; see --list-patterns"
        ))),
    }
}

fn single(mut ids: Vec<NodeId>) -> QueryResult {
    ids.sort();
    ids.dedup();
    QueryResult {
        columns: vec!["node".to_string()],
        rows: ids.into_iter().map(|id| vec![id]).collect(),
        aggregates: None,
    }
}

fn kind_of(kg: &KnowledgeGraph, id: &NodeId) -> Option<NodeKind> {
    kg.node(id).and_then(|n| n.kind())
}

/// `target -> distinct sources` over `implements`/`inherits` (how many types
/// implement/extend a given abstraction).
fn impl_indegree(kg: &KnowledgeGraph) -> HashMap<NodeId, HashSet<NodeId>> {
    let mut m: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
    for e in kg.edges() {
        if e.relation == "implements" || e.relation == "inherits" {
            m.entry(e.target.clone())
                .or_default()
                .insert(e.source.clone());
        }
    }
    m
}

/// God class: a large type with many outgoing dependencies. Matches any
/// type-like kind (class/struct/trait/enum/interface/protocol), not just
/// `class`, so it fires on Rust/Go/etc. where types are structs/interfaces; size
/// is the type's effective loc (declaration plus its members), so an `impl`-heavy
/// type is not undercounted by its small declaration span.
fn detect_god_class(kg: &KnowledgeGraph) -> Vec<NodeId> {
    let mut out = Vec::new();
    for n in kg.nodes() {
        let is_type = matches!(
            n.kind(),
            Some(
                NodeKind::Class
                    | NodeKind::Interface
                    | NodeKind::Trait
                    | NodeKind::Struct
                    | NodeKind::Enum
                    | NodeKind::Protocol
            )
        );
        if is_type && kg.effective_loc(&n.id).unwrap_or(0) > 500 && kg.fan_out(&n.id, &[]) > 20 {
            out.push(n.id.clone());
        }
    }
    out
}

/// Singleton: a class that holds (a `field`) or returns (`return_type`) an
/// instance of its own type — captured as a `references` edge from the class or
/// one of its methods back to the class itself.
fn detect_singleton(kg: &KnowledgeGraph) -> Vec<NodeId> {
    let mut methods: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for e in kg.edges() {
        if e.relation == "method" {
            methods
                .entry(e.source.clone())
                .or_default()
                .push(e.target.clone());
        }
    }
    // class -> set of sources that reference it as a field/return type.
    let mut refs_to: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
    for e in kg.edges() {
        if e.relation == "references"
            && matches!(e.context.as_deref(), Some("field") | Some("return_type"))
        {
            refs_to
                .entry(e.target.clone())
                .or_default()
                .insert(e.source.clone());
        }
    }
    let mut out = Vec::new();
    for n in kg.nodes() {
        if kind_of(kg, &n.id) != Some(NodeKind::Class) {
            continue;
        }
        let Some(srcs) = refs_to.get(&n.id) else {
            continue;
        };
        let mut own: HashSet<&NodeId> = HashSet::new();
        own.insert(&n.id);
        if let Some(ms) = methods.get(&n.id) {
            own.extend(ms.iter());
        }
        if own.iter().any(|o| srcs.contains(*o)) {
            out.push(n.id.clone());
        }
    }
    out
}

/// Factory: a function/method whose return type has 2+ implementations.
fn detect_factory(kg: &KnowledgeGraph) -> Vec<NodeId> {
    let impls = impl_indegree(kg);
    let mut out = Vec::new();
    for e in kg.edges() {
        if e.relation == "references" && e.context.as_deref() == Some("return_type") {
            let n_impls = impls.get(&e.target).map(|s| s.len()).unwrap_or(0);
            if n_impls >= 2
                && matches!(
                    kind_of(kg, &e.source),
                    Some(NodeKind::Function) | Some(NodeKind::Method)
                )
            {
                out.push(e.source.clone());
            }
        }
    }
    out
}

/// Observer: a subject class that holds (a `field`) an interface implemented by
/// 2+ types. Returns the subject classes.
fn detect_observer(kg: &KnowledgeGraph) -> Vec<NodeId> {
    let impls = impl_indegree(kg);
    let mut out = Vec::new();
    for e in kg.edges() {
        if e.relation == "references" && e.context.as_deref() == Some("field") {
            let n_impls = impls.get(&e.target).map(|s| s.len()).unwrap_or(0);
            if n_impls >= 2 && kind_of(kg, &e.source) == Some(NodeKind::Class) {
                out.push(e.source.clone());
            }
        }
    }
    out
}

/// Service locator: a class accessed (`calls`/`references` incoming) from 3+
/// distinct communities — a global access point.
fn detect_service_locator(kg: &KnowledgeGraph) -> Vec<NodeId> {
    let mut access: HashMap<NodeId, HashSet<u32>> = HashMap::new();
    for e in kg.edges() {
        if e.relation == "calls" || e.relation == "references" {
            if let Some(c) = kg.node(&e.source).and_then(|n| n.community) {
                access.entry(e.target.clone()).or_default().insert(c);
            }
        }
    }
    let mut out = Vec::new();
    for (id, comms) in access {
        if comms.len() >= 3 && kind_of(kg, &id) == Some(NodeKind::Class) {
            out.push(id);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, Span, Visibility};

    fn node(id: &str, kind: NodeKind, community: Option<u32>) -> Node {
        let mut n = Node {
            id: NodeId(id.into()),
            label: id.into(),
            file_type: FileType::Code,
            source_file: format!("{id}.rs"),
            source_location: Some("L1".into()),
            community,
            repo: None,
            extra: Map::new(),
        };
        n.set_kind(kind);
        n.set_visibility(Visibility::Public);
        n.set_span(Span {
            start_line: 1,
            start_col: 1,
            end_line: 3,
            end_col: 1,
        });
        n
    }

    fn edge(s: &str, t: &str, rel: &str, ctx: Option<&str>) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: rel.into(),
            confidence: Confidence::Extracted,
            source_file: "x.rs".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: ctx.map(str::to_string),
            cross_repo: false,
            extra: Map::new(),
        }
    }

    fn graph(nodes: Vec<Node>, edges: Vec<Edge>) -> KnowledgeGraph {
        KnowledgeGraph::from_graph_data(GraphData {
            nodes,
            links: edges,
            ..Default::default()
        })
    }

    fn ids(r: QueryResult) -> Vec<String> {
        let mut v: Vec<String> = r.rows.iter().map(|row| row[0].0.clone()).collect();
        v.sort();
        v
    }

    #[test]
    fn singleton_detects_self_holding_class() {
        // Cfg holds an instance of itself via getInstance() return type.
        let kg = graph(
            vec![
                node("Cfg", NodeKind::Class, None),
                node("Cfg.getInstance", NodeKind::Method, None),
                node("Plain", NodeKind::Class, None),
            ],
            vec![
                edge("Cfg", "Cfg.getInstance", "method", None),
                edge("Cfg.getInstance", "Cfg", "references", Some("return_type")),
            ],
        );
        assert_eq!(ids(run_pattern(&kg, "singleton").unwrap()), vec!["Cfg"]);
    }

    #[test]
    fn factory_detects_method_returning_abstraction() {
        // make() returns Shape, which Circle and Square implement.
        let kg = graph(
            vec![
                node("make", NodeKind::Function, None),
                node("Shape", NodeKind::Interface, None),
                node("Circle", NodeKind::Class, None),
                node("Square", NodeKind::Class, None),
            ],
            vec![
                edge("make", "Shape", "references", Some("return_type")),
                edge("Circle", "Shape", "implements", None),
                edge("Square", "Shape", "implements", None),
            ],
        );
        assert_eq!(ids(run_pattern(&kg, "factory").unwrap()), vec!["make"]);
    }

    #[test]
    fn observer_detects_subject_holding_listener_interface() {
        let kg = graph(
            vec![
                node("Subject", NodeKind::Class, None),
                node("Listener", NodeKind::Interface, None),
                node("A", NodeKind::Class, None),
                node("B", NodeKind::Class, None),
            ],
            vec![
                edge("Subject", "Listener", "references", Some("field")),
                edge("A", "Listener", "implements", None),
                edge("B", "Listener", "implements", None),
            ],
        );
        assert_eq!(ids(run_pattern(&kg, "observer").unwrap()), vec!["Subject"]);
    }

    #[test]
    fn service_locator_detects_widely_accessed_class() {
        let kg = graph(
            vec![
                node("Locator", NodeKind::Class, Some(0)),
                node("u1", NodeKind::Function, Some(1)),
                node("u2", NodeKind::Function, Some(2)),
                node("u3", NodeKind::Function, Some(3)),
            ],
            vec![
                edge("u1", "Locator", "calls", None),
                edge("u2", "Locator", "calls", None),
                edge("u3", "Locator", "references", None),
            ],
        );
        assert_eq!(
            ids(run_pattern(&kg, "service-locator").unwrap()),
            vec!["Locator"]
        );
    }

    #[test]
    fn all_patterns_expose_a_consistent_node_column() {
        // Every pattern -- the detector-based ones and the SYNQL `god-class`
        // query -- must return a single column named `node`. god-class
        // previously leaked the query binding `c`, an inconsistency visible to
        // CLI/MCP callers.
        let kg = graph(vec![node("C", NodeKind::Class, Some(0))], vec![]);
        for p in ["god-class", "singleton", "factory", "observer"] {
            assert_eq!(
                run_pattern(&kg, p).unwrap().columns,
                vec!["node".to_string()],
                "pattern {p} must return a single `node` column"
            );
        }
    }

    #[test]
    fn negative_cases_return_nothing() {
        // A plain class with none of the patterns.
        let kg = graph(vec![node("Plain", NodeKind::Class, Some(0))], vec![]);
        for p in ["singleton", "factory", "observer", "service-locator"] {
            assert!(
                run_pattern(&kg, p).unwrap().rows.is_empty(),
                "{p} should be empty"
            );
        }
        assert!(run_pattern(&kg, "bogus").is_err());
    }

    #[test]
    fn god_class_matches_large_struct_not_just_class() {
        // A Rust struct (kind=struct, not class) made large by its impl and
        // depending on many symbols. The god-class pattern must flag it even
        // though its kind is `struct` and its declaration span is tiny -- the
        // size comes from effective loc (declaration + members).
        let s = node("Big", NodeKind::Struct, Some(0)); // declaration spans lines 1-3
        let mut m = node("Big_method", NodeKind::Method, Some(0));
        m.source_file = "Big.rs".into(); // same file as the struct
        m.set_span(Span {
            start_line: 4,
            start_col: 1,
            end_line: 604,
            end_col: 1,
        });
        let mut nodes = vec![s, m];
        let mut edges = vec![edge("Big", "Big_method", "method", None)];
        // 21 distinct out-neighbors to clear fan_out > 20.
        for i in 0..21 {
            let dep = format!("dep{i}");
            nodes.push(node(&dep, NodeKind::Struct, Some(0)));
            edges.push(edge("Big", &dep, "references", None));
        }
        let kg = graph(nodes, edges);
        assert_eq!(ids(run_pattern(&kg, "god-class").unwrap()), vec!["Big"]);
    }
}
