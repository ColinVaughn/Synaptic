use serde::{Deserialize, Serialize};

/// The four product memory families plus the concrete source/event types that
/// make them useful to an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    ChangeEpisode,
    Issue,
    Incident,
    PullRequest,
    ReviewFinding,
    CiRun,
    ArchitectureDecision,
    Invariant,
    Convention,
    Procedure,
    FailedAttempt,
    Regression,
    Release,
    CustomerReport,
    AgentTask,
    SemanticSummary,
}

impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChangeEpisode => "change_episode",
            Self::Issue => "issue",
            Self::Incident => "incident",
            Self::PullRequest => "pull_request",
            Self::ReviewFinding => "review_finding",
            Self::CiRun => "ci_run",
            Self::ArchitectureDecision => "architecture_decision",
            Self::Invariant => "invariant",
            Self::Convention => "convention",
            Self::Procedure => "procedure",
            Self::FailedAttempt => "failed_attempt",
            Self::Regression => "regression",
            Self::Release => "release",
            Self::CustomerReport => "customer_report",
            Self::AgentTask => "agent_task",
            Self::SemanticSummary => "semantic_summary",
        }
    }

    pub fn is_pitfall(self) -> bool {
        matches!(
            self,
            Self::FailedAttempt | Self::Regression | Self::ReviewFinding | Self::Incident
        )
    }

    pub fn is_decision(self) -> bool {
        matches!(
            self,
            Self::ArchitectureDecision | Self::Invariant | Self::Convention | Self::Procedure
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLifecycle {
    Active,
    Resolved,
    Superseded,
    Retracted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum AccessScope {
    Private,
    Repository,
    Workspace { workspace: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Unknown,
    Passed,
    Failed,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationOutcome {
    pub status: VerificationStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
}

impl Default for VerificationOutcome {
    fn default() -> Self {
        Self {
            status: VerificationStatus::Unknown,
            commands: Vec::new(),
            notes: String::new(),
        }
    }
}

/// A raw artifact from which the memory was derived. `uri` is a stable locator:
/// `git:<sha>`, a review URL, an ADR path at a revision, or an agent task URI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceArtifact {
    pub kind: String,
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// Revision-aware identity for a code symbol affected by a memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SymbolAnchor {
    pub node_id: String,
    pub label: String,
    pub source_file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default = "one")]
    pub confidence: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolChangeKind {
    Renamed,
}

/// A symbol identity transition observed between two revisions. The old anchor
/// may be inferred from a patch when no historical graph is available, while
/// the new anchor is resolved against the graph for the ingested revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SymbolChange {
    pub kind: SymbolChangeKind,
    pub old: SymbolAnchor,
    pub new: SymbolAnchor,
    pub confidence: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
}

/// Revision-aware file lineage retained by Git episodes. Rename and copy
/// records keep both endpoints so history remains discoverable after a move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathChange {
    pub kind: PathChangeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_path: Option<String>,
}

fn one() -> f32 {
    1.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRelation {
    Changed,
    Introduced,
    Fixed,
    Regressed,
    MotivatedBy,
    ReviewedBy,
    FailedBecause,
    VerifiedBy,
    Supersedes,
    ImplementsDecision,
    ViolatesDecision,
    SimilarTo,
    ObservedIn,
    RolledBackBy,
    AppliesTo,
    DerivedFrom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryLink {
    pub relation: MemoryRelation,
    pub target: String,
}

/// One immutable repository-memory record. Corrections create another record
/// with a `supersedes` link instead of rewriting the original observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub version: u32,
    pub id: String,
    pub idempotency_key: String,
    pub kind: MemoryKind,
    pub title: String,
    pub summary: String,
    pub repository: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub occurred_at: i64,
    pub recorded_at: i64,
    pub sources: Vec<SourceArtifact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub affected_symbols: Vec<SymbolAnchor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symbol_changes: Vec<SymbolChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_changes: Vec<PathChange>,
    #[serde(default)]
    pub verification: VerificationOutcome,
    pub confidence: f32,
    pub lifecycle: MemoryLifecycle,
    pub access_scope: AccessScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<MemoryLink>,
}

impl MemoryRecord {
    pub const VERSION: u32 = 3;

    pub fn new(
        idempotency_key: impl Into<String>,
        kind: MemoryKind,
        title: impl Into<String>,
        summary: impl Into<String>,
        repository: impl Into<String>,
        occurred_at: i64,
        sources: Vec<SourceArtifact>,
    ) -> Self {
        let idempotency_key = idempotency_key.into();
        let repository = repository.into();
        let id = stable_id(&repository, &idempotency_key);
        Self {
            version: Self::VERSION,
            id,
            idempotency_key,
            kind,
            title: title.into(),
            summary: summary.into(),
            repository,
            branch: None,
            commit: None,
            occurred_at,
            recorded_at: occurred_at,
            sources,
            affected_symbols: Vec::new(),
            symbol_changes: Vec::new(),
            path_changes: Vec::new(),
            verification: VerificationOutcome::default(),
            confidence: 1.0,
            lifecycle: MemoryLifecycle::Active,
            access_scope: AccessScope::Private,
            owner: None,
            links: Vec::new(),
        }
    }
}

fn stable_id(repository: &str, key: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(repository.as_bytes());
    hasher.update(b"\0");
    hasher.update(key.as_bytes());
    format!("mem_{}", &hasher.finalize().to_hex()[..24])
}
