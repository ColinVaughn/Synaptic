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
    assess_applicability, ApplicabilityEvidence, ApplicabilityInput, ApplicabilityVerdict,
    EvidenceDirection, EvidenceKind,
};
pub use check::{check_dependency, DependencySafety, SafetyVerdict};
pub use cvss4::cvss_v4_base_score;
pub use features::{
    feature_gated_dependencies, feature_gated_in, manifest_features, ManifestFeatures,
};
pub use finding::{finding_id, Finding};
pub use handoff::{VerifiedVulnerabilityRunHandoff, VulnerabilityHandoffError};
pub use ledger::{
    decision, Decision, DecisionKind, FindingRecord, FindingState, FindingStore, LedgerError,
};
pub use lockfiles::{parse as parse_lockfile, LockfileKind};
pub use lockgraph::{
    discover_repository_files, LockGraphError, LockfileRead, PackageGraph, PackageKey,
    PackageScope, RepositoryFiles, ResolvedPackage,
};
pub use matching::{match_range, match_version, VersionMatch};
// Re-exported because this crate's public signatures are written in terms of
// it, so a caller cannot use them without naming the type.
pub use osv_api::{
    fetch_advisories, fetch_advisories_for_package, offline_forced, osv_ecosystem_name,
    OsvTransport, SystemOsvTransport, OSV_API_BASE, OSV_BATCH_LIMIT, OSV_TIMEOUT_SECONDS,
};
pub use plan::{
    compatibility_risk, plan_remediation, CompatibilityRisk, RemediationKind, RemediationPlan,
    VersionAvailability,
};
pub use policy::{DenyRule, ExceptionRule, PinRule, PolicyError, VulnPolicy, DEFAULT_POLICY_PATH};
pub use reach::{
    remediation_scope, CallSite, EntryPoint, EntryPointKind, ImpactForecast, ImpactIndex,
    ReachIndex, RemediationScope,
};
pub use repair::{repair_inputs, RepairInputs};
pub use scan::{
    advisories_for, is_sbom_source, scan, EcosystemCoverage, GraphUsageOracle, NoUsageEvidence,
    ScanError, ScanReport, ScanRequest, SuppressedFinding, UsageOracle,
};
pub use severity::{
    assess_severity, band_for_score, cvss_v3_base_score, prioritize, Priority, PriorityInputs,
    SeverityAssessment, SeverityBand, SeverityScoreSource,
};
pub use source::{AdvisorySource, CompositeSource, LocalDirSource, SourceDescription, SourceError};
pub use synaptic_api::{Ecosystem, PackageCoordinate};
pub use sync::{
    osv_bulk_url, sync_ecosystem, unpack_corpus, CorpusCache, CorpusFetcher, CorpusHead,
    CorpusMetadata, SyncError, SystemCorpusFetcher, DEFAULT_MAX_DOWNLOAD_BYTES,
    DEFAULT_STALE_AFTER_SECONDS, OSV_BULK_BASE,
};
