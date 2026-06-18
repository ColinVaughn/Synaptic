//! `serve` command(s) split from main.rs.

use crate::commands::common::default_graph_path;
use anyhow::{Context, Result};
use codegraph_server::{serve_http, Server};
use std::path::{Path, PathBuf};

pub(crate) fn run_serve(
    graph: Option<PathBuf>,
    http: Option<String>,
    api_key: Option<String>,
    source_root: Option<PathBuf>,
) -> Result<()> {
    let path = default_graph_path(graph);
    let mut server = Server::load(path.clone()).with_context(|| {
        format!(
            "loading {} (run `codegraph extract` first?)",
            path.display()
        )
    })?;
    let root = source_root.unwrap_or_else(|| default_source_root(&path));
    server = server.with_source_root(root);
    match http {
        Some(addr_str) => {
            let addr: std::net::SocketAddr = addr_str
                .parse()
                .context("parsing --http address (host:port)")?;
            let api_key = api_key.or_else(|| std::env::var("CODEGRAPH_API_KEY").ok());
            if api_key.is_none() && addr.ip().is_unspecified() {
                eprintln!("[codegraph] WARNING: serving on a wildcard address with no API key");
            }
            eprintln!("[codegraph] MCP server on http://{addr}/mcp");
            let rt = tokio::runtime::Runtime::new().context("starting async runtime")?;
            rt.block_on(serve_http(server, addr, api_key))
                .context("serving over HTTP")?;
        }
        None => {
            // Status to stderr so it never pollutes the JSON-RPC stream on stdout.
            eprintln!("[codegraph] MCP server ready on stdio");
            server.serve_stdio().context("serving over stdio")?;
        }
    }
    Ok(())
}

/// Default source root from the graph path: the repo root is the directory
/// above codegraph-out/. `Path::parent` yields `Some("")` (not `None`) for a
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
            default_source_root(Path::new("codegraph-out/graph.json")),
            PathBuf::from(".")
        );
        // A bare filename -> current dir.
        assert_eq!(
            default_source_root(Path::new("graph.json")),
            PathBuf::from(".")
        );
        // A nested absolute path -> two levels up (the repo root).
        assert_eq!(
            default_source_root(Path::new("/proj/codegraph-out/graph.json")),
            PathBuf::from("/proj")
        );
    }
}
