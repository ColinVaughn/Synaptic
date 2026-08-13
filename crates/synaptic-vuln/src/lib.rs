//! Dependency vulnerability detection, applicability analysis, and remediation
//! planning.
//!
//! The crate is deliberately offline by default: nothing here reads the network.
//! Advisories are supplied by an advisory source, whose default implementation
//! reads a local directory of OSV documents.
#![forbid(unsafe_code)]

mod advisory;
mod applicability;
mod check;
mod cvss4;
mod features;
mod finding;
mod handoff;
mod ledger;
mod lockfiles;
mod lockgraph;
mod matching;
mod osv_api;
mod plan;
mod policy;
mod reach;
mod repair;
mod scan;
mod severity;
mod source;
mod sync;

pub use advisory::{
    Advisory, AdvisoryError, Affected, RangeEvent, RangeKind, Severity, SeverityKind, VersionRange,
};
pub use applicability::{
    ApplicabilityEvidence, ApplicabilityInput, ApplicabilityVerdict, EvidenceDirection,
    EvidenceKind, assess_applicability,
};
pub use check::{DependencySafety, SafetyVerdict, check_dependency};
pub use cvss4::cvss_v4_base_score;
pub use features::{
    ManifestFeatures, feature_gated_dependencies, feature_gated_in, manifest_features,
};
pub use finding::{Finding, finding_id};
pub use handoff::{VerifiedVulnerabilityRunHandoff, VulnerabilityHandoffError};
pub use ledger::{
    Decision, DecisionKind, FindingRecord, FindingState, FindingStore, LedgerError, decision,
};
pub use lockfiles::{LockfileKind, parse as parse_lockfile};
pub use lockgraph::{
    LockGraphError, LockfileRead, PackageGraph, PackageKey, PackageScope, RepositoryFiles,
    ResolvedPackage, discover_repository_files,
};
pub use matching::{VersionMatch, match_range, match_version};
// Re-exported because this crate's public signatures are written in terms of
// it, so a caller cannot use them without naming the type.
pub use osv_api::{
    OSV_API_BASE, OSV_BATCH_LIMIT, OSV_TIMEOUT_SECONDS, OsvTransport, SystemOsvTransport,
    fetch_advisories, fetch_advisories_for_package, offline_forced, osv_ecosystem_name,
};
pub use plan::{
    CompatibilityRisk, RemediationKind, RemediationPlan, VersionAvailability, compatibility_risk,
    plan_remediation,
};
pub use policy::{DEFAULT_POLICY_PATH, DenyRule, ExceptionRule, PinRule, PolicyError, VulnPolicy};
pub use reach::{
    CallSite, EntryPoint, EntryPointKind, ImpactForecast, ImpactIndex, ReachIndex,
    RemediationScope, remediation_scope,
};
pub use repair::{RepairInputs, repair_inputs};
pub use scan::{
    EcosystemCoverage, GraphUsageOracle, NoUsageEvidence, ScanError, ScanReport, ScanRequest,
    SuppressedFinding, UsageOracle, advisories_for, is_sbom_source, scan,
};
pub use severity::{
    Priority, PriorityInputs, SeverityAssessment, SeverityBand, SeverityScoreSource,
    assess_severity, band_for_score, cvss_v3_base_score, prioritize,
};
pub use source::{AdvisorySource, CompositeSource, LocalDirSource, SourceDescription, SourceError};
pub use synaptic_api::{Ecosystem, PackageCoordinate};
pub use sync::{
    CorpusCache, CorpusFetcher, CorpusHead, CorpusMetadata, DEFAULT_MAX_DOWNLOAD_BYTES,
    DEFAULT_STALE_AFTER_SECONDS, OSV_BULK_BASE, SyncError, SystemCorpusFetcher, osv_bulk_url,
    sync_ecosystem, unpack_corpus,
};
