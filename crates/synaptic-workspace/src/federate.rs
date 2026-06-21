//! Namespacing + composition of per-member subgraphs.
//!
//! [`prefix_graph`] namespaces a subgraph for the global store: every node id
//! becomes `tag::id`, the node gains a `repo` attribute and a `local_id` (so the
//! original id is recoverable), edges/hyperedges are remapped, and `source_file`
//! is repo-prefixed. [`compose`] prefixes every member, unions them, and
//! collapses shared third-party **external** nodes (no `source_file`) by label so
//! the federated graph has one `serde`/`requests` node, not one per repo.
//!
//! Cross-repo *symbol* resolution (the §4.3 hard part) lives in
//! [`crate::export_surface`]/D3 and runs on the composed graph this module
//! produces.

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use synaptic_core::{GraphData, NodeId};
use synaptic_incremental::union_graphs;

/// Namespace one subgraph under `tag`: `id` → `tag::id`, set `repo`, stash the
/// original id as `local_id`, repo-prefix non-empty `source_file`s, and remap
/// every edge endpoint and hyperedge member.
pub fn prefix_graph(graph: GraphData, tag: &str) -> GraphData {
    let pfx = |id: &NodeId| NodeId(format!("{tag}::{}", id.0));

    let mut nodes = Vec::with_capacity(graph.nodes.len());
    for mut n in graph.nodes {
        let original = n.id.0.clone();
        n.id = pfx(&n.id);
        n.repo = Some(tag.to_string());
        n.extra
            .entry("local_id".to_string())
            .or_insert_with(|| Value::String(original));
        if !n.source_file.is_empty() {
            n.source_file = format!("{tag}/{}", n.source_file);
        }
        nodes.push(n);
    }

    let mut links = Vec::with_capacity(graph.links.len());
    for mut e in graph.links {
        e.source = pfx(&e.source);
        e.target = pfx(&e.target);
        if !e.source_file.is_empty() {
            e.source_file = format!("{tag}/{}", e.source_file);
        }
        links.push(e);
    }

    let mut hyperedges = Vec::with_capacity(graph.hyperedges.len());
    for mut h in graph.hyperedges {
        h.id = format!("{tag}::{}", h.id);
        h.nodes = h.nodes.iter().map(pfx).collect();
        hyperedges.push(h);
    }

    GraphData {
        directed: graph.directed,
        multigraph: graph.multigraph,
        graph: graph.graph,
        nodes,
        links,
        hyperedges,
        built_at_commit: graph.built_at_commit,
    }
}

/// Collapse `source_file`-less **external** nodes that share a label onto the
/// first-seen one (third-party deps appearing in multiple repos), rewiring edges
/// and dropping the self-loops that collapse can produce. This is the external
/// dedup applied when adding a repo to the global store.
pub fn dedup_externals(mut g: GraphData) -> GraphData {
    let mut canon: HashMap<String, NodeId> = HashMap::new();
    let mut remap: HashMap<NodeId, NodeId> = HashMap::new();
    for n in &g.nodes {
        if n.source_file.is_empty() && !n.label.is_empty() {
            match canon.get(&n.label) {
                None => {
                    canon.insert(n.label.clone(), n.id.clone());
                }
                Some(c) => {
                    remap.insert(n.id.clone(), c.clone());
                }
            }
        }
    }
    if remap.is_empty() {
        return g;
    }

    g.nodes.retain(|n| !remap.contains_key(&n.id));

    let links = std::mem::take(&mut g.links);
    let mut seen: HashSet<(NodeId, NodeId, String)> = HashSet::new();
    let mut kept = Vec::with_capacity(links.len());
    for mut e in links {
        let s = remap.get(&e.source);
        let t = remap.get(&e.target);
        let collapsed = s.is_some() || t.is_some();
        if let Some(c) = s {
            e.source = c.clone();
        }
        if let Some(c) = t {
            e.target = c.clone();
        }
        // Drop only self-loops introduced by the collapse; a legitimate
        // pre-existing self-loop (e.g. a recursive `f calls f`) must survive.
        if collapsed && e.source == e.target {
            continue;
        }
        if seen.insert((e.source.clone(), e.target.clone(), e.relation.clone())) {
            kept.push(e);
        }
    }
    g.links = kept;

    for h in &mut g.hyperedges {
        for m in &mut h.nodes {
            if let Some(c) = remap.get(m) {
                *m = c.clone();
            }
        }
    }
    g
}

/// Federate per-member subgraphs into one graph: prefix each with its tag, union
/// them, then dedup shared externals. The federated graph is *not* re-clustered
/// here (the build step does that after cross-repo resolution).
pub fn compose(subgraphs: Vec<(String, GraphData)>) -> GraphData {
    let mut iter = subgraphs.into_iter().map(|(tag, g)| prefix_graph(g, &tag));
    let Some(first) = iter.next() else {
        return GraphData::default();
    };
    let unioned = iter.fold(first, union_graphs);
    dedup_externals(unioned)
}

/// Like [`compose`] but **without** external dedup — the `merge-graphs` path,
/// which composes inputs verbatim.
pub fn compose_no_dedup(subgraphs: Vec<(String, GraphData)>) -> GraphData {
    let mut iter = subgraphs.into_iter().map(|(tag, g)| prefix_graph(g, &tag));
    let Some(first) = iter.next() else {
        return GraphData::default();
    };
    iter.fold(first, union_graphs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Confidence, Edge, FileType, Node};

    fn node(id: &str, label: &str, source_file: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: source_file.into(),
            source_location: None,
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
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    fn gd(nodes: Vec<Node>, links: Vec<Edge>) -> GraphData {
        GraphData {
            nodes,
            links,
            ..Default::default()
        }
    }

    #[test]
    fn prefix_namespaces_ids_repo_local_id_and_source_file() {
        let g = gd(
            vec![node("f", "Foo", "src/foo.py")],
            vec![edge("f", "f", "calls")],
        );
        let p = prefix_graph(g, "billing");
        assert_eq!(p.nodes[0].id.0, "billing::f");
        assert_eq!(p.nodes[0].repo.as_deref(), Some("billing"));
        assert_eq!(p.nodes[0].extra.get("local_id").unwrap(), "f");
        assert_eq!(p.nodes[0].source_file, "billing/src/foo.py");
        assert_eq!(p.links[0].source.0, "billing::f");
        assert_eq!(p.links[0].target.0, "billing::f");
        // label is untouched (display).
        assert_eq!(p.nodes[0].label, "Foo");
    }

    #[test]
    fn external_nodes_keep_empty_source_file_through_prefix() {
        let g = gd(vec![node("ext_serde", "serde", "")], vec![]);
        let p = prefix_graph(g, "a");
        assert_eq!(p.nodes[0].id.0, "a::ext_serde");
        assert_eq!(p.nodes[0].source_file, ""); // not prefixed, so still dedup-able
    }

    #[test]
    fn compose_namespaces_two_repos_and_keeps_both() {
        let a = gd(vec![node("main", "main", "a.rs")], vec![]);
        let b = gd(vec![node("main", "main", "b.rs")], vec![]);
        let fed = compose(vec![("repoa".into(), a), ("repob".into(), b)]);
        let ids: Vec<&str> = fed.nodes.iter().map(|n| n.id.0.as_str()).collect();
        assert!(
            ids.contains(&"repoa::main") && ids.contains(&"repob::main"),
            "{ids:?}"
        );
    }

    #[test]
    fn compose_collapses_shared_externals_and_rewires() {
        // Both repos depend on external "serde"; one node survives, the edge from
        // repob's code rewires onto repoa's serde node.
        let a = gd(
            vec![node("lib", "lib", "a.rs"), node("ext_serde", "serde", "")],
            vec![edge("lib", "ext_serde", "imports")],
        );
        let b = gd(
            vec![node("app", "app", "b.rs"), node("ext_serde", "serde", "")],
            vec![edge("app", "ext_serde", "imports")],
        );
        let fed = compose(vec![("repoa".into(), a), ("repob".into(), b)]);
        let serde_nodes: Vec<&Node> = fed.nodes.iter().filter(|n| n.label == "serde").collect();
        assert_eq!(serde_nodes.len(), 1, "one shared external survives");
        let canon = &serde_nodes[0].id;
        // repob's import edge now points at the surviving (repoa) serde node.
        assert!(
            fed.links
                .iter()
                .any(|e| e.source.0 == "repob::app" && &e.target == canon),
            "edge rewired onto canonical external"
        );
    }

    #[test]
    fn compose_no_dedup_keeps_every_external_copy() {
        let a = gd(vec![node("ext_x", "x", "")], vec![]);
        let b = gd(vec![node("ext_x", "x", "")], vec![]);
        let fed = compose_no_dedup(vec![("a".into(), a), ("b".into(), b)]);
        assert_eq!(fed.nodes.iter().filter(|n| n.label == "x").count(), 2);
    }

    #[test]
    fn compose_empty_is_empty() {
        let fed = compose(vec![]);
        assert!(fed.nodes.is_empty() && fed.links.is_empty());
    }
}
