//! Synaptic workspace federation.
//!
//! Turns a set of *member* sources — local package paths (monorepo) or remote
//! repositories (multi-repo) — into one **federated** graph: each member is
//! extracted into its own subgraph, node ids are namespaced as `repo_tag::id`,
//! the subgraphs are composed, **cross-repo** edges are resolved against each
//! member's published **export surface**, and the merged graph is re-clustered
//! at the workspace level.
//!
//! Two lower-level building blocks support this: a persistent cross-repo
//! **global graph store** (id-prefixing + a `~/.synaptic` store; see [`global`])
//! and the `merge-graphs` command ([`merge_graphs`]). The workspace model, member
//! auto-discovery, and export-surface cross-repo resolution are built on top.
#![forbid(unsafe_code)]

pub mod alias;
pub mod coordinate;
pub mod discover;
pub mod export_surface;
pub mod federate;
pub mod global;
pub mod import_map;
pub mod manifest;
pub mod merge_graphs;
mod module_federation;
pub mod repo_scope;
pub mod scan;
pub mod state;
mod tsconfig;
pub mod workspace_build;

/// Refuse to load a graph/surface file larger than this (memory-bomb guard),
/// matching the merge driver's 50 MB cap.
pub const MAX_GRAPH_BYTES: u64 = 50 * 1024 * 1024;
/// Refuse graphs with more nodes than this (100k-node safety cap).
pub const MAX_NODES: usize = 100_000;
/// Current `export-surface.json` schema version. Bump on a breaking change.
pub const SURFACE_SCHEMA_VERSION: u32 = 1;

/// Errors the workspace layer can surface.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// An I/O error reading or writing a workspace file.
    #[error("{context}: {source}")]
    Io {
        context: String,
        source: std::io::Error,
    },
    /// A `synaptic-workspace.toml` (or member manifest) failed to parse.
    #[error("parsing {path}: {source}")]
    Toml {
        path: String,
        source: toml::de::Error,
    },
    /// A `graph.json` / `export-surface.json` failed to parse.
    #[error("parsing {path}: {source}")]
    Json {
        path: String,
        source: serde_json::Error,
    },
    /// A loaded file exceeded a safety cap.
    #[error("{path} exceeds the {limit}-byte cap ({size} bytes)")]
    TooBig { path: String, size: u64, limit: u64 },
    /// A loaded/merged graph exceeded the node cap.
    #[error("{path} has {count} nodes, over the {MAX_NODES} cap")]
    TooManyNodes { path: String, count: usize },
    /// A per-member rebuild failed.
    #[error("building member {member}: {source}")]
    Member {
        member: String,
        source: synaptic_incremental::IncrementalError,
    },
    /// A member path resolved outside the workspace root.
    #[error("member {member} resolves outside the workspace root")]
    OutsideRoot { member: String },
    /// A remote member (git clone / subgraph fetch) failed or was misconfigured.
    #[error("remote member {member}: {reason}")]
    Remote { member: String, reason: String },
    /// An export surface declares a schema version newer than this build supports.
    #[error("{path}: export-surface schema version {found} is newer than supported {supported}")]
    SurfaceVersion {
        path: String,
        found: u32,
        supported: u32,
    },
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, WorkspaceError>;

use std::path::Path;

use synaptic_core::GraphData;

/// Load a `graph.json` with the byte + node safety caps applied. Shared by the
/// merge-graphs, global-store, and artifact-federation paths.
pub fn load_graph(path: &Path) -> Result<GraphData> {
    let label = path.display().to_string();
    let meta = std::fs::metadata(path).map_err(|source| WorkspaceError::Io {
        context: format!("reading {label}"),
        source,
    })?;
    if meta.len() > MAX_GRAPH_BYTES {
        return Err(WorkspaceError::TooBig {
            path: label,
            size: meta.len(),
            limit: MAX_GRAPH_BYTES,
        });
    }
    let bytes = std::fs::read(path).map_err(|source| WorkspaceError::Io {
        context: format!("reading {label}"),
        source,
    })?;
    let g: GraphData = serde_json::from_slice(&bytes).map_err(|source| WorkspaceError::Json {
        path: label.clone(),
        source,
    })?;
    if g.nodes.len() > MAX_NODES {
        return Err(WorkspaceError::TooManyNodes {
            path: label,
            count: g.nodes.len(),
        });
    }
    Ok(g)
}

/// Write a `graph.json` (pretty, matching the rest of the toolchain), creating
/// the parent directory if needed.
pub fn write_graph(path: &Path, g: &GraphData) -> Result<()> {
    if g.nodes.len() > MAX_NODES {
        return Err(WorkspaceError::TooManyNodes {
            path: path.display().to_string(),
            count: g.nodes.len(),
        });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| WorkspaceError::Io {
            context: format!("creating {}", parent.display()),
            source,
        })?;
    }
    let bytes = serde_json::to_vec_pretty(g).map_err(|source| WorkspaceError::Json {
        path: path.display().to_string(),
        source,
    })?;
    std::fs::write(path, bytes).map_err(|source| WorkspaceError::Io {
        context: format!("writing {}", path.display()),
        source,
    })
}

/// Sanitize a string into a federation **repo tag**. Node ids are namespaced as
/// `tag::id`, so a tag must not contain `::` or path separators (which would make
/// the split ambiguous or leak into `source_file` prefixes). Runs of unsafe
/// characters collapse to a single `-`; the result is trimmed of leading/trailing
/// `-` and never empty (falls back to `repo`).
pub fn sanitize_tag(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        // Keep alphanumerics, `_`, `.`, `@`, `+`; everything else (including `:`,
        // `/`, `\`, whitespace) becomes a separator run.
        if c.is_alphanumeric() || matches!(c, '_' | '.' | '@' | '+' | '-') {
            out.push(c);
            prev_dash = c == '-';
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "repo".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tag_tests {
    use super::sanitize_tag;

    #[test]
    fn strips_separators_that_would_break_namespacing() {
        assert_eq!(sanitize_tag("acme/billing"), "acme-billing");
        assert_eq!(sanitize_tag("a::b"), "a-b");
        assert_eq!(sanitize_tag("path\\to\\repo"), "path-to-repo");
        assert_eq!(sanitize_tag("  spaced  "), "spaced");
        assert_eq!(sanitize_tag("@scope/pkg"), "@scope-pkg");
        assert_eq!(sanitize_tag("///"), "repo");
        assert_eq!(sanitize_tag("billing"), "billing");
    }
}
