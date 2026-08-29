//! `chart` command: turn an existing graph.json into a high-level architecture map.

use crate::commands::common::{load_scoped_graph, write_file};
use anyhow::Result;
use std::path::{Path, PathBuf};
use synaptic_graph::{ClusterOptions, apply_communities, cluster, is_structural_node};

pub(crate) fn run_chart(
    graph: Option<PathBuf>,
    out: Option<PathBuf>,
    repo: Option<String>,
    max_communities: usize,
) -> Result<()> {
    if !(1..=24).contains(&max_communities) {
        anyhow::bail!("--max-communities must be between 1 and 24");
    }
    let graph_path = graph.unwrap_or_else(|| PathBuf::from("synaptic-out").join("graph.json"));
    let mut kg = load_scoped_graph(&graph_path, repo.as_deref())?;
    if !kg.nodes().any(is_structural_node) {
        anyhow::bail!("the graph has no structural nodes to chart");
    }
    if kg
        .nodes()
        .filter(|node| is_structural_node(node))
        .any(|node| node.community.is_none())
    {
        let communities = cluster(&kg, &ClusterOptions::default());
        apply_communities(&mut kg, &communities);
    }
    let base = graph_path.parent().unwrap_or_else(|| Path::new("."));
    let output = out.unwrap_or_else(|| base.join("chart.html"));
    write_file("chart.html", &output, |path| {
        synaptic_output::to_chart(&kg, path, max_communities)
    })
}
