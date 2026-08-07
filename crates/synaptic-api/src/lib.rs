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
    extract_sdk_surface_from_graph, AdapterError, ChangelogAdapter, ConfiguredVendorAdapter,
    OpenApiAdapter, PackageReleaseAdapter, SdkSurface, SdkSurfaceLoss, StaticAdapter,
    StripeAdapter, VendorAdapter,
};
pub use artifact::{
    ArtifactFetchRequest, ArtifactFetcher, FetchArtifactError, FetchedArtifact,
    SystemArtifactFetcher,
};

pub use behavior::{
    import_behavioral_evidence, BehavioralEvidenceError, BehavioralEvidenceReport,
    BehavioralObservation, BehavioralOutcome, BehavioralRegressionCandidate,
};
pub use binding::{
    bind_direct_http_usages, bind_repository_api_usages,
    bind_repository_api_usages_with_dependencies, bind_sdk_dependencies, bind_sdk_usages,
    ApiBindingReport, API_OPERATION_NODE_TYPE, API_VENDOR_NODE_TYPE,
};
pub use brief::{
    build_repair_brief, impact_from_nodes, ApiImpactForecast, ApiImpactHit, BriefBudget,
    BriefError, MemoryEvidence, RepairBrief, RepairBriefRequest, SourceSlice,
    VerificationRequirement,
};
pub use catalog::{
    CachedPackageMetadataResolver, PackageMetadata, PackageMetadataError, PackageMetadataResolver,
};
pub use config::{
    load_optional_registry, maintenance_policy_digest, ApiMaintenanceConfig, CommandPolicy,
    ConfigError, ConfigLoadError, CoveragePolicy, CoverageWaiver, MaintenanceMode, PublishPolicy,
    SdkBindingRule, VendorConfig, VendorMatch, VendorRegistry, VendorSource, DEFAULT_CONFIG_PATH,
};
pub use contract::{
    diff_contracts, normalize_contract, normalize_openapi, ApiContract, AutoSurfaceReader,
    CompatibilityPolicy, ContractError, ContractOperation, DefaultCompatibilityPolicy, FieldShape,
    ParseCompleteness, SurfaceFormat, SurfaceLoss, SurfaceReader,
};
pub use coverage::{
    analyze_api_coverage, analyze_api_coverage_with_evidence, analyze_api_coverage_with_runtime,
    attach_api_coverage, attach_api_coverage_with_evidence, ApiCoverageReport, CoverageGap,
    CoverageGapKind, CoverageState, EvidenceWindow, ExternalSurfaceKind,
    ExternalSurfaceObservation, EXTERNAL_SURFACE_NODE_TYPE, OBSERVES_EXTERNAL_RELATION,
};
pub use discovery::{
    candidate_profile_toml, discover_contracts, ContractDiscoveryReport, DiscoveredContract,
    DiscoveryError, RejectedContractCandidate,
};
pub use evaluation::{HistoricalCaseObservation, HistoricalEvaluationReport};
pub use event::{
    ApiBreakingChange, ApiChangeEvent, BreakingChangeKind, EvidenceSpan, SdkSymbolAnchor,
    SourceArtifact, VersionRange,
};
pub use handoff::{HandoffError, VerifiedRunHandoff};
pub use invariants::{verify_api_invariants, ApiInvariantReport, InvariantCheck};
pub use inventory::{
    inventory, is_sbom_manifest, scan_dependencies, scan_dependencies_and_sbom_evidence,
    scan_sbom_evidence, AmbiguousVendorDependency, ApiInventory, ExternalServiceEvidence,
    InventoryError, SbomCompleteness, SbomDocumentEvidence, SbomEvidenceReport, VendorDependency,
};
pub use ledger::{ApiRunRecord, ApiRunStore, LedgerError, RunState};
pub use model::{
    ApiOperationAnchor, Dependency, DependencyScope, Ecosystem, PackageCoordinate, PackageUrl,
};
pub use patch_policy::{validate_patch, PatchInspection, PatchPolicy, PatchPolicyError};
pub use publisher::{
    deterministic_branch, publish_verified_change_request, publish_verified_draft,
    ChangeRequestKind, ChangeRequestProvider, CommandOutput, DraftPublishRequest, PublishAction,
    PublishCommandRunner, PublishContext, PublishError, PublishResult, SystemPublishCommandRunner,
};
pub use relevance::{
    evaluate_relevance, usage_bindings, ApiUsageBinding, ApplicabilityReason, ApplicabilityState,
    BindingBasis, RelevanceAssessment,
};
pub use repair::{
    failed_attempt_summary, run_repair_attempts, GateOutcome, GateResult, GeneratedPatch,
    PatchGenerationError, PatchGenerator, PatchVerifier, RepairAttempt, RepairError, RepairFailure,
    RepairOutcome, VerificationReport,
};
pub use runtime::{
    import_runtime_evidence, RuntimeEvidenceError, RuntimeEvidenceReport, RuntimeSurfaceEvidence,
    RuntimeSurfaceKind,
};
pub use scan::{
    sanitize_release_text, scan_repository, ReviewCandidate, ScanDisposition, ScanError,
    ScanReport, ScannedSource, WebhookArtifactEnvelope,
};
pub use store::{ApiEventStore, SourceLockState, StoreError};
pub use worker::{
    build_coordination_plan, credential_scope_for_stage, execute_worker_attempt, BoundedJobQueue,
    CancellationToken, CoordinatedRepositoryRepair, CoordinationPlan, CredentialScope,
    HostedApiJob, JobStage, QueueError, RepositoryImpact, RetryPolicy, WorkerAttemptOutcome,
    WorkerEvent, WorkerEventSink, WorkerEventState, WorkerJobRunner,
};
