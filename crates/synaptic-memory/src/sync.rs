use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{MemoryPrincipal, MemoryRecord, MemoryStore, RecordOutcome};

const SCHEMA: &str = "synaptic.memory-bundle/v1";
const MAX_RECORDS: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportBundleReport {
    pub records: usize,
    pub bytes: usize,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportBundleReport {
    pub records: usize,
    pub created: usize,
    pub already_present: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("bundle schema must be {SCHEMA:?}, got {0:?}")]
    InvalidSchema(String),
    #[error("bundle exceeds the {MAX_RECORDS} record safety limit")]
    TooManyRecords,
    #[error("bundle digest mismatch")]
    DigestMismatch,
    #[error(transparent)]
    Memory(#[from] crate::MemoryError),
}

#[derive(Debug, Serialize, Deserialize)]
struct MemoryBundle {
    schema: String,
    records: Vec<MemoryRecord>,
    digest: String,
}

/// Export only records visible to `principal`. The checksum covers canonical
/// record JSON so transport corruption or tampering is rejected on import.
pub fn export_bundle(
    store: &MemoryStore,
    path: &Path,
    principal: &MemoryPrincipal,
) -> Result<ExportBundleReport, BundleError> {
    let mut records = store.all_authorized(principal)?;
    records.sort_by(|a, b| a.id.cmp(&b.id));
    let digest = digest(&records)?;
    let bundle = MemoryBundle {
        schema: SCHEMA.into(),
        records,
        digest: digest.clone(),
    };
    let bytes = serde_json::to_vec(&bundle)?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    synaptic_core::fsio::write_atomic(path, &bytes)?;
    Ok(ExportBundleReport {
        records: bundle.records.len(),
        bytes: bytes.len(),
        digest,
    })
}

/// Merge a checked bundle into the primary store. Scope policy is evaluated
/// for every record before its idempotent write.
pub fn import_bundle(
    store: &MemoryStore,
    path: &Path,
    principal: &MemoryPrincipal,
) -> Result<ImportBundleReport, BundleError> {
    let bundle: MemoryBundle = serde_json::from_slice(&std::fs::read(path)?)?;
    if bundle.schema != SCHEMA {
        return Err(BundleError::InvalidSchema(bundle.schema));
    }
    if bundle.records.len() > MAX_RECORDS {
        return Err(BundleError::TooManyRecords);
    }
    if digest(&bundle.records)? != bundle.digest {
        return Err(BundleError::DigestMismatch);
    }
    let mut report = ImportBundleReport {
        records: bundle.records.len(),
        created: 0,
        already_present: 0,
    };
    for record in bundle.records {
        match store.record_as(&record, principal)? {
            RecordOutcome::Created => report.created += 1,
            RecordOutcome::AlreadyPresent => report.already_present += 1,
        }
    }
    Ok(report)
}

fn digest(records: &[MemoryRecord]) -> Result<String, serde_json::Error> {
    Ok(blake3::hash(&serde_json::to_vec(records)?)
        .to_hex()
        .to_string())
}
