//! Safe refactor: plan a single-symbol rename (confidence-scored, agent-executable)
//! and verify the graph after the agent applies it. Synaptic never edits source.
//!
//! The pipeline reuses existing primitives: the loaded `KnowledgeGraph` for the
//! symbol set, `synaptic_query::affected_nodes` for the blast radius, the AST
//! cache (`synaptic_extract::cached_extract_source`) for column-accurate per-site
//! spans, `synaptic_incremental::rebuild` to re-extract for verify, and
//! `synaptic_graph::find_import_cycles` for the no-new-cycle invariant.

pub mod emit;
pub mod plan;
pub mod relocate;
pub mod resolve;
pub mod sites;
pub mod verify;

pub use plan::{plan_rename, BlastRadius, Collision, RenameOptions, RenamePlan};
pub use relocate::{plan_relocate, RelocatePlan};
pub use resolve::Candidate;
pub use sites::EditSite;
pub use verify::{verify_plan, verify_relocate, VerifyCheck, VerifyReport};

use synaptic_core::Confidence;

/// The canonical wire string for a confidence level (matches its serde form).
pub(crate) fn confidence_str(c: Confidence) -> &'static str {
    match c {
        Confidence::Extracted => "EXTRACTED",
        Confidence::Inferred => "INFERRED",
        Confidence::Ambiguous => "AMBIGUOUS",
    }
}

/// Errors the refactor pipeline can surface.
#[derive(Debug, thiserror::Error)]
pub enum RefactorError {
    #[error("symbol not found: {0}")]
    NotFound(String),
    #[error("ambiguous symbol {name}: {count} candidates; disambiguate with --id or --file")]
    Ambiguous { name: String, count: usize },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("rebuild error: {0}")]
    Rebuild(String),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}
