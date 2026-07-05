//! `migrate` command: build a sharded redb store from an existing `graph.json`.

use crate::commands::common::{default_graph_path, store_dir_for, write_store};
use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use synaptic_core::GraphData;

/// `synaptic migrate`: read `graph.json`, split it into per-repo shards, and
/// write the redb shard store. Idempotent: re-running rewrites changed shards.
pub(crate) fn run_migrate(graph: Option<PathBuf>, store: Option<PathBuf>) -> Result<()> {
    let graph_path = default_graph_path(graph);
    let store_dir = store.unwrap_or_else(|| store_dir_for(&graph_path));

    let text = fs::read_to_string(&graph_path).with_context(|| {
        format!(
            "reading {} (run `synaptic extract` first?)",
            graph_path.display()
        )
    })?;
    let gd: GraphData = serde_json::from_str(&text).context("parsing graph.json")?;

    let report = write_store(&gd, &store_dir)?;

    let bridge_note = if report.bridge_edges > 0 {
        format!(", {} cross-repo bridge edge(s)", report.bridge_edges)
    } else {
        String::new()
    };
    let skip_note = if report.skipped > 0 {
        format!(", {} unchanged shard(s) skipped", report.skipped)
    } else {
        String::new()
    };
    println!(
        "Migrated {} into {} shard(s) at {}{}{}",
        graph_path.display(),
        report.shard_tags.len(),
        store_dir.display(),
        skip_note,
        bridge_note
    );
    Ok(())
}
