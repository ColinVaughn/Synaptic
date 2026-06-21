//! Extraction facts consumed by cross-file symbol resolution. They live in
//! `core` (not `synaptic-extract`) so `synaptic-graph::symbol_resolution` can
//! consume them without `graph` depending on `extract` (the dep DAG forbids it).

use serde::{Deserialize, Serialize};

use crate::span::Span;
use crate::NodeId;

/// An unresolved call captured during extraction, resolved across files by the
/// symbol-resolution pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawCall {
    pub caller: NodeId,
    pub callee: String,
    pub is_member_call: bool,
    pub source_file: String,
    pub source_location: Option<String>,
    /// Precise call-site range (column-accurate), when the extractor captured it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub span: Option<Span>,
}

/// A top-level `from M import name [as local]` captured during extraction, used
/// as deterministic evidence by symbol resolution. Only emitted when the module
/// has a non-empty final component (`module_stem`); wildcards are skipped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportRecord {
    /// The name bound in the importing file (the alias if `as`, else the symbol).
    pub local_name: String,
    /// The original symbol name in the source module.
    pub imported_name: String,
    /// Final component of the module path (`helper` from `pkg.helper` or `.helper`).
    pub module_stem: String,
    pub source_file: String,
    pub source_location: Option<String>,
}
