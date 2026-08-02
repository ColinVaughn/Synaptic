use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::VerificationReport;

const WINDOWS_IO_RETRIES: usize = 40;
const WINDOWS_IO_RETRY_DELAY: Duration = Duration::from_millis(5);
static TEMPORARY_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Planned,
    NotApplicable,
    ReviewRequired,
    Repairing,
    RepairFailed,
    VerificationFailed,
    Verified,
    PrOpen,
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiRunRecord {
    pub version: u32,
    pub id: String,
    pub repository_identity: String,
    pub base_sha: String,
    pub event_id: String,
    pub policy_digest: String,
    pub state: RunState,
    pub attempts: usize,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub verification: Option<VerificationReport>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pull_request_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ApiRunStore {
    root: PathBuf,
}

impl ApiRunStore {
    pub fn new(repository_root: &Path) -> Self {
        Self {
            root: repository_root.join(".synaptic/api-maintenance/runs"),
        }
    }

    pub fn begin(
        &self,
        repository_identity: &str,
        base_sha: &str,
        event_id: &str,
        policy_digest: &str,
    ) -> Result<ApiRunRecord, LedgerError> {
        fs::create_dir_all(&self.root)?;
        let _lock = RunLock::acquire(&self.root)?;
        let identity =
            serde_json::to_vec(&(repository_identity, base_sha, event_id, policy_digest))?;
        let digest = blake3::hash(&identity).to_hex().to_string();
        let id = format!("api_run_{}", &digest[..24]);
        let path = self.path(&id)?;
        if path.exists() {
            let record = read_record(&path)?;
            if record.repository_identity != repository_identity
                || record.base_sha != base_sha
                || record.event_id != event_id
                || record.policy_digest != policy_digest
            {
                return Err(LedgerError::Integrity(format!(
                    "run id {id} maps to different identity fields"
                )));
            }
            return Ok(record);
        }
        let now = unix_timestamp();
        let record = ApiRunRecord {
            version: 1,
            id,
            repository_identity: repository_identity.into(),
            base_sha: base_sha.into(),
            event_id: event_id.into(),
            policy_digest: policy_digest.into(),
            state: RunState::Planned,
            attempts: 0,
            created_at: now,
            updated_at: now,
            verification: None,
            pull_request_url: None,
        };
        write_record(&path, &record)?;
        Ok(record)
    }

    pub fn transition(
        &self,
        record: &mut ApiRunRecord,
        next: RunState,
        verification: Option<VerificationReport>,
        pull_request_url: Option<String>,
    ) -> Result<(), LedgerError> {
        fs::create_dir_all(&self.root)?;
        let _lock = RunLock::acquire(&self.root)?;
        let path = self.path(&record.id)?;
        let disk = read_record(&path)?;
        if disk != *record {
            return Err(LedgerError::ConcurrentUpdate(record.id.clone()));
        }
        if !valid_transition(record.state, next) {
            return Err(LedgerError::InvalidTransition {
                from: record.state,
                to: next,
            });
        }
        if next == RunState::Verified
            && !verification.as_ref().is_some_and(|report| report.verified)
        {
            return Err(LedgerError::UnverifiedTransition);
        }
        if next == RunState::PrOpen
            && (record.state != RunState::Verified
                || pull_request_url.as_deref().is_none_or(str::is_empty))
        {
            return Err(LedgerError::UnverifiedTransition);
        }
        record.state = next;
        if next == RunState::Repairing {
            record.attempts += 1;
        }
        if let Some(verification) = verification {
            record.verification = Some(verification);
        }
        if let Some(url) = pull_request_url {
            record.pull_request_url = Some(url);
        }
        record.updated_at = unix_timestamp();
        write_record(&path, record)
    }

    pub fn load(&self, id: &str) -> Result<ApiRunRecord, LedgerError> {
        fs::create_dir_all(&self.root)?;
        let _lock = RunLock::acquire(&self.root)?;
        read_record(&self.path(id)?)
    }

    pub fn list(&self) -> Result<Vec<ApiRunRecord>, LedgerError> {
        fs::create_dir_all(&self.root)?;
        let _lock = RunLock::acquire(&self.root)?;
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(LedgerError::Io(error)),
        };
        let mut records = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
                records.push(read_record(&path)?);
            }
        }
        records.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(records)
    }

    fn path(&self, id: &str) -> Result<PathBuf, LedgerError> {
        if id.is_empty()
            || !id.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
        {
            return Err(LedgerError::InvalidId(id.into()));
        }
        Ok(self.root.join(format!("{id}.json")))
    }
}

fn valid_transition(from: RunState, to: RunState) -> bool {
    from == to
        || to == RunState::Superseded
        || matches!(
            (from, to),
            (
                RunState::Planned,
                RunState::NotApplicable | RunState::ReviewRequired | RunState::Repairing
            ) | (
                RunState::Repairing,
                RunState::RepairFailed | RunState::VerificationFailed | RunState::Verified
            ) | (
                RunState::RepairFailed | RunState::VerificationFailed,
                RunState::Repairing
            ) | (RunState::Verified, RunState::PrOpen)
        )
}

fn read_record(path: &Path) -> Result<ApiRunRecord, LedgerError> {
    let record: ApiRunRecord = serde_json::from_slice(&fs::read(path)?)?;
    if record.version != 1 || path.file_stem().and_then(|stem| stem.to_str()) != Some(&record.id) {
        return Err(LedgerError::Integrity(format!(
            "run record identity mismatch at {}",
            path.display()
        )));
    }
    Ok(record)
}

fn write_record(path: &Path, record: &ApiRunRecord) -> Result<(), LedgerError> {
    let mut bytes = serde_json::to_vec_pretty(record)?;
    bytes.push(b'\n');
    let temporary = temporary_path(path);
    retry_transient_windows_io(|| fs::write(&temporary, &bytes))?;
    if path.exists() {
        retry_transient_windows_io(|| match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        })?;
    }
    if let Err(error) = retry_transient_windows_io(|| fs::rename(&temporary, path)) {
        let _ = fs::remove_file(temporary);
        return Err(LedgerError::Io(error));
    }
    Ok(())
}

struct RunLock {
    path: PathBuf,
}

impl RunLock {
    fn acquire(root: &Path) -> Result<Self, LedgerError> {
        let path = root.join(".ledger.lock");
        for _ in 0..200 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists
                        || is_transient_windows_access_error(&error) =>
                {
                    if fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > Duration::from_secs(60))
                    {
                        let _ = fs::remove_file(&path);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(LedgerError::Io(error)),
            }
        }
        Err(LedgerError::LockTimeout)
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let sequence = TEMPORARY_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!("tmp-{}-{sequence}", std::process::id()))
}

fn retry_transient_windows_io(
    mut operation: impl FnMut() -> std::io::Result<()>,
) -> std::io::Result<()> {
    for attempt in 0..WINDOWS_IO_RETRIES {
        match operation() {
            Ok(()) => return Ok(()),
            Err(error)
                if is_transient_windows_access_error(&error)
                    && attempt + 1 < WINDOWS_IO_RETRIES =>
            {
                std::thread::sleep(WINDOWS_IO_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("the retry loop always returns on its last attempt")
}

fn is_transient_windows_access_error(error: &std::io::Error) -> bool {
    cfg!(windows) && error.kind() == std::io::ErrorKind::PermissionDenied
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("invalid run id {0:?}")]
    InvalidId(String),
    #[error("run ledger integrity error: {0}")]
    Integrity(String),
    #[error("invalid run transition {from:?} -> {to:?}")]
    InvalidTransition { from: RunState, to: RunState },
    #[error("verified/pr_open transitions require conclusive verification and a PR URL")]
    UnverifiedTransition,
    #[error("run {0} changed concurrently")]
    ConcurrentUpdate(String),
    #[error("timed out acquiring run ledger lock")]
    LockTimeout,
    #[error("run ledger I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("run ledger JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}
