//! `merge` command(s) split from main.rs.

use anyhow::Result;
use std::fs;
use std::path::PathBuf;

pub(crate) fn run_merge_graphs(graphs: Vec<PathBuf>, out: Option<PathBuf>) -> Result<()> {
    if graphs.is_empty() {
        anyhow::bail!("provide at least one graph.json to merge");
    }
    let out = out.unwrap_or_else(|| PathBuf::from("codegraph-out/merged-graph.json"));
    if let Some(p) = out.parent() {
        fs::create_dir_all(p).ok();
    }
    let report = codegraph_workspace::merge_graphs::merge_graph_files(&graphs, &out)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!(
        "Merged {} graph(s) → {} ({} nodes, {} edges)\ntags: {}",
        graphs.len(),
        report.out.display(),
        report.node_count,
        report.edge_count,
        report.tags.join(", ")
    );
    Ok(())
}
