//! `serve` command(s) split from main.rs.

use crate::commands::common::default_graph_path;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use synaptic_server::{serve_http, Server};

pub(crate) fn run_serve(
    graph: Option<PathBuf>,
    http: Option<String>,
    api_key: Option<String>,
    source_root: Option<PathBuf>,
    allow_exec: bool,
    concise: bool,
    watch: bool,
) -> Result<()> {
    let path = default_graph_path(graph);
    let mut server = Server::load(path.clone())
        .with_context(|| format!("loading {} (run `synaptic extract` first?)", path.display()))?;
    let root = source_root.unwrap_or_else(|| default_source_root(&path));
    server = server
        .with_source_root(root.clone())
        .with_allow_exec(allow_exec)
        .with_concise(concise);
    // Event-driven staleness (`--watch` / SYNAPTIC_SERVE_WATCH): a background
    // watcher flips a dirty flag on relevant changes, so queries skip the
    // walk-per-query check and the debounce window. The flag starts dirty so
    // the first query still catches up on edits made before the watcher ran.
    // Best-effort: if the watcher cannot start, serve falls back to the
    // debounced walk. `_watcher` must outlive the serve loop.
    let watch = watch || synaptic_server::env_flag("SYNAPTIC_SERVE_WATCH", false);
    let _watcher = if watch {
        match spawn_watch_flag(&root) {
            Ok((flag, watcher)) => {
                server.set_watch_dirty(flag);
                eprintln!(
                    "[synaptic] event-driven staleness: watching {}",
                    root.display()
                );
                Some(watcher)
            }
            Err(e) => {
                eprintln!(
                    "[synaptic] could not start the filesystem watcher ({e}); using the debounced walk"
                );
                None
            }
        }
    } else {
        None
    };
    // When serving a federated/global graph, register each member repo's source
    // root so `get_source` can read nodes whose `source_file` points at a sibling
    // repo outside the single source root.
    let repo_roots = federated_repo_roots(&path);
    if !repo_roots.is_empty() {
        server = server.with_repo_roots(repo_roots);
    }
    if allow_exec {
        eprintln!(
            "[synaptic] WARNING: --allow-exec enabled; the `speculate` tool can run this project's test/build commands"
        );
    }
    match http {
        Some(addr_str) => {
            let addr: std::net::SocketAddr = addr_str
                .parse()
                .context("parsing --http address (host:port)")?;
            let api_key = api_key.or_else(|| std::env::var("SYNAPTIC_API_KEY").ok());
            if api_key.is_none() && addr.ip().is_unspecified() {
                eprintln!("[synaptic] WARNING: serving on a wildcard address with no API key");
            }
            eprintln!("[synaptic] MCP server on http://{addr}/mcp");
            let rt = tokio::runtime::Runtime::new().context("starting async runtime")?;
            rt.block_on(serve_http(server, addr, api_key))
                .context("serving over HTTP")?;
        }
        None => {
            // Status to stderr so it never pollutes the JSON-RPC stream on stdout.
            eprintln!("[synaptic] MCP server ready on stdio");
            server.serve_stdio().context("serving over stdio")?;
        }
    }
    Ok(())
}

/// Start a recursive watcher on `root` that sets the returned flag whenever a
/// graph-input file (code / extractable markdown) outside the ignored subtrees
/// changes. The flag starts set (pre-watch edits still catch up on the first
/// query). The watcher must be kept alive by the caller.
fn spawn_watch_flag(
    root: &Path,
) -> notify::Result<(
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    notify::RecommendedWatcher,
)> {
    use notify::{RecursiveMode, Watcher};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let flag = Arc::new(AtomicBool::new(true));
    let f = flag.clone();
    let raw_root = root.to_path_buf();
    let canon_root = raw_root.canonicalize().unwrap_or_else(|_| raw_root.clone());
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(ev) => {
                // A rescan notice means events were dropped (buffer overflow
                // on a huge change like a branch switch): assume dirty.
                if ev.need_rescan() {
                    f.store(true, Ordering::Release);
                    return;
                }
                if ev
                    .paths
                    .iter()
                    .any(|p| relevant_change(p, &canon_root, &raw_root))
                {
                    f.store(true, Ordering::Release);
                }
            }
            // A watcher error may mean lost events; a false dirty flag only
            // costs one staleness walk, a missed one serves stale forever.
            Err(_) => f.store(true, Ordering::Release),
        }
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok((flag, watcher))
}

/// True when an event path is a graph-input change. Filters on the
/// repo-RELATIVE path so a noise dir name in an ancestor of the root (a
/// checkout under `/build/app`) cannot silence the whole tree; the root is
/// stripped in canonical and raw forms because notify's event paths and the
/// configured root may disagree on canonicalization (relative roots, `\\?\`
/// prefixes, macOS symlinked temp dirs). Only when no form strips does the
/// absolute-path filter apply as a last resort (self-trigger safety beats the
/// remote ancestor-name hazard).
fn relevant_change(p: &Path, canon_root: &Path, raw_root: &Path) -> bool {
    use synaptic_incremental::{is_rebuildable, should_ignore_path};
    let rel = p
        .strip_prefix(canon_root)
        .or_else(|_| p.strip_prefix(raw_root))
        .map(Path::to_path_buf)
        .ok()
        .or_else(|| {
            p.canonicalize()
                .ok()
                .and_then(|cp| cp.strip_prefix(canon_root).map(Path::to_path_buf).ok())
        });
    match rel {
        Some(r) => !should_ignore_path(&r) && is_rebuildable(&r),
        None => !should_ignore_path(p) && is_rebuildable(p),
    }
}

/// Build the `tag -> repo source root` map for a federated/global graph. The
/// signal is a `global-manifest.json` next to the graph; each member's
/// `source_path` points at its own `graph.json`, whose grandparent is that
/// repo's source root (matching [`default_source_root`]). Returns an empty map
/// for an ordinary single-repo graph, leaving the single source root in charge.
fn federated_repo_roots(graph_path: &Path) -> HashMap<String, PathBuf> {
    let mut roots = HashMap::new();
    let Some(dir) = graph_path.parent() else {
        return roots;
    };
    if !dir.join("global-manifest.json").exists() {
        return roots;
    }
    let store = synaptic_workspace::global::GlobalStore::at(dir.to_path_buf());
    for (tag, entry) in store.list() {
        let src = Path::new(&entry.source_path);
        if let Some(repo_root) = src.parent().and_then(Path::parent) {
            if !repo_root.as_os_str().is_empty() {
                roots.insert(tag, repo_root.to_path_buf());
            }
        }
    }
    roots
}

/// Default source root from the graph path: the repo root is the directory
/// above synaptic-out/. `Path::parent` yields `Some("")` (not `None`) for a
/// relative default path run from the repo root, so an empty result falls back
/// to the current directory rather than an unresolvable empty path.
fn default_source_root(graph_path: &Path) -> PathBuf {
    match graph_path.parent().and_then(Path::parent) {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_source_root_handles_relative_and_absolute() {
        // Relative default path run from the repo root -> current dir.
        assert_eq!(
            default_source_root(Path::new("synaptic-out/graph.json")),
            PathBuf::from(".")
        );
        // A bare filename -> current dir.
        assert_eq!(
            default_source_root(Path::new("graph.json")),
            PathBuf::from(".")
        );
        // A nested absolute path -> two levels up (the repo root).
        assert_eq!(
            default_source_root(Path::new("/proj/synaptic-out/graph.json")),
            PathBuf::from("/proj")
        );
    }
}
