use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{ApiChangeEvent, ApiContract};

const LOCK_VERSION: u32 = 1;
const WINDOWS_IO_RETRIES: usize = 40;
const WINDOWS_IO_RETRY_DELAY: Duration = Duration::from_millis(5);
static TEMPORARY_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLockState {
    pub vendor: String,
    pub source_uri: String,
    pub revision: String,
    pub content_digest: String,
    #[serde(default)]
    pub checked_at: i64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub etag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_modified: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub contract_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SourceLockFile {
    version: u32,
    sources: BTreeMap<String, SourceLockState>,
}

impl Default for SourceLockFile {
    fn default() -> Self {
        Self {
            version: LOCK_VERSION,
            sources: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApiEventStore {
    root: PathBuf,
}

impl ApiEventStore {
    pub fn new(repository_root: &Path) -> Self {
        Self {
            root: repository_root.join(".synaptic/api-maintenance"),
        }
    }

    pub fn put_event(&self, event: &ApiChangeEvent) -> Result<PathBuf, StoreError> {
        validate_key(&event.id)?;
        let path = self.root.join("events").join(format!("{}.json", event.id));
        put_immutable_json(&path, event)?;
        Ok(path)
    }

    pub fn load_event(&self, id: &str) -> Result<ApiChangeEvent, StoreError> {
        validate_key(id)?;
        let path = self.root.join("events").join(format!("{id}.json"));
        let event: ApiChangeEvent = serde_json::from_slice(&fs::read(&path)?)?;
        if event.id != id {
            return Err(StoreError::Integrity(format!(
                "event id {} does not match filename {id}",
                event.id
            )));
        }
        Ok(event)
    }

    pub fn put_contract(&self, contract: &ApiContract) -> Result<PathBuf, StoreError> {
        validate_key(&contract.vendor)?;
        validate_key(&contract.digest)?;
        let path = self
            .root
            .join("contracts")
            .join(&contract.vendor)
            .join(format!("{}.json", contract.digest));
        put_immutable_json(&path, contract)?;
        Ok(path)
    }

    pub fn load_contract(&self, vendor: &str, digest: &str) -> Result<ApiContract, StoreError> {
        validate_key(vendor)?;
        validate_key(digest)?;
        let path = self
            .root
            .join("contracts")
            .join(vendor)
            .join(format!("{digest}.json"));
        let contract: ApiContract = serde_json::from_slice(&fs::read(path)?)?;
        if contract.vendor != vendor || contract.digest != digest {
            return Err(StoreError::Integrity(
                "contract identity does not match storage path".into(),
            ));
        }
        Ok(contract)
    }

    pub fn source_state(
        &self,
        vendor: &str,
        source_uri: &str,
    ) -> Result<Option<SourceLockState>, StoreError> {
        fs::create_dir_all(&self.root)?;
        let _lock = StoreLock::acquire(&self.root)?;
        let lock = self.load_lock()?;
        Ok(lock.sources.get(&source_key(vendor, source_uri)).cloned())
    }

    /// Atomically advance a source cursor. Reusing a revision with different
    /// bytes is an integrity failure, never a new event.
    pub fn record_source(&self, state: SourceLockState) -> Result<(), StoreError> {
        fs::create_dir_all(&self.root)?;
        let _lock = StoreLock::acquire(&self.root)?;
        let mut lock = self.load_lock()?;
        let key = source_key(&state.vendor, &state.source_uri);
        if let Some(prior) = lock.sources.get(&key)
            && prior.revision == state.revision
            && prior.content_digest != state.content_digest
        {
            return Err(StoreError::Integrity(format!(
                "source {} changed payload under the same revision {}",
                state.source_uri, state.revision
            )));
        }
        lock.sources.insert(key, state);
        self.write_lock(&lock)
    }

    pub fn list_events(&self) -> Result<Vec<ApiChangeEvent>, StoreError> {
        let directory = self.root.join("events");
        let mut events = Vec::new();
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(events),
            Err(error) => return Err(StoreError::Io(error)),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let event: ApiChangeEvent = serde_json::from_slice(&fs::read(path)?)?;
            events.push(event);
        }
        events.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(events)
    }

    pub fn put_artifact(&self, digest: &str, bytes: &[u8]) -> Result<PathBuf, StoreError> {
        validate_key(digest)?;
        if blake3::hash(bytes).to_hex().as_str() != digest {
            return Err(StoreError::Integrity("raw artifact digest mismatch".into()));
        }
        let path = self.root.join("artifacts").join(format!("{digest}.bin"));
        if path.exists() {
            if fs::read(&path)? == bytes {
                return Ok(path);
            }
            return Err(StoreError::Integrity(format!(
                "cached artifact changed at {}",
                path.display()
            )));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = temporary_path(&path);
        fs::write(&temporary, bytes)?;
        match fs::rename(&temporary, &path) {
            Ok(()) => Ok(path),
            Err(_error) if path.exists() && fs::read(&path)? == bytes => {
                let _ = fs::remove_file(temporary);
                Ok(path)
            }
            Err(error) => {
                let _ = fs::remove_file(temporary);
                Err(StoreError::Io(error))
            }
        }
    }

    pub fn load_artifact(&self, digest: &str) -> Result<Vec<u8>, StoreError> {
        validate_key(digest)?;
        let bytes = fs::read(self.root.join("artifacts").join(format!("{digest}.bin")))?;
        if blake3::hash(&bytes).to_hex().as_str() != digest {
            return Err(StoreError::Integrity(format!(
                "cached artifact {digest} failed digest validation"
            )));
        }
        Ok(bytes)
    }

    fn load_lock(&self) -> Result<SourceLockFile, StoreError> {
        let path = self.root.join("lock.json");
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SourceLockFile::default());
            }
            Err(error) => return Err(StoreError::Io(error)),
        };
        let lock: SourceLockFile = serde_json::from_slice(&bytes)?;
        if lock.version != LOCK_VERSION {
            return Err(StoreError::Integrity(format!(
                "unsupported source lock version {}",
                lock.version
            )));
        }
        Ok(lock)
    }

    fn write_lock(&self, lock: &SourceLockFile) -> Result<(), StoreError> {
        let path = self.root.join("lock.json");
        let mut bytes = serde_json::to_vec_pretty(lock)?;
        bytes.push(b'\n');
        write_atomic(&path, &bytes)
    }
}

struct StoreLock {
    path: PathBuf,
}

impl StoreLock {
    fn acquire(root: &Path) -> Result<Self, StoreError> {
        let path = root.join(".store.lock");
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
                Err(error) => return Err(StoreError::Io(error)),
            }
        }
        Err(StoreError::LockTimeout)
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn source_key(vendor: &str, uri: &str) -> String {
    blake3::hash(format!("{}\0{}", vendor.to_ascii_lowercase(), uri).as_bytes())
        .to_hex()
        .to_string()
}

fn put_immutable_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    if path.exists() {
        let prior = fs::read(path)?;
        if prior == bytes {
            return Ok(());
        }
        return Err(StoreError::Integrity(format!(
            "immutable artifact changed under the same identity: {}",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_path(path);
    fs::write(&temporary, &bytes)?;
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(_error) if path.exists() => {
            let _ = fs::remove_file(&temporary);
            let prior = fs::read(path)?;
            if prior == bytes {
                Ok(())
            } else {
                Err(StoreError::Integrity(format!(
                    "concurrent immutable artifact conflict at {}",
                    path.display()
                )))
            }
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(StoreError::Io(error))
        }
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_path(path);
    fs::write(&temporary, bytes)?;
    if let Err(error) = fs::rename(&temporary, path) {
        // Windows does not replace an existing destination atomically. Remove
        // only this exact lock file and retry the rename; event files still use
        // immutable create semantics above.
        if path.exists() {
            retry_transient_windows_io(|| match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            })?;
            retry_transient_windows_io(|| fs::rename(&temporary, path))?;
        } else {
            let _ = fs::remove_file(&temporary);
            return Err(StoreError::Io(error));
        }
    }
    Ok(())
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

fn validate_key(key: &str) -> Result<(), StoreError> {
    if key.is_empty()
        || !key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(StoreError::InvalidKey(key.to_string()));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("invalid artifact key {0:?}")]
    InvalidKey(String),
    #[error("artifact integrity error: {0}")]
    Integrity(String),
    #[error("artifact I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("artifact JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("timed out acquiring API event-store lock")]
    LockTimeout,
}
