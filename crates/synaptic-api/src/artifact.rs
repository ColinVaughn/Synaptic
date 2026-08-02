use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactFetchRequest {
    pub uri: String,
    pub max_bytes: u64,
    pub accepted_content_types: Vec<String>,
    pub prior_etag: Option<String>,
    pub prior_last_modified: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchedArtifact {
    pub uri: String,
    pub revision: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
    pub content_digest: String,
    pub fetched_at: i64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub etag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_modified: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not_modified: bool,
}

impl FetchedArtifact {
    pub fn new(
        uri: impl Into<String>,
        revision: impl Into<String>,
        content_type: impl Into<String>,
        bytes: Vec<u8>,
        fetched_at: i64,
    ) -> Self {
        let content_digest = blake3::hash(&bytes).to_hex().to_string();
        Self {
            uri: uri.into(),
            revision: revision.into(),
            content_type: content_type.into(),
            bytes,
            content_digest,
            fetched_at,
            etag: None,
            last_modified: None,
            not_modified: false,
        }
    }
}

/// Network access is injected so scanners and workers can be tested offline and
/// credentials never need to enter contract-fetching code.
pub trait ArtifactFetcher: Send + Sync {
    fn fetch(&self, request: &ArtifactFetchRequest) -> Result<FetchedArtifact, FetchArtifactError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemArtifactFetcher;

impl ArtifactFetcher for SystemArtifactFetcher {
    fn fetch(&self, request: &ArtifactFetchRequest) -> Result<FetchedArtifact, FetchArtifactError> {
        let response = synaptic_ingest::safe_fetch_response(
            &request.uri,
            request.max_bytes,
            request.prior_etag.as_deref(),
            request.prior_last_modified.as_deref(),
        )?;
        let content_type = response
            .content_type
            .unwrap_or_else(|| "application/octet-stream".into());
        if !response.not_modified {
            validate_content_type(&content_type, &request.accepted_content_types)?;
        }
        let revision = response
            .etag
            .clone()
            .or_else(|| response.last_modified.clone())
            .unwrap_or_else(|| blake3::hash(&response.bytes).to_hex().to_string());
        let mut artifact = FetchedArtifact::new(
            &request.uri,
            revision,
            content_type,
            response.bytes,
            unix_timestamp(),
        );
        artifact.etag = response.etag;
        artifact.last_modified = response.last_modified;
        artifact.not_modified = response.not_modified;
        Ok(artifact)
    }
}

pub(crate) fn validate_artifact(
    artifact: &FetchedArtifact,
    request: &ArtifactFetchRequest,
) -> Result<(), FetchArtifactError> {
    if artifact.not_modified {
        return Ok(());
    }
    if artifact.bytes.len() as u64 > request.max_bytes {
        return Err(FetchArtifactError::TooLarge {
            actual: artifact.bytes.len() as u64,
            maximum: request.max_bytes,
        });
    }
    if artifact.revision.trim().is_empty() {
        return Err(FetchArtifactError::MissingRevision(artifact.uri.clone()));
    }
    let actual_digest = blake3::hash(&artifact.bytes).to_hex().to_string();
    if actual_digest != artifact.content_digest {
        return Err(FetchArtifactError::Integrity(format!(
            "artifact digest does not match bytes for {}",
            artifact.uri
        )));
    }
    validate_content_type(&artifact.content_type, &request.accepted_content_types)
}

fn validate_content_type(
    content_type: &str,
    accepted: &[String],
) -> Result<(), FetchArtifactError> {
    let actual = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();
    if accepted.iter().any(|expected| {
        expected == "*/*"
            || actual == *expected
            || (expected.ends_with("/*") && actual.starts_with(&expected[..expected.len() - 1]))
    }) {
        Ok(())
    } else {
        Err(FetchArtifactError::ContentType {
            actual,
            accepted: accepted.to_vec(),
        })
    }
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Debug, thiserror::Error)]
pub enum FetchArtifactError {
    #[error("artifact unavailable: {0}")]
    Unavailable(String),
    #[error("artifact {0} has no stable revision, ETag, Last-Modified value, or digest")]
    MissingRevision(String),
    #[error("artifact is {actual} bytes; maximum is {maximum} bytes")]
    TooLarge { actual: u64, maximum: u64 },
    #[error("artifact content type {actual:?} is not one of {accepted:?}")]
    ContentType {
        actual: String,
        accepted: Vec<String>,
    },
    #[error("artifact integrity error: {0}")]
    Integrity(String),
    #[error("safe fetch failed: {0}")]
    Fetch(#[from] synaptic_ingest::FetchError),
}
