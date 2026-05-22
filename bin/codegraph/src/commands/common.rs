//! `common` command(s) split from main.rs.

use anyhow::{Context, Result};
use codegraph_core::{GraphData, NodeId};
use codegraph_graph::KnowledgeGraph;
use std::fs;
use std::path::{Path, PathBuf};

/// Run a single-file writer against `path` and report it.
pub(crate) fn write_file(
    label: &str,
    path: &Path,
    write: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<()> {
    write(path).with_context(|| format!("writing {label}"))?;
    println!("Wrote {}", path.display());
    Ok(())
}

pub(crate) fn default_graph_path(graph: Option<PathBuf>) -> PathBuf {
    graph.unwrap_or_else(|| PathBuf::from("codegraph-out/graph.json"))
}

pub(crate) fn load_graph(path: &Path) -> Result<KnowledgeGraph> {
    let text = fs::read_to_string(path).with_context(|| {
        format!(
            "reading {} (run `codegraph extract` first?)",
            path.display()
        )
    })?;
    let gd: GraphData = serde_json::from_str(&text).context("parsing graph.json")?;
    Ok(KnowledgeGraph::from_graph_data(gd))
}

/// Load a graph, optionally scoped to one federated member (`--repo`). Scoping
/// drops nodes from other repos + the cross-repo edges that span them.
pub(crate) fn load_scoped_graph(path: &Path, repo: Option<&str>) -> Result<KnowledgeGraph> {
    let kg = load_graph(path)?;
    match repo {
        Some(r) => {
            let scoped = codegraph_workspace::repo_scope::filter_repo(&kg.to_graph_data(), r);
            Ok(KnowledgeGraph::from_graph_data(scoped))
        }
        None => Ok(kg),
    }
}

/// Resolve a user-supplied node id or label to a node id.
pub(crate) fn resolve(kg: &KnowledgeGraph, arg: &str) -> Option<NodeId> {
    let nid = NodeId(arg.to_string());
    if kg.contains_node(&nid) {
        return Some(nid);
    }
    kg.nodes().find(|n| n.label == arg).map(|n| n.id.clone())
}

pub(crate) fn label_or_id(kg: &KnowledgeGraph, id: &NodeId) -> String {
    kg.node(id)
        .map(|n| n.label.clone())
        .unwrap_or_else(|| id.0.clone())
}
