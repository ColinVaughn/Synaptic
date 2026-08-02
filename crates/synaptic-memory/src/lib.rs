//! Source-grounded, temporal repository memory.
//!
//! Memory is persisted independently from the rebuildable structural graph, then
//! joined to current graph symbols through [`SymbolAnchor`] values at query time.
#![forbid(unsafe_code)]

pub mod access;
mod api_maintenance;
pub mod artifact;
pub mod benchmark;
pub mod document;
pub mod git;
pub mod model;
pub mod query;
pub mod semantic;
pub mod store;
pub mod sync;

pub use access::MemoryPrincipal;
pub use api_maintenance::{
    record_api_maintenance_memory, ApiMaintenanceMemory, ApiMaintenanceMemoryError,
};
pub use artifact::{ingest_artifact_file, ArtifactIngestError, ArtifactIngestReport};
pub use benchmark::{
    enforce_benchmark_gate, run_benchmark_file, BenchmarkCaseResult, BenchmarkError, BenchmarkGate,
    BenchmarkGateFailure, BenchmarkReport,
};
pub use document::{ingest_repository_documents, DocumentIngestError, DocumentIngestReport};
pub use git::{ingest_commit, GitIngestError};
pub use model::{
    AccessScope, MemoryKind, MemoryLifecycle, MemoryLink, MemoryRecord, MemoryRelation, PathChange,
    PathChangeKind, SourceArtifact, SymbolAnchor, SymbolChange, SymbolChangeKind,
    VerificationOutcome, VerificationStatus,
};
pub use query::{MemoryQuery, MemorySearchDiagnostics, MemorySearchHit, MemorySearchResult};
pub use semantic::{
    generate_semantic_summaries, refresh_repository_memory, RepositoryRefreshError,
    RepositoryRefreshReport, SemanticRefreshReport,
};
pub use store::{CompactionReport, MemoryError, MemoryStore, RecordOutcome};
pub use sync::{export_bundle, import_bundle, BundleError, ExportBundleReport, ImportBundleReport};
