use std::path::Path;

use serde::{Deserialize, Serialize};
use synaptic_graph::KnowledgeGraph;

use crate::{
    AccessScope, MemoryKind, MemoryLifecycle, MemoryLink, MemoryRecord, MemoryRelation,
    MemoryStore, PathChange, PathChangeKind, RecordOutcome, SourceArtifact, SymbolAnchor,
    VerificationOutcome, VerificationStatus,
};

const SCHEMA: &str = "synaptic.memory-artifacts/v1";
const MAX_ARTIFACTS: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactIngestReport {
    pub scanned: usize,
    pub created: usize,
    pub already_present: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactIngestError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("artifact schema must be {SCHEMA:?}, got {0:?}")]
    InvalidSchema(String),
    #[error("artifact file exceeds the {MAX_ARTIFACTS} record safety limit")]
    TooManyArtifacts,
    #[error("unsupported external artifact kind {0:?}")]
    UnsupportedKind(String),
    #[error(transparent)]
    Memory(#[from] crate::MemoryError),
}

#[derive(Debug, Deserialize)]
struct ArtifactEnvelope {
    schema: String,
    artifacts: Vec<ExternalArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExternalArtifact {
    kind: MemoryKind,
    external_id: String,
    title: String,
    summary: String,
    source_uri: String,
    repository: String,
    occurred_at: i64,
    #[serde(default)]
    status: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    affected_symbols: Vec<String>,
    #[serde(default)]
    affected_files: Vec<String>,
    #[serde(default)]
    verification_status: Option<String>,
    #[serde(default)]
    verification_commands: Vec<String>,
    #[serde(default)]
    verification_notes: String,
    #[serde(default = "default_confidence")]
    confidence: f32,
    #[serde(default = "default_scope")]
    scope: String,
    #[serde(default)]
    links: Vec<MemoryLink>,
}

/// Ingest a deterministic, portable artifact envelope produced by issue
/// trackers, code-review systems, CI, incident tooling, release automation, or
/// an agent runner. Every item retains its source URI plus a digest of the exact
/// canonical adapter payload.
pub fn ingest_artifact_file(
    store: &MemoryStore,
    path: &Path,
    graph: Option<&KnowledgeGraph>,
) -> Result<ArtifactIngestReport, ArtifactIngestError> {
    let envelope: ArtifactEnvelope = serde_json::from_slice(&std::fs::read(path)?)?;
    if envelope.schema != SCHEMA {
        return Err(ArtifactIngestError::InvalidSchema(envelope.schema));
    }
    if envelope.artifacts.len() > MAX_ARTIFACTS {
        return Err(ArtifactIngestError::TooManyArtifacts);
    }
    let mut existing = store.all()?;
    let mut report = ArtifactIngestReport {
        scanned: envelope.artifacts.len(),
        created: 0,
        already_present: 0,
    };
    for artifact in envelope.artifacts {
        validate_kind(artifact.kind)?;
        let bytes = serde_json::to_vec(&artifact)?;
        let digest = blake3::hash(&bytes).to_hex().to_string();
        let mut record = MemoryRecord::new(
            format!(
                "external:{}:{}:{}",
                artifact.kind.as_str(),
                artifact.external_id,
                digest
            ),
            artifact.kind,
            artifact.title,
            artifact.summary,
            artifact.repository,
            artifact.occurred_at,
            vec![SourceArtifact {
                kind: artifact.kind.as_str().to_string(),
                uri: artifact.source_uri.clone(),
                revision: artifact.commit.clone(),
                digest: Some(digest),
            }],
        );
        record.branch = artifact.branch;
        record.commit = artifact.commit.clone();
        record.lifecycle = lifecycle(&artifact.status);
        record.confidence = artifact.confidence.clamp(0.0, 1.0);
        record.access_scope = parse_scope(&artifact.scope);
        record.verification = VerificationOutcome {
            status: parse_verification(artifact.verification_status.as_deref()),
            commands: artifact.verification_commands,
            notes: artifact.verification_notes,
        };
        record.affected_symbols = artifact
            .affected_symbols
            .iter()
            .map(|symbol| resolve_anchor(graph, symbol, artifact.commit.as_deref()))
            .collect();
        record.path_changes = artifact
            .affected_files
            .into_iter()
            .map(|path| {
                let path = path.replace('\\', "/");
                PathChange {
                    kind: PathChangeKind::Modified,
                    old_path: Some(path.clone()),
                    new_path: Some(path),
                }
            })
            .collect();
        record.links = artifact.links;
        if let Some(previous) = existing
            .iter()
            .filter(|candidate| {
                candidate.id != record.id
                    && candidate.repository == record.repository
                    && candidate
                        .sources
                        .iter()
                        .any(|source| source.uri == artifact.source_uri)
                    && !matches!(
                        candidate.lifecycle,
                        MemoryLifecycle::Superseded | MemoryLifecycle::Retracted
                    )
            })
            .max_by_key(|candidate| (candidate.occurred_at, candidate.recorded_at))
        {
            record.links.push(MemoryLink {
                relation: MemoryRelation::Supersedes,
                target: previous.id.clone(),
            });
        }
        match store.record(&record)? {
            RecordOutcome::Created => {
                report.created += 1;
                existing.push(record);
            }
            RecordOutcome::AlreadyPresent => report.already_present += 1,
        }
    }
    Ok(report)
}

fn validate_kind(kind: MemoryKind) -> Result<(), ArtifactIngestError> {
    if matches!(
        kind,
        MemoryKind::Issue
            | MemoryKind::PullRequest
            | MemoryKind::ReviewFinding
            | MemoryKind::CiRun
            | MemoryKind::Incident
            | MemoryKind::Release
            | MemoryKind::AgentTask
            | MemoryKind::CustomerReport
            | MemoryKind::Regression
            | MemoryKind::FailedAttempt
    ) {
        Ok(())
    } else {
        Err(ArtifactIngestError::UnsupportedKind(
            kind.as_str().to_string(),
        ))
    }
}

fn resolve_anchor(
    graph: Option<&KnowledgeGraph>,
    symbol: &str,
    commit: Option<&str>,
) -> SymbolAnchor {
    let matches = graph
        .into_iter()
        .flat_map(KnowledgeGraph::nodes)
        .filter(|node| {
            node.id.0.eq_ignore_ascii_case(symbol) || node.label.eq_ignore_ascii_case(symbol)
        })
        .collect::<Vec<_>>();
    if matches.len() == 1 {
        let node = matches[0];
        SymbolAnchor {
            node_id: node.id.0.clone(),
            label: node.label.clone(),
            source_file: node.source_file.replace('\\', "/"),
            repo: node.repo.clone(),
            commit: commit.map(str::to_string),
            confidence: 1.0,
        }
    } else {
        SymbolAnchor {
            node_id: symbol.to_string(),
            label: symbol.to_string(),
            source_file: String::new(),
            repo: None,
            commit: commit.map(str::to_string),
            confidence: 0.5,
        }
    }
}

fn lifecycle(status: &str) -> MemoryLifecycle {
    match status.trim().to_ascii_lowercase().as_str() {
        "superseded" | "deprecated" => MemoryLifecycle::Superseded,
        "retracted" | "rejected" | "withdrawn" => MemoryLifecycle::Retracted,
        "resolved" | "closed" | "passed" | "failed" | "released" | "completed" | "merged" => {
            MemoryLifecycle::Resolved
        }
        _ => MemoryLifecycle::Active,
    }
}

fn parse_verification(status: Option<&str>) -> VerificationStatus {
    match status
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "passed" | "success" | "succeeded" => VerificationStatus::Passed,
        "failed" | "failure" => VerificationStatus::Failed,
        "partial" => VerificationStatus::Partial,
        _ => VerificationStatus::Unknown,
    }
}

fn parse_scope(scope: &str) -> AccessScope {
    let scope = scope.trim();
    if scope.eq_ignore_ascii_case("repository") {
        AccessScope::Repository
    } else if let Some(workspace) = scope.strip_prefix("workspace:") {
        AccessScope::Workspace {
            workspace: workspace.trim().to_string(),
        }
    } else {
        AccessScope::Private
    }
}

fn default_confidence() -> f32 {
    1.0
}

fn default_scope() -> String {
    "private".to_string()
}
