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

use std::collections::HashMap;

use serde_json::Value;
use synaptic_core::{EdgeKey, EdgeSiteAccumulator, GraphData, NodeId};
use synaptic_incremental::union_graphs_many;

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
        let mut aggregated_sites = e
            .extra
            .contains_key("sites")
            .then(|| EdgeSiteAccumulator::new(&e));
        if !e.source_file.is_empty() {
            e.source_file = format!("{tag}/{}", e.source_file);
        }
        if let Some(sites) = &mut aggregated_sites {
            sites.rewrite(|site| {
                if !site.source_file.is_empty() {
                    site.source_file = format!("{tag}/{}", site.source_file);
                }
            });
        }
        if let Some(sites) = aggregated_sites {
            sites.apply_to(&mut e);
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

/// The identity under which an external node deduplicates: its `_node_type`
/// (so a `command` stub named `orders` never merges with the SQL `table`
/// `orders` -- 2026-07 audit) plus a canonical label -- `_route_canon` for
/// routes (so `/users/:id` meets `/users/{id}` cross-repo), else the label
/// case-folded (same-repo identity is case-folded; cross-repo must agree).
fn external_dedup_key(n: &synaptic_core::Node) -> (String, String) {
    let node_type = n
        .extra
        .get("_node_type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let label = n
        .extra
        .get("_route_canon")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| n.label.to_ascii_lowercase());
    (node_type, label)
}

/// Collapse `source_file`-less **external** nodes that share an identity onto
/// the first-seen one (third-party deps appearing in multiple repos), rewiring
/// edges and dropping the self-loops that collapse can produce. This is the
/// external dedup applied when adding a repo to the global store.
pub fn dedup_externals(mut g: GraphData) -> GraphData {
    let mut canon: HashMap<(String, String), NodeId> = HashMap::new();
    let mut remap: HashMap<NodeId, NodeId> = HashMap::new();
    for n in &g.nodes {
        if n.source_file.is_empty() && !n.label.is_empty() {
            let key = external_dedup_key(n);
            match canon.get(&key) {
                None => {
                    canon.insert(key, n.id.clone());
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
    // Context is part of edge identity: a GET and a POST calls_service between
    // the same fn and route are two couplings, not a duplicate (wave-2 low).
    let mut seen: HashMap<EdgeKey, usize> = HashMap::new();
    let mut kept: Vec<synaptic_core::Edge> = Vec::with_capacity(links.len());
    let mut site_accumulators: Vec<Option<EdgeSiteAccumulator>> = Vec::with_capacity(links.len());
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
        let key = EdgeKey::new(&e, true);
        if let Some(&index) = seen.get(&key) {
            if site_accumulators[index].is_none() {
                site_accumulators[index] = Some(EdgeSiteAccumulator::new(&kept[index]));
            }
            site_accumulators[index]
                .as_mut()
                .expect("duplicate edge has a site accumulator")
                .include_edge(&e);
        } else {
            seen.insert(key, kept.len());
            kept.push(e);
            site_accumulators.push(None);
        }
    }
    for (edge, sites) in kept.iter_mut().zip(site_accumulators) {
        if let Some(sites) = sites {
            sites.apply_to(edge);
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
    let unioned = union_graphs_many(
        subgraphs
            .into_iter()
            .map(|(tag, graph)| prefix_graph(graph, &tag)),
    );
    dedup_externals(unioned)
}

/// Like [`compose`] but **without** external dedup — the `merge-graphs` path,
/// which composes inputs verbatim.
pub fn compose_no_dedup(subgraphs: Vec<(String, GraphData)>) -> GraphData {
    union_graphs_many(
        subgraphs
            .into_iter()
            .map(|(tag, graph)| prefix_graph(graph, &tag)),
    )
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
        assert_eq!(p.links[0].source_file, "billing/a.py");
        // label is untouched (display).
        assert_eq!(p.nodes[0].label, "Foo");
    }

    #[test]
    fn prefix_rewrites_every_aggregated_edge_site() {
        let mut first = edge("f", "f", "calls");
        first.source_location = Some("L1".into());
        let mut second = first.clone();
        second.source_file = "src/other.py".into();
        second.source_location = Some("L2".into());
        first.merge_sites_from(&second);

        let prefixed = prefix_graph(
            gd(vec![node("f", "Foo", "src/foo.py")], vec![first]),
            "billing",
        );
        let sites = prefixed.links[0].sites();

        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].source_file, "billing/a.py");
        assert_eq!(sites[1].source_file, "billing/src/other.py");
        assert_eq!(sites[0].source_location.as_deref(), Some("L1"));
        assert_eq!(sites[1].source_location.as_deref(), Some("L2"));
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

    fn typed_node(id: &str, label: &str, node_type: &str) -> Node {
        let mut n = node(id, label, "");
        n.extra
            .insert("_node_type".into(), serde_json::json!(node_type));
        n
    }

    /// D2 (2026-07 audit): label-equal externals of DIFFERENT kinds must not
    /// merge -- a `command` stub named `orders` is not the SQL `table` `orders`.
    #[test]
    fn dedup_requires_matching_node_type() {
        let a = gd(vec![typed_node("cmd_orders", "orders", "command")], vec![]);
        let b = gd(vec![typed_node("sql_orders", "orders", "table")], vec![]);
        let fed = compose(vec![("a".into(), a), ("b".into(), b)]);
        assert_eq!(
            fed.nodes.iter().filter(|n| n.label == "orders").count(),
            2,
            "different _node_type -> no merge"
        );
    }

    /// D2: boundary labels differing only by case merge cross-repo (same-repo
    /// identity is case-folded; cross-repo must agree).
    #[test]
    fn dedup_case_insensitive_for_externals() {
        let a = gd(
            vec![typed_node("event_save", "event #Save", "event_channel")],
            vec![],
        );
        let b = gd(
            vec![typed_node("event_save", "event #save", "event_channel")],
            vec![],
        );
        let fed = compose(vec![("a".into(), a), ("b".into(), b)]);
        assert_eq!(
            fed.nodes
                .iter()
                .filter(|n| n.label.eq_ignore_ascii_case("event #save"))
                .count(),
            1,
            "case-only label difference merges"
        );
    }

    /// D2: route nodes merge by their canonical path (`_route_canon`), so an
    /// Express `/users/:id` in repo A meets an axum `/users/{id}` in repo B.
    #[test]
    fn dedup_routes_by_canon() {
        let mut ra = typed_node("route_a", "/users/:id", "route");
        ra.extra
            .insert("_route_canon".into(), serde_json::json!("/users/{p}"));
        let mut rb = typed_node("route_b", "/users/{id}", "route");
        rb.extra
            .insert("_route_canon".into(), serde_json::json!("/users/{p}"));
        let a = gd(vec![ra], vec![]);
        let b = gd(vec![rb], vec![]);
        let fed = compose(vec![("a".into(), a), ("b".into(), b)]);
        assert_eq!(
            fed.nodes
                .iter()
                .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
                .count(),
            1,
            "equivalent templates merge cross-repo by canon"
        );
    }
}
