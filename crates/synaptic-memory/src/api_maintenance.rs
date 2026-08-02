use serde::{Deserialize, Serialize};

use crate::{
    AccessScope, MemoryError, MemoryKind, MemoryRecord, MemoryStore, RecordOutcome, SourceArtifact,
    VerificationOutcome, VerificationStatus,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiMaintenanceMemory {
    pub repository: String,
    pub vendor: String,
    pub event_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub run_id: Option<String>,
    pub occurred_at: i64,
    pub source_uri: String,
    pub source_revision: String,
    pub source_digest: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub base_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pull_request_url: Option<String>,
    pub summary: String,
    #[serde(default)]
    pub commands: Vec<String>,
    pub verification: VerificationStatus,
}

/// Record one API-maintenance outcome using existing memory kinds. The
/// idempotency key includes the event, run, and concrete outcome kind.
pub fn record_api_maintenance_memory(
    store: &MemoryStore,
    kind: MemoryKind,
    input: &ApiMaintenanceMemory,
) -> Result<RecordOutcome, ApiMaintenanceMemoryError> {
    if !matches!(
        kind,
        MemoryKind::Release
            | MemoryKind::AgentTask
            | MemoryKind::FailedAttempt
            | MemoryKind::Regression
            | MemoryKind::PullRequest
    ) {
        return Err(ApiMaintenanceMemoryError::UnsupportedKind(kind));
    }
    let run = input.run_id.as_deref().unwrap_or("no-run");
    let key = format!(
        "api-maintenance:{}:{}:{}:{}",
        input.vendor,
        input.event_id,
        run,
        kind.as_str()
    );
    let title = match kind {
        MemoryKind::Release => format!("{} API release {}", input.vendor, input.event_id),
        MemoryKind::AgentTask => format!("{} API maintenance run {run}", input.vendor),
        MemoryKind::FailedAttempt => format!("Failed {} API repair {run}", input.vendor),
        MemoryKind::Regression => format!("{} API migration regression {run}", input.vendor),
        MemoryKind::PullRequest => format!("{} API migration pull request {run}", input.vendor),
        _ => unreachable!("validated above"),
    };
    let mut sources = vec![SourceArtifact {
        kind: "api_vendor_artifact".into(),
        uri: input.source_uri.clone(),
        revision: Some(input.source_revision.clone()),
        digest: Some(input.source_digest.clone()),
    }];
    if let Some(url) = &input.pull_request_url {
        sources.push(SourceArtifact {
            kind: "pull_request".into(),
            uri: url.clone(),
            revision: None,
            digest: None,
        });
    }
    let mut record = MemoryRecord::new(
        key,
        kind,
        title,
        input.summary.chars().take(4_000).collect::<String>(),
        input.repository.clone(),
        input.occurred_at,
        sources,
    );
    record.branch = input.branch.clone();
    record.commit = input.base_sha.clone();
    record.access_scope = AccessScope::Repository;
    record.verification = VerificationOutcome {
        status: input.verification,
        commands: input.commands.iter().take(50).cloned().collect(),
        notes: format!("API event {}; run {run}", input.event_id),
    };
    Ok(store.record(&record)?)
}

#[derive(Debug, thiserror::Error)]
pub enum ApiMaintenanceMemoryError {
    #[error("memory kind {0:?} is not an API maintenance outcome kind")]
    UnsupportedKind(MemoryKind),
    #[error(transparent)]
    Memory(#[from] MemoryError),
}
