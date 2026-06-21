//! `serve` command(s) split from main.rs.

use crate::commands::common::default_graph_path;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use synaptic_server::{serve_http, Server};

pub(crate) fn run_serve(
    graph: Option<PathBuf>,
    http: Option<String>,
    api_key: Option<String>,
    source_root: Option<PathBuf>,
    allow_exec: bool,
) -> Result<()> {
    let path = default_graph_path(graph);
    let mut server = Server::load(path.clone())
        .with_context(|| format!("loading {} (run `synaptic extract` first?)", path.display()))?;
    let root = source_root.unwrap_or_else(|| default_source_root(&path));
    server = server.with_source_root(root).with_allow_exec(allow_exec);
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
