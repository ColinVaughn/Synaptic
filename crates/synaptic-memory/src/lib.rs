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
    ApiMaintenanceMemory, ApiMaintenanceMemoryError, record_api_maintenance_memory,
};
pub use artifact::{ArtifactIngestError, ArtifactIngestReport, ingest_artifact_file};
pub use benchmark::{
    BenchmarkCaseResult, BenchmarkError, BenchmarkGate, BenchmarkGateFailure, BenchmarkReport,
    enforce_benchmark_gate, run_benchmark_file,
};
pub use document::{DocumentIngestError, DocumentIngestReport, ingest_repository_documents};
pub use git::{GitIngestError, ingest_commit};
pub use model::{
    AccessScope, MemoryKind, MemoryLifecycle, MemoryLink, MemoryRecord, MemoryRelation, PathChange,
    PathChangeKind, SourceArtifact, SymbolAnchor, SymbolChange, SymbolChangeKind,
    VerificationOutcome, VerificationStatus,
};
pub use query::{MemoryQuery, MemorySearchDiagnostics, MemorySearchHit, MemorySearchResult};
pub use semantic::{
    RepositoryRefreshError, RepositoryRefreshReport, SemanticRefreshReport,
    generate_semantic_summaries, refresh_repository_memory,
};
pub use store::{CompactionReport, MemoryError, MemoryStore, RecordOutcome};
pub use sync::{BundleError, ExportBundleReport, ImportBundleReport, export_bundle, import_bundle};
