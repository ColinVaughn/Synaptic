use std::collections::{BTreeSet, VecDeque};
use std::path::{Component, Path};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedApiJob {
    pub version: u32,
    pub tenant_id: String,
    pub repository_identity: String,
    pub base_sha: String,
    pub event_id: String,
    pub policy_digest: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordination_group: Option<String>,
}

impl HostedApiJob {
    pub fn new(
        tenant_id: impl Into<String>,
        repository_identity: impl Into<String>,
        base_sha: impl Into<String>,
        event_id: impl Into<String>,
        policy_digest: impl Into<String>,
    ) -> Self {
        let tenant_id = tenant_id.into();
        let repository_identity = repository_identity.into();
        let base_sha = base_sha.into();
        let event_id = event_id.into();
        let policy_digest = policy_digest.into();
        let identity =
            format!("{tenant_id}\0{repository_identity}\0{base_sha}\0{event_id}\0{policy_digest}");
        let key = format!(
            "api_job_{}",
            &blake3::hash(identity.as_bytes()).to_hex()[..24]
        );
        Self {
            version: 1,
            tenant_id,
            repository_identity,
            base_sha,
            event_id,
            policy_digest,
            key,
            coordination_group: None,
        }
    }

    /// Validates a scheduler-provided workspace without touching the filesystem.
    ///
    /// Hosted schedulers should create the assigned root themselves. This check makes
    /// sure a job cannot redirect a repair into a sibling tenant or repository.
    pub fn validate_workspace(
        &self,
        assigned_root: &Path,
        candidate: &Path,
    ) -> Result<(), QueueError> {
        if !assigned_root.is_absolute()
            || !candidate.is_absolute()
            || has_unsafe_component(assigned_root)
            || has_unsafe_component(candidate)
            || !candidate.starts_with(assigned_root)
        {
            return Err(QueueError::WorkspaceEscape);
        }
        let root = assigned_root
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase();
        let tenant = safe_component(&self.tenant_id);
        let repository = safe_component(&self.repository_identity);
        let expected = format!("/{tenant}/{repository}");
        if !root.ends_with(&expected) {
            return Err(QueueError::WorkspaceIdentityMismatch);
        }
        Ok(())
    }
}

fn has_unsafe_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn safe_component(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum QueueError {
    #[error("queue capacity must be between 1 and 100000")]
    InvalidCapacity,
    #[error("worker queue is full")]
    Full,
    #[error("workspace escapes its assigned repository root")]
    WorkspaceEscape,
    #[error("workspace root does not match the job tenant and repository")]
    WorkspaceIdentityMismatch,
    #[error("duplicate repository impact for {0}")]
    DuplicateRepository(String),
    #[error("retry policy is invalid")]
    InvalidRetryPolicy,
}

#[derive(Debug)]
pub struct BoundedJobQueue {
    capacity: usize,
    queued: Mutex<VecDeque<HostedApiJob>>,
    known_keys: Mutex<BTreeSet<String>>,
}

impl BoundedJobQueue {
    pub fn new(capacity: usize) -> Result<Self, QueueError> {
        if !(1..=100_000).contains(&capacity) {
            return Err(QueueError::InvalidCapacity);
        }
        Ok(Self {
            capacity,
            queued: Mutex::new(VecDeque::with_capacity(capacity.min(1024))),
            known_keys: Mutex::new(BTreeSet::new()),
        })
    }

    /// Returns `false` for a job already observed by this queue.
    pub fn enqueue(&self, job: HostedApiJob) -> Result<bool, QueueError> {
        let mut keys = self
            .known_keys
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if keys.contains(&job.key) {
            return Ok(false);
        }
        let mut queued = self
            .queued
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if queued.len() >= self.capacity {
            return Err(QueueError::Full);
        }
        keys.insert(job.key.clone());
        queued.push_back(job);
        Ok(true)
    }

    /// Claims only a job belonging to the authenticated tenant partition.
    pub fn claim(&self, tenant_id: &str) -> Option<HostedApiJob> {
        let mut queued = self
            .queued
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let index = queued.iter().position(|job| job.tenant_id == tenant_id)?;
        queued.remove(index)
    }
}

#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay_seconds: u64,
    pub max_delay_seconds: u64,
}

impl RetryPolicy {
    pub fn validate(self) -> Result<Self, QueueError> {
        if !(1..=10).contains(&self.max_attempts)
            || self.base_delay_seconds == 0
            || self.max_delay_seconds < self.base_delay_seconds
            || self.max_delay_seconds > 86_400
        {
            return Err(QueueError::InvalidRetryPolicy);
        }
        Ok(self)
    }

    pub fn delay_for_attempt(self, attempt: u32) -> u64 {
        let exponent = attempt.saturating_sub(1).min(20);
        self.base_delay_seconds
            .saturating_mul(1_u64 << exponent)
            .min(self.max_delay_seconds)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerEventState {
    Started,
    RetryScheduled,
    Cancelled,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerEvent {
    pub job_key: String,
    pub tenant_id: String,
    pub repository_identity: String,
    pub attempt: u32,
    pub state: WorkerEventState,
    pub elapsed_millis: u64,
    pub message: String,
}

pub trait WorkerEventSink: Send + Sync {
    fn record(&self, event: WorkerEvent);
}

pub trait WorkerJobRunner: Send + Sync {
    fn run(&self, job: &HostedApiJob, cancellation: &CancellationToken) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerAttemptOutcome {
    Succeeded,
    RetryScheduled { after_seconds: u64 },
    Failed,
    Cancelled,
}

pub fn execute_worker_attempt(
    job: &HostedApiJob,
    attempt: u32,
    retry: &RetryPolicy,
    cancellation: &CancellationToken,
    runner: &dyn WorkerJobRunner,
    events: &dyn WorkerEventSink,
) -> WorkerAttemptOutcome {
    let started = Instant::now();
    if cancellation.is_cancelled() {
        emit(
            events,
            job,
            attempt,
            WorkerEventState::Cancelled,
            started,
            "cancelled",
        );
        return WorkerAttemptOutcome::Cancelled;
    }
    let retry = match retry.validate() {
        Ok(policy) if attempt > 0 && attempt <= policy.max_attempts => policy,
        _ => {
            emit(
                events,
                job,
                attempt,
                WorkerEventState::Failed,
                started,
                "invalid retry attempt",
            );
            return WorkerAttemptOutcome::Failed;
        }
    };
    emit(
        events,
        job,
        attempt,
        WorkerEventState::Started,
        started,
        "started",
    );
    match runner.run(job, cancellation) {
        Ok(()) if cancellation.is_cancelled() => {
            emit(
                events,
                job,
                attempt,
                WorkerEventState::Cancelled,
                started,
                "cancelled",
            );
            WorkerAttemptOutcome::Cancelled
        }
        Ok(()) => {
            emit(
                events,
                job,
                attempt,
                WorkerEventState::Succeeded,
                started,
                "succeeded",
            );
            WorkerAttemptOutcome::Succeeded
        }
        Err(error) if attempt < retry.max_attempts && !cancellation.is_cancelled() => {
            let delay = retry.delay_for_attempt(attempt);
            emit(
                events,
                job,
                attempt,
                WorkerEventState::RetryScheduled,
                started,
                &format!("retry in {delay}s: {}", redact_message(&error)),
            );
            WorkerAttemptOutcome::RetryScheduled {
                after_seconds: delay,
            }
        }
        Err(error) => {
            emit(
                events,
                job,
                attempt,
                WorkerEventState::Failed,
                started,
                &redact_message(&error),
            );
            WorkerAttemptOutcome::Failed
        }
    }
}

fn emit(
    sink: &dyn WorkerEventSink,
    job: &HostedApiJob,
    attempt: u32,
    state: WorkerEventState,
    started: Instant,
    message: &str,
) {
    sink.record(WorkerEvent {
        job_key: job.key.clone(),
        tenant_id: job.tenant_id.clone(),
        repository_identity: job.repository_identity.clone(),
        attempt,
        state,
        elapsed_millis: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        message: redact_message(message),
    });
}

fn redact_message(message: &str) -> String {
    crate::redaction::redact_sensitive_text(&message.chars().take(2_000).collect::<String>())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStage {
    Fetch,
    Repair,
    Test,
    Publish,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialScope {
    None,
    RepositoryWrite {
        tenant_id: String,
        repository_identity: String,
    },
}

pub fn credential_scope_for_stage(job: &HostedApiJob, stage: JobStage) -> CredentialScope {
    match stage {
        JobStage::Fetch | JobStage::Repair | JobStage::Test => CredentialScope::None,
        JobStage::Publish => CredentialScope::RepositoryWrite {
            tenant_id: job.tenant_id.clone(),
            repository_identity: job.repository_identity.clone(),
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryImpact {
    pub tenant_id: String,
    pub repository_identity: String,
    pub seed_node_ids: Vec<String>,
}

impl RepositoryImpact {
    pub fn new(
        tenant_id: impl Into<String>,
        repository_identity: impl Into<String>,
        seed_node_ids: Vec<String>,
    ) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            repository_identity: repository_identity.into(),
            seed_node_ids,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatedRepositoryRepair {
    pub tenant_id: String,
    pub repository_identity: String,
    pub event_id: String,
    pub coordination_group: String,
    pub seed_node_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinationPlan {
    pub version: u32,
    pub event_id: String,
    pub repositories: Vec<CoordinatedRepositoryRepair>,
}

pub fn build_coordination_plan(
    event_id: &str,
    mut impacts: Vec<RepositoryImpact>,
) -> Result<CoordinationPlan, QueueError> {
    impacts.sort_by(|a, b| {
        a.tenant_id
            .cmp(&b.tenant_id)
            .then_with(|| a.repository_identity.cmp(&b.repository_identity))
    });
    let mut identities = BTreeSet::new();
    let mut repositories = Vec::with_capacity(impacts.len());
    for mut impact in impacts {
        let identity = format!("{}/{}", impact.tenant_id, impact.repository_identity);
        if !identities.insert(identity.clone()) {
            return Err(QueueError::DuplicateRepository(identity));
        }
        impact.seed_node_ids.sort();
        impact.seed_node_ids.dedup();
        let group_identity = format!("{}\0{}", impact.tenant_id, event_id);
        let coordination_group = format!(
            "api_coord_{}",
            &blake3::hash(group_identity.as_bytes()).to_hex()[..20]
        );
        repositories.push(CoordinatedRepositoryRepair {
            tenant_id: impact.tenant_id,
            repository_identity: impact.repository_identity,
            event_id: event_id.into(),
            coordination_group,
            seed_node_ids: impact.seed_node_ids,
        });
    }
    Ok(CoordinationPlan {
        version: 1,
        event_id: event_id.into(),
        repositories,
    })
}
