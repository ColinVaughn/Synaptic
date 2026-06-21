//! `watch` command(s) split from main.rs.

use crate::commands::update::run_update;
use anyhow::{Context, Result};
use std::path::Path;
use synaptic_detect::FileType;
use synaptic_incremental::{should_ignore_path, ChangeBatch, DEBOUNCE_MS};

/// Watch the working tree and rebuild incrementally on change. Debounces a burst
/// of saves (`DEBOUNCE_MS`) into one rebuild, ignores the output/VCS/build
/// subtrees (so writing `graph.json` can't self-trigger), and routes each batch
/// of changed **code** files through [`run_update`] (which holds the rebuild lock
/// and writes artifacts).
pub(crate) fn run_watch(directed: bool, force: bool) -> Result<()> {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc::channel;
    use std::time::Duration;

    let root = std::env::current_dir().context("resolving current directory")?;

    let (tx, rx) = channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .context("creating filesystem watcher")?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .with_context(|| format!("watching {}", root.display()))?;

    println!(
        "Watching {} for changes (debounce {}ms; Ctrl-C to stop)…",
        root.display(),
        DEBOUNCE_MS
    );

    // Code-file changes warrant an AST rebuild; so do markdown edits (headings
    // get structural extraction in `rebuild`). Other edits are ignored.
    let is_rebuildable = |p: &Path| {
        synaptic_detect::classify_file(p) == Some(FileType::Code)
            || matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("md") | Some("mdx") | Some("qmd")
            )
    };

    // Block until the first change, then drain a quiet window to batch a burst.
    // Loop ends when the watcher is dropped (channel closed).
    while let Ok(first) = rx.recv() {
        let mut batch = ChangeBatch::new();
        let mut record = |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                for p in ev.paths {
                    if !should_ignore_path(&p) && is_rebuildable(&p) {
                        batch.record(p);
                    }
                }
            }
        };
        record(first);
        while let Ok(ev) = rx.recv_timeout(Duration::from_millis(DEBOUNCE_MS)) {
            record(ev);
        }

        let paths = batch.take();
        if paths.is_empty() {
            continue; // burst was all ignored/non-code files
        }
        println!(
            "\nDetected {} changed code file(s) → rebuilding…",
            paths.len()
        );
        if let Err(e) = run_update(paths, false, directed, force) {
            eprintln!("rebuild failed: {e}");
        }
    }
    Ok(())
}
