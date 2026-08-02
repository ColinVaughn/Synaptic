use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{
    diff_contracts, normalize_contract, ApiChangeEvent, ApiEventStore, ArtifactFetchRequest,
    ArtifactFetcher, FetchArtifactError, FetchedArtifact, SourceArtifact, SourceLockState,
    StoreError, SurfaceFormat, VendorRegistry, VendorSource, VersionRange,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanDisposition {
    BaselineStored,
    Unchanged,
    BreakingChange,
    ChangedNonBreaking,
    ReviewRequired,
    RateLimited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScannedSource {
    pub vendor: String,
    pub source: String,
    pub revision: String,
    pub content_digest: String,
    pub disposition: ScanDisposition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCandidate {
    pub vendor: String,
    pub source: String,
    pub revision: String,
    pub summary: String,
    pub content_digest: String,
    pub confidence_basis: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookArtifactEnvelope {
    pub schema: u32,
    pub vendor: String,
    pub revision: String,
    pub occurred_at: i64,
    pub content_type: String,
    pub content_digest: String,
    pub contract: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanReport {
    pub version: u32,
    pub sources: Vec<ScannedSource>,
    pub events: Vec<ApiChangeEvent>,
    pub review_candidates: Vec<ReviewCandidate>,
}

impl Default for ScanReport {
    fn default() -> Self {
        Self {
            version: 1,
            sources: Vec::new(),
            events: Vec::new(),
            review_candidates: Vec::new(),
        }
    }
}

pub fn scan_repository(
    repository_root: &Path,
    registry: &VendorRegistry,
    fetcher: &dyn ArtifactFetcher,
    offline: bool,
) -> Result<ScanReport, ScanError> {
    let root = repository_root.canonicalize()?;
    let store = ApiEventStore::new(&root);
    let mut report = ScanReport::default();

    for vendor in &registry.config().vendors {
        if !vendor.enabled {
            continue;
        }
        for source in &vendor.sources {
            if offline && !source.is_local() {
                return Err(ScanError::OfflineSource(source.location()));
            }
            let location = source.location();
            let prior = store.source_state(&vendor.id, &location)?;
            if !source.is_local()
                && prior.as_ref().is_some_and(|state| {
                    unix_timestamp().saturating_sub(state.checked_at)
                        < source.min_poll_interval_seconds() as i64
                })
            {
                let prior = prior.as_ref().expect("checked above");
                report.sources.push(scanned(
                    &vendor.id,
                    &location,
                    &prior.revision,
                    &prior.content_digest,
                    ScanDisposition::RateLimited,
                ));
                continue;
            }
            let request = request_for(source, prior.as_ref());
            let artifact = if source.is_local() {
                read_local_artifact(&root, &vendor.id, source, &request)?
            } else {
                fetcher.fetch(&request)?
            };
            if artifact.not_modified {
                let mut prior = prior.ok_or_else(|| {
                    ScanError::Integrity(format!(
                        "source {} returned not-modified without a cached state",
                        location
                    ))
                })?;
                prior.checked_at = unix_timestamp();
                prior.etag = artifact.etag.clone().or(prior.etag);
                prior.last_modified = artifact.last_modified.clone().or(prior.last_modified);
                store.record_source(prior.clone())?;
                report.sources.push(scanned(
                    &vendor.id,
                    &location,
                    &prior.revision,
                    &prior.content_digest,
                    ScanDisposition::Unchanged,
                ));
                continue;
            }
            crate::artifact::validate_artifact(&artifact, &request)?;
            if artifact.uri != location {
                return Err(ScanError::Integrity(format!(
                    "fetcher returned {} for requested source {}",
                    artifact.uri, location
                )));
            }
            if let Some(prior) = &prior {
                if prior.revision == artifact.revision
                    && prior.content_digest != artifact.content_digest
                {
                    return Err(ScanError::Integrity(format!(
                        "source {} changed payload under the same revision {}",
                        location, artifact.revision
                    )));
                }
            }
            store.put_artifact(&artifact.content_digest, &artifact.bytes)?;

            match source {
                VendorSource::Changelog { .. } => {
                    scan_changelog(&store, &vendor.id, &artifact, prior.as_ref(), &mut report)?
                }
                VendorSource::PackageRelease {
                    package,
                    affected_versions,
                    ..
                } => scan_package_release(
                    &store,
                    &vendor.id,
                    package.to_string(),
                    affected_versions,
                    &artifact,
                    prior.as_ref(),
                    &mut report,
                )?,
                VendorSource::OpenApi {
                    affected_versions, ..
                }
                | VendorSource::StaticContract {
                    affected_versions, ..
                }
                | VendorSource::Webhook {
                    affected_versions, ..
                } => scan_contract(
                    &store,
                    &vendor.id,
                    &artifact,
                    prior.as_ref(),
                    affected_versions,
                    if matches!(source, VendorSource::Webhook { .. }) {
                        "webhook"
                    } else {
                        "openapi"
                    },
                    &mut report,
                )?,
            }
        }
    }
    report.sources.sort_by(|a, b| {
        a.vendor
            .cmp(&b.vendor)
            .then_with(|| a.source.cmp(&b.source))
    });
    report.events.sort_by(|a, b| a.id.cmp(&b.id));
    report.review_candidates.sort_by(|a, b| {
        a.vendor
            .cmp(&b.vendor)
            .then_with(|| a.source.cmp(&b.source))
    });
    Ok(report)
}

fn request_for(source: &VendorSource, prior: Option<&SourceLockState>) -> ArtifactFetchRequest {
    let accepted_content_types = match source {
        VendorSource::Changelog { .. } => vec![
            "text/*".into(),
            "application/json".into(),
            "application/xml".into(),
            "application/atom+xml".into(),
            "application/rss+xml".into(),
        ],
        _ => vec![
            "application/json".into(),
            "application/yaml".into(),
            "application/x-yaml".into(),
            "text/yaml".into(),
            "text/plain".into(),
            "application/octet-stream".into(),
        ],
    };
    ArtifactFetchRequest {
        uri: source.location(),
        max_bytes: source.max_bytes(),
        accepted_content_types,
        prior_etag: prior.and_then(|state| state.etag.clone()),
        prior_last_modified: prior.and_then(|state| state.last_modified.clone()),
    }
}

fn read_local_artifact(
    root: &Path,
    vendor: &str,
    source: &VendorSource,
    request: &ArtifactFetchRequest,
) -> Result<FetchedArtifact, ScanError> {
    let path = match source {
        VendorSource::StaticContract { path, .. } | VendorSource::Webhook { path, .. } => path,
        _ => return Err(ScanError::Integrity("expected a local source".into())),
    };
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ScanError::LocalPath { path: path.clone() });
    }
    let resolved = root.join(path).canonicalize()?;
    if !resolved.starts_with(root) {
        return Err(ScanError::LocalPath { path: path.clone() });
    }
    let metadata = fs::metadata(&resolved)?;
    if !metadata.is_file() || metadata.len() > request.max_bytes {
        return Err(ScanError::LocalPath { path: path.clone() });
    }
    let bytes = fs::read(&resolved)?;
    if matches!(source, VendorSource::Webhook { .. }) {
        return read_webhook_artifact(vendor, request, &bytes);
    }
    let digest = blake3::hash(&bytes).to_hex().to_string();
    let content_type = match resolved
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("yaml" | "yml") => "application/yaml",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    };
    Ok(FetchedArtifact::new(
        request.uri.clone(),
        digest,
        content_type,
        bytes,
        0,
    ))
}

fn read_webhook_artifact(
    vendor: &str,
    request: &ArtifactFetchRequest,
    bytes: &[u8],
) -> Result<FetchedArtifact, ScanError> {
    let envelope: WebhookArtifactEnvelope = serde_json::from_slice(bytes)
        .map_err(|error| ScanError::InvalidWebhook(error.to_string()))?;
    if envelope.schema != 1 {
        return Err(ScanError::InvalidWebhook(format!(
            "unsupported webhook schema {}",
            envelope.schema
        )));
    }
    if !envelope.vendor.eq_ignore_ascii_case(vendor) {
        return Err(ScanError::InvalidWebhook(format!(
            "webhook vendor {:?} does not match configured vendor {vendor:?}",
            envelope.vendor
        )));
    }
    if envelope.revision.trim().is_empty() || envelope.occurred_at < 0 {
        return Err(ScanError::InvalidWebhook(
            "webhook revision and occurrence time are invalid".into(),
        ));
    }
    if !matches!(
        envelope.content_type.as_str(),
        "application/json" | "application/yaml" | "application/x-yaml" | "text/yaml"
    ) {
        return Err(ScanError::InvalidWebhook(format!(
            "unsupported webhook content type {:?}",
            envelope.content_type
        )));
    }
    let contract = serde_json::to_vec(&envelope.contract)
        .map_err(|error| ScanError::InvalidWebhook(error.to_string()))?;
    let digest = blake3::hash(&contract).to_hex().to_string();
    if digest != envelope.content_digest {
        return Err(ScanError::InvalidWebhook(
            "webhook contract digest does not match its envelope".into(),
        ));
    }
    Ok(FetchedArtifact::new(
        request.uri.clone(),
        envelope.revision,
        envelope.content_type,
        contract,
        envelope.occurred_at,
    ))
}

fn scan_contract(
    store: &ApiEventStore,
    vendor: &str,
    artifact: &FetchedArtifact,
    prior: Option<&SourceLockState>,
    affected_versions: &str,
    evidence_kind: &str,
    report: &mut ScanReport,
) -> Result<(), ScanError> {
    let contract = normalize_contract(vendor, &artifact.bytes)?;
    store.put_contract(&contract)?;
    let state = lock_state(vendor, artifact, Some(contract.digest.clone()));
    if contract.completeness == crate::ParseCompleteness::Partial {
        let summary = contract
            .losses
            .iter()
            .take(8)
            .map(|loss| format!("{}: {}", loss.pointer, loss.reason))
            .collect::<Vec<_>>()
            .join("; ");
        report.review_candidates.push(ReviewCandidate {
            vendor: vendor.into(),
            source: artifact.uri.clone(),
            revision: artifact.revision.clone(),
            summary,
            content_digest: artifact.content_digest.clone(),
            confidence_basis:
                "contract normalization is partial; unattended compatibility and repair are disabled"
                    .into(),
        });
        store.record_source(state)?;
        report.sources.push(scanned(
            vendor,
            &artifact.uri,
            &artifact.revision,
            &artifact.content_digest,
            ScanDisposition::ReviewRequired,
        ));
        return Ok(());
    }
    let disposition = match prior {
        None => ScanDisposition::BaselineStored,
        Some(previous) if previous.content_digest == artifact.content_digest => {
            ScanDisposition::Unchanged
        }
        Some(previous) => {
            let prior_digest = previous.contract_digest.as_deref().ok_or_else(|| {
                ScanError::Integrity(format!(
                    "prior contract state for {} has no normalized digest",
                    artifact.uri
                ))
            })?;
            let old = store.load_contract(vendor, prior_digest)?;
            let detected_kind = if evidence_kind == "webhook" {
                "webhook"
            } else {
                match contract.format {
                    SurfaceFormat::OpenApi => "openapi",
                    SurfaceFormat::AsyncApi => "asyncapi",
                    SurfaceFormat::GraphQl => "graphql",
                    SurfaceFormat::Protobuf => "protobuf",
                    SurfaceFormat::Wsdl => "wsdl",
                    SurfaceFormat::Smithy => "smithy",
                    SurfaceFormat::OpenRpc => "openrpc",
                }
            };
            let source = source_artifact(artifact, detected_kind);
            let versions = VersionRange::parse(affected_versions)?;
            let event = diff_contracts(&old, &contract, source, versions)?;
            if event.changes.is_empty() {
                ScanDisposition::ChangedNonBreaking
            } else {
                store.put_event(&event)?;
                report.events.push(event);
                ScanDisposition::BreakingChange
            }
        }
    };
    store.record_source(state)?;
    report.sources.push(scanned(
        vendor,
        &artifact.uri,
        &artifact.revision,
        &artifact.content_digest,
        disposition,
    ));
    Ok(())
}

fn scan_changelog(
    store: &ApiEventStore,
    vendor: &str,
    artifact: &FetchedArtifact,
    prior: Option<&SourceLockState>,
    report: &mut ScanReport,
) -> Result<(), ScanError> {
    let unchanged = prior.is_some_and(|state| state.content_digest == artifact.content_digest);
    let summary = sanitize_release_text(&String::from_utf8_lossy(&artifact.bytes));
    let has_candidate = !unchanged && looks_breaking(&summary);
    let disposition = if unchanged {
        ScanDisposition::Unchanged
    } else if has_candidate {
        report.review_candidates.push(ReviewCandidate {
            vendor: vendor.into(),
            source: artifact.uri.clone(),
            revision: artifact.revision.clone(),
            summary,
            content_digest: artifact.content_digest.clone(),
            confidence_basis: "official prose is uncorroborated and cannot trigger repair".into(),
        });
        ScanDisposition::ReviewRequired
    } else if prior.is_none() {
        ScanDisposition::BaselineStored
    } else {
        ScanDisposition::ChangedNonBreaking
    };
    store.record_source(lock_state(vendor, artifact, None))?;
    report.sources.push(scanned(
        vendor,
        &artifact.uri,
        &artifact.revision,
        &artifact.content_digest,
        disposition,
    ));
    Ok(())
}

fn scan_package_release(
    store: &ApiEventStore,
    vendor: &str,
    package: String,
    affected_versions: &str,
    artifact: &FetchedArtifact,
    prior: Option<&SourceLockState>,
    report: &mut ScanReport,
) -> Result<(), ScanError> {
    let unchanged = prior.is_some_and(|state| state.content_digest == artifact.content_digest);
    let adapter = crate::PackageReleaseAdapter::new(vendor, &package);
    if let Ok(surface) = adapter.normalize_surface(artifact) {
        let disposition = match prior {
            None => ScanDisposition::BaselineStored,
            Some(_) if unchanged => ScanDisposition::Unchanged,
            Some(previous) => {
                let old_bytes = store.load_artifact(&previous.content_digest)?;
                let old_artifact = FetchedArtifact::new(
                    &previous.source_uri,
                    &previous.revision,
                    "application/json",
                    old_bytes,
                    artifact.fetched_at,
                );
                let old = adapter.normalize_surface(&old_artifact)?;
                let event = adapter.diff_event(
                    &old,
                    &surface,
                    source_artifact(artifact, "package_release"),
                    VersionRange::parse(affected_versions)?,
                )?;
                if event.changes.is_empty() {
                    ScanDisposition::ChangedNonBreaking
                } else {
                    store.put_event(&event)?;
                    report.events.push(event);
                    ScanDisposition::BreakingChange
                }
            }
        };
        store.record_source(lock_state(vendor, artifact, Some(surface.digest)))?;
        report.sources.push(scanned(
            vendor,
            &artifact.uri,
            &artifact.revision,
            &artifact.content_digest,
            disposition,
        ));
        return Ok(());
    }
    let summary = sanitize_release_text(&String::from_utf8_lossy(&artifact.bytes));
    let disposition = if unchanged {
        ScanDisposition::Unchanged
    } else if looks_breaking(&summary) {
        report.review_candidates.push(ReviewCandidate {
            vendor: vendor.into(),
            source: artifact.uri.clone(),
            revision: artifact.revision.clone(),
            summary: format!("{package}: {summary}"),
            content_digest: artifact.content_digest.clone(),
            confidence_basis: "package release prose requires export-surface corroboration".into(),
        });
        ScanDisposition::ReviewRequired
    } else if prior.is_none() {
        ScanDisposition::BaselineStored
    } else {
        ScanDisposition::ChangedNonBreaking
    };
    store.record_source(lock_state(vendor, artifact, None))?;
    report.sources.push(scanned(
        vendor,
        &artifact.uri,
        &artifact.revision,
        &artifact.content_digest,
        disposition,
    ));
    Ok(())
}

fn source_artifact(artifact: &FetchedArtifact, evidence_kind: &str) -> SourceArtifact {
    SourceArtifact {
        uri: artifact.uri.clone(),
        revision: artifact.revision.clone(),
        etag: artifact.etag.clone(),
        last_modified: artifact.last_modified.clone(),
        content_digest: artifact.content_digest.clone(),
        fetched_at: artifact.fetched_at,
        adapter_version: 1,
        evidence_kind: evidence_kind.into(),
    }
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn lock_state(
    vendor: &str,
    artifact: &FetchedArtifact,
    contract_digest: Option<String>,
) -> SourceLockState {
    SourceLockState {
        vendor: vendor.into(),
        source_uri: artifact.uri.clone(),
        revision: artifact.revision.clone(),
        content_digest: artifact.content_digest.clone(),
        etag: artifact.etag.clone(),
        last_modified: artifact.last_modified.clone(),
        contract_digest,
        checked_at: unix_timestamp(),
    }
}

fn scanned(
    vendor: &str,
    source: &str,
    revision: &str,
    content_digest: &str,
    disposition: ScanDisposition,
) -> ScannedSource {
    ScannedSource {
        vendor: vendor.into(),
        source: source.into(),
        revision: revision.into(),
        content_digest: content_digest.into(),
        disposition,
    }
}

fn looks_breaking(text: &str) -> bool {
    let lowercase = text.to_ascii_lowercase();
    [
        "breaking",
        "removed",
        "no longer",
        "deprecated",
        "minimum supported",
    ]
    .iter()
    .any(|needle| lowercase.contains(needle))
}

/// Release prose is retained only as bounded data. Script blocks, markup,
/// control characters, chat sentinels, and command-shaped suffixes are removed.
pub fn sanitize_release_text(text: &str) -> String {
    let mut source = remove_block_case_insensitive(text, "script");
    source = remove_block_case_insensitive(&source, "style");
    let mut plain = String::with_capacity(source.len());
    let mut in_tag = false;
    for character in source.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag && (!character.is_control() || character == '\n') => plain.push(character),
            _ => {}
        }
    }
    let lowercase = plain.to_ascii_lowercase();
    let command_start = [
        "run curl",
        "curl ",
        "wget ",
        "powershell ",
        "cmd.exe",
        "<|im_start|>",
        "ignore previous",
    ]
    .iter()
    .filter_map(|needle| lowercase.find(needle))
    .min()
    .unwrap_or(plain.len());
    plain[..command_start]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(2_000)
        .collect()
}

fn remove_block_case_insensitive(text: &str, tag: &str) -> String {
    let mut result = text.to_string();
    loop {
        let lowercase = result.to_ascii_lowercase();
        let Some(start) = lowercase.find(&format!("<{tag}")) else {
            return result;
        };
        let Some(relative_end) = lowercase[start..].find(&format!("</{tag}>")) else {
            result.truncate(start);
            return result;
        };
        let end = start + relative_end + tag.len() + 3;
        result.replace_range(start..end, " ");
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("cannot fetch network source in offline mode: {0}")]
    OfflineSource(String),
    #[error("local contract path must remain inside the repository: {path:?}")]
    LocalPath { path: PathBuf },
    #[error("API source integrity error: {0}")]
    Integrity(String),
    #[error("invalid webhook artifact envelope: {0}")]
    InvalidWebhook(String),
    #[error("artifact fetch failed: {0}")]
    Fetch(#[from] FetchArtifactError),
    #[error("event store failed: {0}")]
    Store(#[from] StoreError),
    #[error("contract processing failed: {0}")]
    Contract(#[from] crate::ContractError),
    #[error("vendor adapter failed: {0}")]
    Adapter(#[from] crate::AdapterError),
    #[error("invalid affected version range: {0}")]
    Version(#[from] semver::Error),
    #[error("source I/O failed: {0}")]
    Io(#[from] std::io::Error),
}
