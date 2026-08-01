use serde::{Deserialize, Serialize};

use crate::{MemoryKind, MemoryRecord};

#[derive(Debug, Clone, Default)]
pub struct MemoryQuery {
    pub text: String,
    pub kinds: Vec<MemoryKind>,
    pub symbol: Option<String>,
    pub include_superseded: bool,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySearchHit {
    pub record: MemoryRecord,
    pub score: f64,
    pub matched_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySearchDiagnostics {
    pub total_records: usize,
    pub candidate_records: usize,
    pub loaded_from_compaction: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySearchResult {
    pub hits: Vec<MemorySearchHit>,
    pub diagnostics: MemorySearchDiagnostics,
}
