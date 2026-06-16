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
    // Default: graph is at <root>/codegraph-out/graph.json, so the repo root is
    // two levels up. Fall back to the current dir when that is unavailable.
    let root = source_root.unwrap_or_else(|| {
        path.parent()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    });
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
