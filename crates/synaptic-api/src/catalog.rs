use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::PackageUrl;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageMetadata {
    pub purl: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_digest: Option<String>,
    pub provenance: Vec<String>,
}

impl PackageMetadata {
    pub fn validate(&self) -> Result<(), PackageMetadataError> {
        PackageUrl::parse(&self.purl).map_err(PackageMetadataError::InvalidMetadata)?;
        if self.provenance.is_empty()
            || self
                .provenance
                .iter()
                .any(|entry| entry.trim().is_empty() || entry.len() > 2_048)
        {
            return Err(PackageMetadataError::InvalidMetadata(
                "metadata provenance is missing or invalid".into(),
            ));
        }
        if let Some(digest) = &self.artifact_digest {
            if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(PackageMetadataError::InvalidMetadata(
                    "artifact digest must be 64 hexadecimal characters".into(),
                ));
            }
        }
        for location in [
            self.repository.as_deref(),
            self.homepage.as_deref(),
            self.release_source.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !location.starts_with("https://") || location.len() > 4_096 {
                return Err(PackageMetadataError::InvalidMetadata(format!(
                    "metadata URL must be bounded HTTPS: {location:?}"
                )));
            }
        }
        Ok(())
    }
}

/// Explicit package-registry metadata boundary. Implementations may use a network,
/// but callers choose and construct them; Synaptic never hides a network resolver.
pub trait PackageMetadataResolver {
    fn resolve(
        &self,
        package: &PackageUrl,
        version: Option<&str>,
    ) -> Result<Option<PackageMetadata>, PackageMetadataError>;
}

#[derive(Debug)]
pub struct CachedPackageMetadataResolver<R> {
    root: PathBuf,
    inner: R,
}

impl<R> CachedPackageMetadataResolver<R> {
    pub fn new(root: impl Into<PathBuf>, inner: R) -> Result<Self, PackageMetadataError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        if !root.is_dir() {
            return Err(PackageMetadataError::InvalidCacheRoot(root));
        }
        Ok(Self { root, inner })
    }

    pub fn new_for_tenant(
        root: impl Into<PathBuf>,
        tenant: &str,
        inner: R,
    ) -> Result<Self, PackageMetadataError> {
        if tenant.is_empty()
            || tenant.len() > 64
            || !tenant.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
        {
            return Err(PackageMetadataError::InvalidTenant(tenant.into()));
        }
        let digest = blake3::hash(tenant.as_bytes()).to_hex();
        Self::new(root.into().join(format!("tenant-{}", &digest[..24])), inner)
    }

    fn cache_path(&self, package: &PackageUrl, version: Option<&str>) -> PathBuf {
        let key = format!("{}\0{}", package, version.unwrap_or(""));
        self.root
            .join(format!("{}.json", blake3::hash(key.as_bytes()).to_hex()))
    }

    /// Revoke one poisoned or obsolete profile without clearing other tenants or
    /// package versions.
    pub fn revoke(
        &self,
        package: &PackageUrl,
        version: Option<&str>,
    ) -> Result<bool, PackageMetadataError> {
        let path = self.cache_path(package, version);
        match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    version: u32,
    purl: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    release: Option<String>,
    metadata: Option<PackageMetadata>,
    content_digest: String,
}

fn cache_entry_digest(
    version: u32,
    purl: &str,
    release: Option<&str>,
    metadata: Option<&PackageMetadata>,
) -> Result<String, serde_json::Error> {
    let canonical = serde_json::to_vec(&(version, purl, release, metadata))?;
    Ok(blake3::hash(&canonical).to_hex().to_string())
}

impl<R: PackageMetadataResolver> PackageMetadataResolver for CachedPackageMetadataResolver<R> {
    fn resolve(
        &self,
        package: &PackageUrl,
        version: Option<&str>,
    ) -> Result<Option<PackageMetadata>, PackageMetadataError> {
        let path = self.cache_path(package, version);
        if path.is_file() {
            let entry: CacheEntry = serde_json::from_slice(&fs::read(&path)?)?;
            if entry.version != 1
                || entry.purl != package.to_string()
                || entry.release.as_deref() != version
                || entry.content_digest
                    != cache_entry_digest(
                        entry.version,
                        &entry.purl,
                        entry.release.as_deref(),
                        entry.metadata.as_ref(),
                    )?
            {
                return Err(PackageMetadataError::PoisonedCache(path));
            }
            if let Some(metadata) = &entry.metadata {
                metadata.validate()?;
            }
            return Ok(entry.metadata);
        }
        let metadata = self.inner.resolve(package, version)?;
        if let Some(metadata) = &metadata {
            metadata.validate()?;
            if metadata.purl != package.to_string() || metadata.version.as_deref() != version {
                return Err(PackageMetadataError::InvalidMetadata(
                    "resolver returned metadata for a different package or version".into(),
                ));
            }
        }
        let mut entry = CacheEntry {
            version: 1,
            purl: package.to_string(),
            release: version.map(str::to_string),
            metadata: metadata.clone(),
            content_digest: String::new(),
        };
        entry.content_digest = cache_entry_digest(
            entry.version,
            &entry.purl,
            entry.release.as_deref(),
            entry.metadata.as_ref(),
        )?;
        let temporary = self.root.join(format!(
            ".{}.{}.tmp",
            path.file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("metadata"),
            std::process::id()
        ));
        fs::write(&temporary, serde_json::to_vec_pretty(&entry)?)?;
        match fs::rename(&temporary, &path) {
            Ok(()) => {}
            Err(_) if path.is_file() => {
                let _ = fs::remove_file(&temporary);
            }
            Err(error) => return Err(error.into()),
        }
        Ok(metadata)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PackageMetadataError {
    #[error("package metadata I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid package metadata JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid package metadata: {0}")]
    InvalidMetadata(String),
    #[error("package metadata cache root is invalid: {0}")]
    InvalidCacheRoot(PathBuf),
    #[error("package metadata tenant identifier is invalid: {0:?}")]
    InvalidTenant(String),
    #[error("package metadata cache entry failed identity validation: {0}")]
    PoisonedCache(PathBuf),
}
