//! External-source ingestion for Synaptic: URL/office/media ingestion, MCP
//! config, Cargo and PostgreSQL introspection, SCIP import, and SSRF-guarded
//! fetching.
//!
//! Sources: `validate_url`/`safe_fetch` (SSRF-guarded fetch), MCP-config, Cargo
//! introspection, SCIP-index JSON, and Postgres schema introspection (behind
//! the `pg` feature) — all of which emit graph nodes/edges ("shape B") — plus
//! URL ingest (which writes a file for the normal extraction pass — "shape A").
//! Transcription, YouTube, google-workspace and image-vision remain deferred
//! (image-vision lives in `synaptic-llm`).
#![forbid(unsafe_code)]

pub mod cargo_manifest;
#[cfg(feature = "gws")]
pub mod gws;
pub mod mcp;
#[cfg(feature = "media")]
pub mod media;
#[cfg(feature = "office")]
pub mod office;
pub mod pg;
pub mod scip;
pub mod security;
pub mod url;

pub use cargo_manifest::introspect_cargo;
pub use mcp::ingest_mcp_config;
pub use pg::{build_postgres_graph, introspect_postgres, PgError, PgSchema, SchemaSource};
pub use scip::ingest_scip_json;
pub use security::{safe_fetch, safe_fetch_text, validate_url, FetchError};
pub use url::{detect_url_type, ingest_url, yaml_str, UrlKind};

#[cfg(feature = "gws")]
pub use gws::{gws_export_args, ingest_gdoc, is_gws_pointer, parse_gws_pointer};
#[cfg(feature = "media")]
pub use media::{
    ingest_youtube, parse_subtitle, transcribe_args, transcribe_media, yt_dlp_subtitle_args,
};
#[cfg(feature = "office")]
pub use office::xlsx_to_markdown;
#[cfg(feature = "pg")]
pub use pg::SystemPostgres;

use std::path::Path;

use serde_json::{json, Map};
use synaptic_core::{sanitize_label, Confidence, Edge, FileType, Node, NodeId};

/// Nodes + edges produced by a "shape B" source (mcp-config, cargo).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Ingested {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// `{parent_dir}.{stem}` — mirrors `extract._file_stem` (kept local so ingest
/// needn't depend on `synaptic-extract`).
pub(crate) fn file_stem(path: &Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty() && s != ".");
    match (parent, stem) {
        (Some(p), Some(s)) => format!("{p}.{s}"),
        (None, Some(s)) => s,
        _ => String::new(),
    }
}

/// Build a node tagged with an optional `mcp_kind` (carried under `metadata` to
/// match the `graph.json` node shape). `file_type` is always `code`.
pub(crate) fn make_node(
    id: String,
    label: &str,
    source_file: &str,
    line: usize,
    kind: Option<&str>,
) -> Node {
    let mut extra = Map::new();
    if let Some(k) = kind {
        extra.insert("metadata".into(), json!({ "mcp_kind": k }));
    }
    Node {
        id: NodeId(id),
        label: sanitize_label(label),
        file_type: FileType::Code,
        source_file: source_file.to_string(),
        source_location: Some(format!("L{line}")),
        community: None,
        repo: None,
        extra,
    }
}

/// Build an EXTRACTED edge (confidence 1.0), with optional `context`.
pub(crate) fn make_edge(
    source: String,
    target: String,
    relation: &str,
    source_file: &str,
    line: usize,
    context: Option<&str>,
) -> Edge {
    Edge {
        source: NodeId(source),
        target: NodeId(target),
        relation: relation.to_string(),
        confidence: Confidence::Extracted,
        confidence_score: Some(1.0),
        source_file: source_file.to_string(),
        source_location: Some(format!("L{line}")),
        weight: 1.0,
        context: context.map(str::to_string),
        cross_repo: false,
        extra: Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_stem_includes_parent() {
        assert_eq!(file_stem(Path::new("proj/.mcp.json")), "proj..mcp");
        assert_eq!(file_stem(Path::new(".mcp.json")), ".mcp");
    }
}
