//! `common` command(s) split from main.rs.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use synaptic_core::{GraphData, NodeId};
use synaptic_graph::KnowledgeGraph;

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
    graph.unwrap_or_else(|| PathBuf::from("synaptic-out/graph.json"))
}

pub(crate) fn load_graph(path: &Path) -> Result<KnowledgeGraph> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading {} (run `synaptic extract` first?)", path.display()))?;
    let gd: GraphData = serde_json::from_str(&text).context("parsing graph.json")?;
    Ok(KnowledgeGraph::from_graph_data(gd))
}

/// Load a graph, optionally scoped to one federated member (`--repo`). Scoping
/// drops nodes from other repos + the cross-repo edges that span them.
pub(crate) fn load_scoped_graph(path: &Path, repo: Option<&str>) -> Result<KnowledgeGraph> {
    let kg = load_graph(path)?;
    match repo {
        Some(r) => {
            let scoped = synaptic_workspace::repo_scope::filter_repo(&kg.to_graph_data(), r);
            Ok(KnowledgeGraph::from_graph_data(scoped))
        }
        None => Ok(kg),
    }
}

/// Resolve a user-supplied name/id to a single node, or a human-readable error
/// message. Uses the same shared resolver as the MCP server, so the CLI and MCP
/// report ambiguity identically (candidate ids instead of a bare "not found").
pub(crate) fn resolve_or_message(
    kg: &KnowledgeGraph,
    arg: &str,
) -> std::result::Result<NodeId, String> {
    match synaptic_query::resolve_detailed(kg, arg) {
        synaptic_query::Resolution::Unique(id) => Ok(id),
        synaptic_query::Resolution::Ambiguous(ids) => {
            let shown: Vec<String> = ids.iter().take(10).map(|i| i.0.clone()).collect();
            let more = if ids.len() > 10 {
                format!(", +{} more", ids.len() - 10)
            } else {
                String::new()
            };
            Err(format!(
                "'{arg}' is ambiguous - {} candidates: [{}{}]. Pass a node id to disambiguate.",
                ids.len(),
                shown.join(", "),
                more
            ))
        }
        synaptic_query::Resolution::NotFound => Err(format!("No node matches '{arg}'.")),
    }
}

pub(crate) fn label_or_id(kg: &KnowledgeGraph, id: &NodeId) -> String {
    kg.node(id)
        .map(|n| n.label.clone())
        .unwrap_or_else(|| id.0.clone())
}
