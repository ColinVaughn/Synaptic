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
mod finding;
mod ledger;
mod lockfiles;
mod lockgraph;
mod matching;
mod plan;
mod policy;
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
pub use finding::{finding_id, Finding};
pub use ledger::{
    decision, Decision, DecisionKind, FindingRecord, FindingState, FindingStore, LedgerError,
};
pub use lockfiles::{parse as parse_lockfile, LockfileKind};
pub use lockgraph::{LockGraphError, LockfileRead, PackageGraph, PackageKey, ResolvedPackage};
pub use matching::{match_range, match_version, VersionMatch};
pub use plan::{
    compatibility_risk, plan_remediation, CompatibilityRisk, RemediationKind, RemediationPlan,
    VersionAvailability,
};
pub use policy::{DenyRule, ExceptionRule, PinRule, PolicyError, VulnPolicy, DEFAULT_POLICY_PATH};
pub use scan::{
    advisories_for, scan, GraphUsageOracle, NoUsageEvidence, ScanError, ScanReport, ScanRequest,
    SuppressedFinding, UsageOracle,
};
pub use severity::{
    assess_severity, band_for_score, cvss_v3_base_score, prioritize, Priority, PriorityInputs,
    SeverityAssessment, SeverityBand, SeverityScoreSource,
};
pub use source::{AdvisorySource, CompositeSource, LocalDirSource, SourceDescription, SourceError};
pub use sync::{
    osv_bulk_url, sync_ecosystem, unpack_corpus, CorpusCache, CorpusFetcher, CorpusHead,
    CorpusMetadata, SyncError, SystemCorpusFetcher, DEFAULT_MAX_DOWNLOAD_BYTES,
    DEFAULT_STALE_AFTER_SECONDS, OSV_BULK_BASE,
};
