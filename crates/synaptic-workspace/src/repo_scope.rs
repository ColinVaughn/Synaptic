//! Repo-scoped views of a federated graph. The federated
//! `graph.json` carries a `repo` tag on every node, so scoping needs only that
//! field — no extra index.

use std::collections::BTreeMap;

use serde::Serialize;
use synaptic_core::{GraphData, NodeId};

/// Per-repo counts for `list_repos` / `repo_stats`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoStat {
    pub repo: String,
    pub nodes: usize,
    /// Edges whose *source* node belongs to this repo (so a cross-repo edge is
    /// counted under the importing repo).
    pub edges: usize,
}

/// Group a federated graph's nodes/edges by `repo`, sorted by tag. Nodes without
/// a `repo` (a single-repo graph) are ignored.
pub fn list_repos(g: &GraphData) -> Vec<RepoStat> {
    let node_repo: BTreeMap<&NodeId, &str> = g
        .nodes
        .iter()
        .filter_map(|n| n.repo.as_deref().map(|r| (&n.id, r)))
        .collect();

    let mut counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for n in &g.nodes {
        if let Some(r) = n.repo.as_deref() {
            counts.entry(r.to_string()).or_default().0 += 1;
        }
    }
    for e in &g.links {
        if let Some(r) = node_repo.get(&e.source) {
            counts.entry(r.to_string()).or_default().1 += 1;
        }
    }
    counts
        .into_iter()
        .map(|(repo, (nodes, edges))| RepoStat { repo, nodes, edges })
        .collect()
}

/// A new graph containing only `repo`'s nodes and the edges/hyperedges fully
/// inside it (cross-repo edges are dropped, since their other end is gone).
pub fn filter_repo(g: &GraphData, repo: &str) -> GraphData {
    let keep: std::collections::HashSet<&NodeId> = g
        .nodes
        .iter()
        .filter(|n| n.repo.as_deref() == Some(repo))
        .map(|n| &n.id)
        .collect();
    GraphData {
        directed: g.directed,
        multigraph: g.multigraph,
        graph: g.graph.clone(),
        nodes: g
            .nodes
            .iter()
            .filter(|n| keep.contains(&n.id))
            .cloned()
            .collect(),
        links: g
            .links
            .iter()
            .filter(|e| keep.contains(&e.source) && keep.contains(&e.target))
            .cloned()
            .collect(),
        hyperedges: g
            .hyperedges
            .iter()
            .filter(|h| h.nodes.iter().all(|m| keep.contains(m)))
            .cloned()
            .collect(),
        built_at_commit: g.built_at_commit.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{Confidence, Edge, FileType, Node};

    fn node(id: &str, repo: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            label: id.into(),
            file_type: FileType::Code,
            source_file: format!("{id}.rs"),
            source_location: None,
            community: None,
            repo: Some(repo.into()),
            extra: Map::new(),
        }
    }

    fn edge(s: &str, t: &str) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: "calls".into(),
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

    fn graph() -> GraphData {
        GraphData {
            nodes: vec![node("a::1", "a"), node("a::2", "a"), node("b::1", "b")],
            links: vec![edge("a::1", "a::2"), edge("a::2", "b::1")],
            ..Default::default()
        }
    }

    #[test]
    fn list_repos_counts_nodes_and_source_edges() {
        let stats = list_repos(&graph());
        assert_eq!(stats.len(), 2);
        assert_eq!(
            stats[0],
            RepoStat {
                repo: "a".into(),
                nodes: 2,
                edges: 2
            }
        );
        assert_eq!(
            stats[1],
            RepoStat {
                repo: "b".into(),
                nodes: 1,
                edges: 0
            }
        );
    }

    #[test]
    fn filter_repo_keeps_only_in_repo_nodes_and_internal_edges() {
        let a = filter_repo(&graph(), "a");
        assert_eq!(a.nodes.len(), 2);
        // The a::2 -> b::1 cross-repo edge is dropped (b::1 filtered out).
        assert_eq!(a.links.len(), 1);
        assert_eq!(a.links[0].source.0, "a::1");
    }
}
