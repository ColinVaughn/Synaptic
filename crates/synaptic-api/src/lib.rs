//! Vendor-neutral API dependency inventory and maintenance contracts.
#![forbid(unsafe_code)]

mod adapter;
mod artifact;
mod behavior;
mod binding;
mod brief;
mod catalog;
mod config;
mod contract;
mod coverage;
mod discovery;
mod evaluation;
mod event;
mod handoff;
mod invariants;
mod inventory;
mod ledger;
mod model;
mod patch_policy;
mod publisher;
mod redaction;
mod relevance;
mod repair;
mod runtime;
mod scan;
mod store;
mod worker;

pub use adapter::{
    AdapterError, ChangelogAdapter, ConfiguredVendorAdapter, OpenApiAdapter, PackageReleaseAdapter,
    SdkSurface, SdkSurfaceLoss, StaticAdapter, StripeAdapter, VendorAdapter,
    extract_sdk_surface_from_graph,
};
pub use artifact::{
    ArtifactFetchRequest, ArtifactFetcher, FetchArtifactError, FetchedArtifact,
    SystemArtifactFetcher,
};

pub use behavior::{
    BehavioralEvidenceError, BehavioralEvidenceReport, BehavioralObservation, BehavioralOutcome,
    BehavioralRegressionCandidate, import_behavioral_evidence,
};
pub use binding::{
    API_OPERATION_NODE_TYPE, API_VENDOR_NODE_TYPE, ApiBindingReport, bind_direct_http_usages,
    bind_repository_api_usages, bind_repository_api_usages_with_dependencies,
    bind_sdk_dependencies, bind_sdk_usages,
};
pub use brief::{
    ApiImpactForecast, ApiImpactHit, BriefBudget, BriefError, MemoryEvidence, RepairBrief,
    RepairBriefRequest, SourceSlice, VerificationRequirement, build_repair_brief,
    impact_from_nodes,
};
pub use catalog::{
    CachedPackageMetadataResolver, PackageMetadata, PackageMetadataError, PackageMetadataResolver,
};
pub use config::{
    ApiMaintenanceConfig, CommandPolicy, ConfigError, ConfigLoadError, CoveragePolicy,
    CoverageWaiver, DEFAULT_CONFIG_PATH, MaintenanceMode, PublishPolicy, SdkBindingRule,
    VendorConfig, VendorMatch, VendorRegistry, VendorSource, load_optional_registry,
    maintenance_policy_digest,
};
pub use contract::{
    ApiContract, AutoSurfaceReader, CompatibilityPolicy, ContractError, ContractOperation,
    DefaultCompatibilityPolicy, FieldShape, ParseCompleteness, SurfaceFormat, SurfaceLoss,
    SurfaceReader, diff_contracts, normalize_contract, normalize_openapi,
};
pub use coverage::{
    ApiCoverageReport, CoverageGap, CoverageGapKind, CoverageState, EXTERNAL_SURFACE_NODE_TYPE,
    EvidenceWindow, ExternalSurfaceKind, ExternalSurfaceObservation, OBSERVES_EXTERNAL_RELATION,
    analyze_api_coverage, analyze_api_coverage_with_evidence, analyze_api_coverage_with_runtime,
    attach_api_coverage, attach_api_coverage_with_evidence,
};
pub use discovery::{
    ContractDiscoveryReport, DiscoveredContract, DiscoveryError, RejectedContractCandidate,
    candidate_profile_toml, discover_contracts,
};
pub use evaluation::{HistoricalCaseObservation, HistoricalEvaluationReport};
pub use event::{
    ApiBreakingChange, ApiChangeEvent, BreakingChangeKind, EvidenceSpan, SdkSymbolAnchor,
    SourceArtifact, VersionRange,
};
pub use handoff::{HandoffError, VerifiedRunHandoff};
pub use invariants::{ApiInvariantReport, InvariantCheck, verify_api_invariants};
pub use inventory::{
    AmbiguousVendorDependency, ApiInventory, ExternalServiceEvidence, InventoryError,
    SbomCompleteness, SbomDocumentEvidence, SbomEvidenceReport, VendorDependency, inventory,
    is_sbom_manifest, scan_dependencies, scan_dependencies_and_sbom_evidence,
    scan_graph_dependency_evidence, scan_sbom_evidence,
};
pub use ledger::{ApiRunRecord, ApiRunStore, LedgerError, RunState};
pub use model::{
    ApiOperationAnchor, Dependency, DependencyScope, Ecosystem, PackageCoordinate, PackageUrl,
};
pub use patch_policy::{PatchInspection, PatchPolicy, PatchPolicyError, validate_patch};
pub use publisher::{
    ChangeRequestKind, ChangeRequestProvider, CommandOutput, DraftPublishRequest, PublishAction,
    PublishCommandRunner, PublishContext, PublishError, PublishResult, SystemPublishCommandRunner,
    deterministic_branch, deterministic_vulnerability_branch, publish_verified_change_request,
    publish_verified_draft, publish_verified_vulnerability_change_request,
};
pub use relevance::{
    ApiUsageBinding, ApplicabilityReason, ApplicabilityState, BindingBasis, RelevanceAssessment,
    evaluate_relevance, usage_bindings,
};
pub use repair::{
    GateOutcome, GateResult, GeneratedPatch, PatchGenerationError, PatchGenerator, PatchVerifier,
    RepairAttempt, RepairError, RepairFailure, RepairOutcome, VerificationReport,
    failed_attempt_summary, run_repair_attempts,
};
pub use runtime::{
    RuntimeEvidenceError, RuntimeEvidenceReport, RuntimeSurfaceEvidence, RuntimeSurfaceKind,
    import_runtime_evidence,
};
pub use scan::{
    ReviewCandidate, ScanDisposition, ScanError, ScanReport, ScannedSource,
    WebhookArtifactEnvelope, sanitize_release_text, scan_repository,
};
pub use store::{ApiEventStore, SourceLockState, StoreError};
pub use worker::{
    BoundedJobQueue, CancellationToken, CoordinatedRepositoryRepair, CoordinationPlan,
    CredentialScope, HostedApiJob, JobStage, QueueError, RepositoryImpact, RetryPolicy,
    WorkerAttemptOutcome, WorkerEvent, WorkerEventSink, WorkerEventState, WorkerJobRunner,
    build_coordination_plan, credential_scope_for_stage, execute_worker_attempt,
};
