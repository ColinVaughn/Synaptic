//! `update` command(s) split from main.rs.

use crate::commands::extract::write_outputs;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use synaptic_core::GraphData;
use synaptic_graph::analyze;
use synaptic_incremental::{
    drain_pending, merge_changed_paths, queue_pending, rebuild, try_acquire_lock, ChangeSet,
    RebuildOptions,
};

pub(crate) fn run_update(
    paths: Vec<PathBuf>,
    full: bool,
    directed_flag: bool,
    force: bool,
) -> Result<()> {
    // The post-commit hook passes the changed files via SYNAPTIC_CHANGED
    // (newline-delimited) instead of argv, so paths containing spaces or
    // shell-glob characters survive intact (no word-splitting/glob-expansion).
    let paths = if paths.is_empty() && !full {
        match std::env::var("SYNAPTIC_CHANGED") {
            Ok(v) if !v.trim().is_empty() => v
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .collect(),
            _ => paths,
        }
    } else {
        paths
    };
    let root = std::env::current_dir().context("resolving current directory")?;
    let out_dir = root.join("synaptic-out");
    let graph_path = out_dir.join("graph.json");

    // Serialize concurrent rebuilds (e.g. a burst of git hooks). If another
    // rebuild holds the lock, queue our changed paths for it to drain and return.
    let _lock = match try_acquire_lock(&out_dir).context("acquiring rebuild lock")? {
        Some(guard) => guard,
        None => {
            if !full && !paths.is_empty() {
                queue_pending(&out_dir, &paths).context("queueing changed paths")?;
                println!("A rebuild is in progress; queued {} path(s).", paths.len());
            } else {
                println!("A rebuild is in progress; skipping (it will cover current state).");
            }
            return Ok(());
        }
    };

    // Inherit the existing graph (and its `directed` flag) when present.
    let existing: Option<GraphData> = fs::read_to_string(&graph_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    let directed = existing
        .as_ref()
        .map(|g| g.directed)
        .unwrap_or(directed_flag);

    // Absorb any paths queued by callers that couldn't get the lock, then decide.
    let queued = drain_pending(&out_dir).context("draining pending changes")?;
    let changes = if full || existing.is_none() {
        ChangeSet::Full
    } else {
        let merged = merge_changed_paths(queued, paths);
        if merged.is_empty() {
            ChangeSet::Full
        } else {
            ChangeSet::Incremental(merged)
        }
    };

    let opts = RebuildOptions {
        root: root.clone(),
        directed,
        force,
    };
    let outcome = rebuild(&opts, &changes, existing.as_ref())
        .map_err(|e| anyhow::anyhow!("rebuild failed: {e}"))?;

    if !outcome.changed {
        println!(
            "No changes — graph is up to date ({} nodes).",
            outcome.kg.node_count()
        );
        return Ok(());
    }

    let analysis = analyze(&outcome.kg, &outcome.communities, &BTreeMap::new());
    let extras = write_outputs(
        &outcome.kg,
        &analysis,
        &outcome.communities,
        &BTreeMap::new(),
        &out_dir,
        false,
        false,
    )?;
    println!(
        "Rebuilt: {} nodes · {} edges · {} communities (re-extracted {}, evicted {} source(s))",
        outcome.kg.node_count(),
        outcome.kg.edge_count(),
        outcome.communities.len(),
        outcome.reextracted,
        outcome.evicted_sources
    );
    println!("Wrote {}/{{{}}}", out_dir.display(), extras);
    Ok(())
}
