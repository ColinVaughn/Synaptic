//! `update` command(s) split from main.rs.

use crate::commands::extract::write_outputs;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use synaptic_core::GraphData;
use synaptic_detect::{detect_inputs, DetectResult};
use synaptic_graph::analyze;
use synaptic_incremental::{
    drain_pending, drain_queued_rounds, manifest_path, merge_changed_paths, queue_pending,
    rebuild_with_detect, try_acquire_lock, ChangeSet, RebuildOptions,
};

/// Backstop against a writer that re-queues on every round.
const QUEUE_ROUNDS_MAX: usize = 10;

pub(crate) fn run_update(
    paths: Vec<PathBuf>,
    full: bool,
    directed_flag: bool,
    force: bool,
    artifacts: bool,
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
    // The bare-update path already walked the tree to diff the manifest; its
    // detect result feeds the first rebuild round instead of a second walk.
    let mut first_det: Option<DetectResult> = None;
    let changes = if full || existing.is_none() {
        ChangeSet::Full
    } else {
        let merged = merge_changed_paths(queued, paths);
        if merged.is_empty() {
            // Bare `update`: catch up from the manifest diff (the serve
            // path's semantics) instead of a silent full rebuild; `--full`
            // remains the explicit from-scratch rebuild. No prior manifest
            // means the graph's drift is UNKNOWN (older binary, deleted
            // manifest) -- trusting the bootstrap baseline would mask every
            // edit made since that graph was built, so rebuild fully.
            let report = synaptic_incremental::detect_changes(&out_dir, &root);
            if report.bootstrapped {
                println!("No provenance manifest; rebuilding fully to be safe.");
                first_det = Some(report.det);
                ChangeSet::Full
            } else if report.is_empty() {
                println!("No changes detected since the last build.");
                return Ok(());
            } else {
                println!(
                    "Catching up: {} file(s) changed since the last build.",
                    report.changed_paths().len()
                );
                let paths = report.changed_paths();
                first_det = Some(report.det);
                ChangeSet::Incremental(paths)
            }
        } else {
            ChangeSet::Incremental(merged)
        }
    };

    let opts = RebuildOptions {
        root: root.clone(),
        directed,
        force,
    };
    // One rebuild round. The rebuild builds its provenance manifest BEFORE
    // extracting (a mid-rebuild edit stays detectable) and advances only what
    // it ingested; graph.json is written before the manifest so provenance
    // never runs ahead of the graph on disk (a failed write leaves the round
    // re-detectable instead of stamped as seen).
    let run_round = |changes: &ChangeSet,
                     existing: Option<&GraphData>,
                     det: Option<DetectResult>|
     -> Result<_> {
        let det = det.unwrap_or_else(|| detect_inputs(&root));
        let outcome = rebuild_with_detect(&opts, changes, existing, &det)
            .map_err(|e| anyhow::anyhow!("rebuild failed: {e}"))?;
        for key in &outcome.unreadable {
            eprintln!("warning: could not read {key}; kept its previous nodes (will retry)");
        }
        if outcome.changed {
            synaptic_output::to_json(&outcome.kg, &out_dir.join("graph.json"))
                .context("writing graph.json")?;
            super::common::warn_if_over_caps(&out_dir.join("graph.json"), outcome.kg.node_count());
        }
        if let Err(e) = outcome.manifest.save(&manifest_path(&out_dir)) {
            eprintln!("note: could not write serve provenance manifest: {e}");
        }
        Ok(outcome)
    };

    let mut outcome = run_round(&changes, existing.as_ref(), first_det)?;
    let mut any_changed = outcome.changed;
    let mut reextracted = outcome.reextracted;
    let mut evicted = outcome.evicted_sources;

    // Cover paths queued by lock losers while our rebuild ran; they'd otherwise
    // sit in the queue until the next update invocation.
    let (_, clean) = drain_queued_rounds::<anyhow::Error>(&out_dir, QUEUE_ROUNDS_MAX, |paths| {
        let gd = outcome.kg.to_graph_data();
        let next = run_round(&ChangeSet::Incremental(paths), Some(&gd), None)?;
        any_changed |= next.changed;
        reextracted += next.reextracted;
        evicted += next.evicted_sources;
        outcome = next;
        Ok(())
    })?;
    if !clean {
        println!("Changes kept arriving during the rebuild; run `synaptic update` again to cover the remainder.");
    }

    if !any_changed {
        println!(
            "No changes — graph is up to date ({} nodes).",
            outcome.kg.node_count()
        );
        return Ok(());
    }

    println!(
        "Rebuilt: {} nodes · {} edges · {} communities (re-extracted {}, evicted {} source(s))",
        outcome.kg.node_count(),
        outcome.kg.edge_count(),
        outcome.communities.len(),
        reextracted,
        evicted
    );
    // Keep an existing sharded store fresh so redb-backed reads never answer
    // from a stale shard (no store dir means the user opted out at extract
    // time). Unchanged shards are hash-skipped, so this is cheap. On failure
    // the store's manifest stays older than graph.json and the auto backend
    // falls back to parsing graph.json, exactly as the warning says.
    let store_dir = out_dir.join("store");
    if store_dir.join("manifest.json").exists() {
        match super::common::write_store(&outcome.kg.to_graph_data(), &store_dir) {
            Ok(report) => println!(
                "Refreshed {}/store ({} shard(s))",
                out_dir.display(),
                report.shard_tags.len()
            ),
            Err(e) => eprintln!(
                "warning: could not refresh the sharded store ({e}); reads fall back to graph.json"
            ),
        }
    }
    // graph.json was already written by each changed round. An update runs on
    // every save in the watch/hook flows; the visual artifact suite (SVG, 3D
    // HTML, GraphML, ...) dominates that cost and nobody reads it mid-edit, so
    // graph.json alone is the default.
    if !artifacts {
        println!(
            "Wrote {}/graph.json (pass --artifacts for the full artifact suite)",
            out_dir.display()
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
    println!("Wrote {}/{{{}}}", out_dir.display(), extras);
    Ok(())
}
