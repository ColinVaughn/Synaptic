//! `watch` command(s) split from main.rs.

use crate::commands::update::run_update;
use anyhow::{Context, Result};
use std::path::Path;
use synaptic_incremental::{is_rebuildable, should_ignore_path, ChangeBatch, DEBOUNCE_MS};

/// Watch the working tree and rebuild incrementally on change. Debounces a burst
/// of saves (`DEBOUNCE_MS`) into one rebuild, ignores the output/VCS/build
/// subtrees (so writing `graph.json` can't self-trigger), and routes each batch
/// of changed **code** files through [`run_update`] (which holds the rebuild lock
/// and writes artifacts).
pub(crate) fn run_watch(
    directed: bool,
    force: bool,
    artifacts: bool,
    debounce_ms: Option<u64>,
) -> Result<()> {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc::channel;
    use std::time::Duration;

    let root = std::env::current_dir().context("resolving current directory")?;
    let debounce = debounce_ms
        .or_else(|| {
            std::env::var("SYNAPTIC_WATCH_DEBOUNCE_MS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(DEBOUNCE_MS);

    let (tx, rx) = channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .context("creating filesystem watcher")?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .with_context(|| format!("watching {}", root.display()))?;

    // Catch up on edits made before the watcher started (and on any events a
    // past session dropped): a bare update diffs the manifest and rebuilds
    // exactly what changed. Watching is already live, so an edit landing
    // mid-catch-up queues as a normal event instead of falling in a gap.
    if let Err(e) = run_update(Vec::new(), false, directed, force, artifacts, false) {
        eprintln!("startup catch-up failed: {e}");
    }

    println!(
        "Watching {} for changes (debounce {debounce}ms; Ctrl-C to stop)…",
        root.display()
    );

    // Block until the first change, then drain a quiet window to batch a burst.
    // Loop ends when the watcher is dropped (channel closed).
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.clone());
    while let Ok(first) = rx.recv() {
        let mut batch = ChangeBatch::new();
        let mut rescan = false;
        let mut record = |res: notify::Result<notify::Event>, rescan: &mut bool| {
            match res {
                Ok(ev) => {
                    // A rescan notice means events were dropped (buffer
                    // overflow on a huge change): fall back to a manifest
                    // catch-up rather than trusting the partial batch.
                    if ev.need_rescan() {
                        *rescan = true;
                        return;
                    }
                    for p in ev.paths {
                        // Filter on the repo-RELATIVE path: a noise dir name in
                        // an ancestor of the root (a checkout under /build/app)
                        // must not ignore the whole tree. Stripping tries the
                        // canonical and raw root forms (notify and current_dir
                        // may disagree on canonicalization); watch runs from
                        // the root, so the relative path resolves for the
                        // rebuild. If no form strips, keep the absolute path
                        // (self-trigger safety beats the ancestor-name hazard).
                        let rel = p
                            .strip_prefix(&canon_root)
                            .or_else(|_| p.strip_prefix(&root))
                            .map(Path::to_path_buf)
                            .unwrap_or(p);
                        if !should_ignore_path(&rel) && is_rebuildable(&rel) {
                            batch.record(rel);
                        }
                    }
                }
                Err(_) => *rescan = true,
            }
        };
        record(first, &mut rescan);
        while let Ok(ev) = rx.recv_timeout(Duration::from_millis(debounce)) {
            record(ev, &mut rescan);
        }

        if rescan {
            println!("\nWatcher lost events → catching up from the manifest…");
            if let Err(e) = run_update(Vec::new(), false, directed, force, artifacts, false) {
                eprintln!("catch-up rebuild failed: {e}");
            }
            continue;
        }
        let paths = batch.take();
        if paths.is_empty() {
            continue; // burst was all ignored/non-code files
        }
        println!(
            "\nDetected {} changed code file(s) → rebuilding…",
            paths.len()
        );
        if let Err(e) = run_update(paths, false, directed, force, artifacts, false) {
            eprintln!("rebuild failed: {e}");
        }
    }
    Ok(())
}
