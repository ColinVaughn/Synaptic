//! Fetching an OSV advisory corpus into a local cache.
//!
//! The scanner itself never reads the network. This module is the only place
//! that does, it is only reached when the caller asks for it, and what it
//! produces is an ordinary directory of OSV documents that
//! [`crate::LocalDirSource`] reads exactly as if a human had put it there.
//!
//! Bulk export is preferred over per-package queries because it costs one
//! request, works offline afterwards, and never tells anyone what this
//! repository depends on.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use synaptic_api::Ecosystem;

/// Public OSV bulk-export bucket.
pub const OSV_BULK_BASE: &str = "https://osv-vulnerabilities.storage.googleapis.com";

/// Refuse to auto-download an export larger than this. npm's export is ~218 MB;
/// silently pulling that on someone's first scan would be rude, and on a
/// metered connection worse than rude.
pub const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// A cache older than this is reported as stale.
pub const DEFAULT_STALE_AFTER_SECONDS: i64 = 7 * 24 * 60 * 60;

/// Name of the file naming the generation currently being served.
const CURRENT_POINTER: &str = "current";

/// Prefix marking a directory as a corpus generation.
const GENERATION_PREFIX: &str = "g-";

/// Prefix marking a generation that is still being unpacked.
///
/// A generation only takes its real name once every document is on disk, so a
/// sync interrupted halfway leaves nothing a reader can mistake for a corpus.
/// Without this the pointer-recovery path in [`CorpusCache::resolve`] would
/// hand back a half-written directory, and a partial corpus reporting no
/// findings reads exactly like a clean one.
const STAGING_PREFIX: &str = "incoming-";

/// How long a retired generation is kept before it can be collected. A scan
/// that resolved it is still reading it, and scans take seconds.
const GENERATION_GRACE_SECONDS: u64 = 10 * 60;

/// What a `HEAD` told us about a corpus before we commit to downloading it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CorpusHead {
    pub size_bytes: Option<u64>,
    pub last_modified: Option<String>,
}

/// Fetches corpus archives. Injectable so every decision this module makes is
/// testable without touching the network.
pub trait CorpusFetcher: Send + Sync {
    fn head(&self, url: &str) -> Result<CorpusHead, SyncError>;
    fn get(&self, url: &str) -> Result<Vec<u8>, SyncError>;
}

/// Provenance of a cached corpus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusMetadata {
    pub ecosystem: String,
    pub source_url: String,
    /// Unix seconds when this cache was written.
    pub fetched_at: i64,
    /// Upstream `Last-Modified`, when the server sent one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_modified: Option<String>,
    pub advisory_count: usize,
    /// The generation directory this sync published. Absent on metadata written
    /// by a release that predates generations.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub generation: Option<String>,
}

impl CorpusMetadata {
    /// Whether the cache is older than `max_age_seconds` relative to `now`.
    pub fn is_stale(&self, now: i64, max_age_seconds: i64) -> bool {
        now.saturating_sub(self.fetched_at) > max_age_seconds
    }
}

/// A directory of per-ecosystem advisory caches.
#[derive(Debug, Clone)]
pub struct CorpusCache {
    root: PathBuf,
}

impl CorpusCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The conventional user-level cache, shared across repositories because an
    /// advisory corpus is not repository-specific.
    pub fn user_default() -> Option<Self> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .filter(|value| !value.is_empty())?;
        Some(Self::new(
            PathBuf::from(home).join(".synaptic").join("advisories"),
        ))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The container holding one ecosystem's corpus generations.
    ///
    /// This is not the directory to read advisories from; see [`Self::resolve`].
    /// Loading this path directly would read every retained generation at once.
    pub fn ecosystem_dir(&self, ecosystem: Ecosystem) -> PathBuf {
        self.root.join(ecosystem.as_str())
    }

    /// Where documents fetched one at a time from the OSV API are kept.
    ///
    /// Deliberately outside any ecosystem's container, so a corpus loader never
    /// reads live documents as if they were a synced bulk export.
    pub fn live_dir(&self) -> PathBuf {
        self.root.join("_live")
    }

    /// The pointer naming the generation currently being served.
    fn pointer_path(&self, ecosystem: Ecosystem) -> PathBuf {
        self.ecosystem_dir(ecosystem).join(CURRENT_POINTER)
    }

    /// The directory a reader should load advisories from, if one exists.
    ///
    /// A corpus is published as an immutable generation directory, and the
    /// `current` pointer is swapped to it by a rename once it is complete.
    /// Resolving therefore never returns a directory that is mid-write, and a
    /// caller that holds the returned path keeps reading a complete corpus even
    /// while the next sync lands.
    ///
    /// A directory written by a release that predates generations holds its
    /// documents flat, with no pointer. That layout is returned as-is, so an
    /// upgrade does not silently invalidate a cache that cost a large download.
    pub fn resolve(&self, ecosystem: Ecosystem) -> Option<PathBuf> {
        let container = self.ecosystem_dir(ecosystem);
        if let Ok(name) = std::fs::read_to_string(self.pointer_path(ecosystem)) {
            let generation = container.join(name.trim());
            if generation.is_dir() {
                return Some(generation);
            }
        }
        // No usable pointer. Either this cache predates generations, or a
        // pointer was lost. Recovering the newest generation beats reporting no
        // corpus at all, which for npm would mean re-downloading 218 MB.
        let mut newest: Option<PathBuf> = None;
        let mut has_flat_documents = false;
        for entry in std::fs::read_dir(&container).ok()?.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with(GENERATION_PREFIX) && path.is_dir() {
                // Generation names are a fixed-width millisecond stamp, so the
                // greatest name is the most recent.
                if newest.as_ref().is_none_or(|current| path > *current) {
                    newest = Some(path);
                }
            } else if path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
            {
                has_flat_documents = true;
            }
        }
        // An empty container means nothing was ever synced. Reporting it as a
        // corpus would let "we read nothing" read as "we found nothing".
        newest.or(has_flat_documents.then_some(container))
    }

    /// Remove generations that are neither current nor recent.
    ///
    /// The grace period exists because a scan that resolved a generation is
    /// still reading it. On Windows an open handle makes removal fail outright,
    /// which is tolerated: a generation that outlives its welcome costs disk,
    /// while one deleted underneath a reader costs a corrupted scan.
    fn collect_garbage(&self, ecosystem: Ecosystem, keep: &str, now: SystemTime) {
        let container = self.ecosystem_dir(ecosystem);
        let Ok(entries) = std::fs::read_dir(&container) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            // Staging directories are collected too: an interrupted sync leaves
            // one behind, and nothing ever renames it into place afterwards.
            let ours = name.starts_with(GENERATION_PREFIX) || name.starts_with(STAGING_PREFIX);
            if !ours || name == keep || !path.is_dir() {
                continue;
            }
            let recent = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_none_or(|age| age.as_secs() < GENERATION_GRACE_SECONDS);
            if recent {
                continue;
            }
            let _ = std::fs::remove_dir_all(&path);
        }
    }

    /// Provenance lives beside the corpus directory, never inside it. Anything
    /// inside is loaded as an advisory, so bookkeeping kept there would be
    /// reported as an unparseable document on every scan.
    fn metadata_path(&self, ecosystem: Ecosystem) -> PathBuf {
        self.root
            .join(format!("{}.corpus.json", ecosystem.as_str()))
    }

    /// Provenance of a cached ecosystem, when one has been fetched.
    pub fn metadata(&self, ecosystem: Ecosystem) -> Option<CorpusMetadata> {
        let body = std::fs::read_to_string(self.metadata_path(ecosystem)).ok()?;
        serde_json::from_str(&body).ok()
    }

    /// True when the ecosystem has no cache at all, or the cache is old.
    pub fn needs_sync(&self, ecosystem: Ecosystem, now: i64, max_age_seconds: i64) -> bool {
        self.metadata(ecosystem)
            .map(|metadata| metadata.is_stale(now, max_age_seconds))
            .unwrap_or(true)
    }
}

/// The OSV bulk-export URL for an ecosystem, if OSV publishes one.
///
/// OSV's ecosystem identifiers are not Synaptic's, so this is an explicit map
/// rather than a string transform. Ecosystems that are not confidently known
/// return `None` instead of a guessed URL that would 404 at scan time.
pub fn osv_bulk_url(ecosystem: Ecosystem) -> Option<String> {
    let osv_name = match ecosystem {
        Ecosystem::Cargo => "crates.io",
        Ecosystem::Npm => "npm",
        Ecosystem::Pypi => "PyPI",
        Ecosystem::Go => "Go",
        Ecosystem::Maven => "Maven",
        Ecosystem::Nuget => "NuGet",
        Ecosystem::Composer => "Packagist",
        Ecosystem::Gem => "RubyGems",
        Ecosystem::Hex => "Hex",
        Ecosystem::Pub => "Pub",
        _ => return None,
    };
    Some(format!("{OSV_BULK_BASE}/{osv_name}/all.zip"))
}

/// Download and unpack one ecosystem's corpus into the cache.
///
/// The size is checked before the body is requested, so an oversized export
/// costs one `HEAD` rather than a surprise download.
pub fn sync_ecosystem(
    cache: &CorpusCache,
    fetcher: &dyn CorpusFetcher,
    ecosystem: Ecosystem,
    max_bytes: u64,
) -> Result<CorpusMetadata, SyncError> {
    let url = osv_bulk_url(ecosystem).ok_or(SyncError::UnsupportedEcosystem(ecosystem))?;
    let head = fetcher.head(&url)?;
    if let Some(size) = head.size_bytes {
        if size > max_bytes {
            return Err(SyncError::TooLarge {
                ecosystem,
                size,
                limit: max_bytes,
            });
        }
    }

    let archive = fetcher.get(&url)?;
    let container = cache.ecosystem_dir(ecosystem);
    create_dir(&container)?;

    // Publish into a brand-new generation rather than over the live one. A
    // resync that drops a withdrawn advisory really drops it, because readers
    // move to the new generation wholesale; an interrupted sync leaves the old
    // generation serving; and a scan running right now sees neither a missing
    // directory nor a half-written one.
    let (generation, staging) = claim_generation(&container)?;
    let advisory_count = unpack_corpus(&archive, &staging)?;
    seal_generation(&container, &staging, &generation)?;
    publish_generation(cache, ecosystem, &generation)?;
    cache.collect_garbage(ecosystem, &generation, SystemTime::now());

    let metadata = CorpusMetadata {
        ecosystem: ecosystem.as_str().to_string(),
        source_url: url,
        fetched_at: unix_now(),
        last_modified: head.last_modified,
        advisory_count,
        generation: Some(generation),
    };
    let encoded = serde_json::to_string_pretty(&metadata)?;
    let metadata_path = cache.metadata_path(ecosystem);
    if let Some(parent) = metadata_path.parent() {
        create_dir(parent)?;
    }
    std::fs::write(&metadata_path, encoded).map_err(|source| SyncError::Io {
        path: metadata_path,
        source,
    })?;
    Ok(metadata)
}

/// Unpack an OSV `all.zip` into `directory`, returning how many documents landed.
///
/// Entries are written by file name only. OSV's exports are flat, and taking
/// the basename means a crafted archive cannot write outside the cache.
pub fn unpack_corpus(archive: &[u8], directory: &Path) -> Result<usize, SyncError> {
    create_dir(directory)?;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))
        .map_err(|error| SyncError::Archive(error.to_string()))?;

    let mut written = 0;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|error| SyncError::Archive(error.to_string()))?;
        if entry.is_dir() {
            continue;
        }
        let Some(name) = entry
            .name()
            .rsplit(['/', '\\'])
            .next()
            .map(str::to_string)
            .filter(|name| name.to_ascii_lowercase().ends_with(".json"))
        else {
            continue;
        };
        if name.is_empty() || name.starts_with('.') {
            continue;
        }

        let mut body = String::new();
        if entry.read_to_string(&mut body).is_err() {
            // A single unreadable member should not abandon the whole corpus.
            continue;
        }
        let path = directory.join(&name);
        std::fs::write(&path, body).map_err(|source| SyncError::Io {
            path: path.clone(),
            source,
        })?;
        written += 1;
    }
    Ok(written)
}

fn create_dir(path: &Path) -> Result<(), SyncError> {
    std::fs::create_dir_all(path).map_err(|source| SyncError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Claim a generation name and an empty staging directory to unpack into.
///
/// Returns the name the generation will take once it is complete, and the
/// directory to write it in meanwhile. `create_dir` fails rather than
/// succeeding when the directory already exists, which makes claiming a name
/// atomic against another process syncing the same ecosystem at the same
/// moment.
fn claim_generation(container: &Path) -> Result<(String, PathBuf), SyncError> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    for attempt in 0..1_000 {
        let suffix = if attempt == 0 {
            stamp.to_string()
        } else {
            format!("{stamp}-{attempt}")
        };
        // Both names have to be free, or a completed sync could not take the
        // generation name it staged for.
        if container
            .join(format!("{GENERATION_PREFIX}{suffix}"))
            .exists()
        {
            continue;
        }
        let path = container.join(format!("{STAGING_PREFIX}{suffix}"));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok((format!("{GENERATION_PREFIX}{suffix}"), path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(SyncError::Io { path, source }),
        }
    }
    Err(SyncError::Io {
        path: container.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not claim a corpus generation name",
        ),
    })
}

/// Give a fully unpacked staging directory its generation name.
///
/// A rename, so the generation appears complete or not at all. Only after this
/// can [`CorpusCache::resolve`] find it, pointer or no pointer.
fn seal_generation(container: &Path, staging: &Path, generation: &str) -> Result<(), SyncError> {
    let published = container.join(generation);
    std::fs::rename(staging, &published).map_err(|source| SyncError::Io {
        path: published,
        source,
    })
}

/// Point readers at a finished generation.
///
/// The pointer is written beside its target and then renamed over the live one.
/// Renaming onto an existing file replaces it in one step on both Unix and
/// Windows, so a reader sees either the old generation or the new one, never
/// neither.
fn publish_generation(
    cache: &CorpusCache,
    ecosystem: Ecosystem,
    generation: &str,
) -> Result<(), SyncError> {
    let pointer = cache.pointer_path(ecosystem);
    let incoming = pointer.with_extension("incoming");
    std::fs::write(&incoming, generation).map_err(|source| SyncError::Io {
        path: incoming.clone(),
        source,
    })?;
    std::fs::rename(&incoming, &pointer).map_err(|source| SyncError::Io {
        path: pointer,
        source,
    })
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("OSV publishes no bulk export for the {0} ecosystem")]
    UnsupportedEcosystem(Ecosystem),
    #[error(
        "the {ecosystem} corpus is {size} bytes, over the {limit} byte auto-download limit; \
         raise the limit to fetch it anyway, or supply a corpus directory explicitly"
    )]
    TooLarge {
        ecosystem: Ecosystem,
        size: u64,
        limit: u64,
    },
    #[error("cannot reach {url}: {message}")]
    Transport { url: String, message: String },
    #[error("the corpus archive could not be read: {0}")]
    Archive(String),
    #[error("cannot write the corpus cache at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot record corpus provenance: {0}")]
    Metadata(#[from] serde_json::Error),
}

/// The real fetcher, using the same blocking client style as `synaptic-upgrade`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemCorpusFetcher;

impl SystemCorpusFetcher {
    fn client() -> Result<reqwest::blocking::Client, SyncError> {
        reqwest::blocking::Client::builder()
            .user_agent(concat!("synaptic-vuln/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|error| SyncError::Transport {
                url: OSV_BULK_BASE.into(),
                message: error.to_string(),
            })
    }
}

impl CorpusFetcher for SystemCorpusFetcher {
    fn head(&self, url: &str) -> Result<CorpusHead, SyncError> {
        let response = Self::client()?
            .head(url)
            .send()
            .map_err(|error| SyncError::Transport {
                url: url.into(),
                message: error.to_string(),
            })?;
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        Ok(CorpusHead {
            size_bytes: header("content-length").and_then(|value| value.parse().ok()),
            last_modified: header("last-modified"),
        })
    }

    fn get(&self, url: &str) -> Result<Vec<u8>, SyncError> {
        let response = Self::client()?
            .get(url)
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|error| SyncError::Transport {
                url: url.into(),
                message: error.to_string(),
            })?;
        response
            .bytes()
            .map(|body| body.to_vec())
            .map_err(|error| SyncError::Transport {
                url: url.into(),
                message: error.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build an in-memory `all.zip` so extraction is tested for real.
    fn zip_with(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut buffer = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
            let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            for (name, body) in entries {
                writer.start_file(*name, options).unwrap();
                writer.write_all(body.as_bytes()).unwrap();
            }
            writer.finish().unwrap();
        }
        buffer
    }

    fn advisory(id: &str) -> String {
        format!(
            r#"{{"id":"{id}","affected":[{{"package":{{"ecosystem":"crates.io","name":"x"}}}}]}}"#
        )
    }

    struct FakeFetcher {
        head: CorpusHead,
        body: Vec<u8>,
    }

    impl CorpusFetcher for FakeFetcher {
        fn head(&self, _url: &str) -> Result<CorpusHead, SyncError> {
            Ok(self.head.clone())
        }

        fn get(&self, _url: &str) -> Result<Vec<u8>, SyncError> {
            Ok(self.body.clone())
        }
    }

    #[test]
    fn maps_cargo_to_the_osv_crates_io_export() {
        assert_eq!(
            osv_bulk_url(Ecosystem::Cargo).as_deref(),
            Some("https://osv-vulnerabilities.storage.googleapis.com/crates.io/all.zip")
        );
    }

    #[test]
    fn uses_osv_ecosystem_names_not_synaptic_ones() {
        assert!(osv_bulk_url(Ecosystem::Pypi).unwrap().contains("/PyPI/"));
        assert!(osv_bulk_url(Ecosystem::Composer)
            .unwrap()
            .contains("/Packagist/"));
        assert!(osv_bulk_url(Ecosystem::Gem).unwrap().contains("/RubyGems/"));
    }

    #[test]
    fn an_ecosystem_without_a_known_export_has_no_guessed_url() {
        assert_eq!(osv_bulk_url(Ecosystem::Generic), None);
        assert_eq!(osv_bulk_url(Ecosystem::Codeql), None);
    }

    #[test]
    fn unpacks_advisory_documents_into_the_cache_directory() {
        let dir = tempfile::tempdir().unwrap();
        let archive = zip_with(&[
            ("RUSTSEC-2026-0001.json", &advisory("RUSTSEC-2026-0001")),
            ("RUSTSEC-2026-0002.json", &advisory("RUSTSEC-2026-0002")),
        ]);

        let count = unpack_corpus(&archive, dir.path()).unwrap();

        assert_eq!(count, 2);
        assert!(dir.path().join("RUSTSEC-2026-0001.json").exists());
        assert!(dir.path().join("RUSTSEC-2026-0002.json").exists());
    }

    #[test]
    fn ignores_archive_entries_that_are_not_advisories() {
        let dir = tempfile::tempdir().unwrap();
        let archive = zip_with(&[
            ("README.md", "not an advisory"),
            ("RUSTSEC-2026-0001.json", &advisory("RUSTSEC-2026-0001")),
        ]);

        let count = unpack_corpus(&archive, dir.path()).unwrap();

        assert_eq!(count, 1);
        assert!(!dir.path().join("README.md").exists());
    }

    #[test]
    fn an_archive_entry_cannot_escape_the_cache_directory() {
        let dir = tempfile::tempdir().unwrap();
        let archive = zip_with(&[("../../escaped.json", &advisory("EVIL"))]);

        let count = unpack_corpus(&archive, dir.path()).unwrap();

        assert_eq!(count, 1, "the entry is kept, but flattened");
        assert!(
            dir.path().join("escaped.json").exists(),
            "written by basename inside the cache"
        );
        assert!(
            !dir.path().parent().unwrap().join("escaped.json").exists(),
            "zip-slip must not write outside the cache directory"
        );
    }

    #[test]
    fn syncing_writes_the_corpus_and_its_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CorpusCache::new(dir.path());
        let fetcher = FakeFetcher {
            head: CorpusHead {
                size_bytes: Some(1024),
                last_modified: Some("Thu, 06 Aug 2026 09:18:44 GMT".into()),
            },
            body: zip_with(&[("RUSTSEC-2026-0001.json", &advisory("RUSTSEC-2026-0001"))]),
        };

        let metadata = sync_ecosystem(
            &cache,
            &fetcher,
            Ecosystem::Cargo,
            DEFAULT_MAX_DOWNLOAD_BYTES,
        )
        .unwrap();

        assert_eq!(metadata.advisory_count, 1);
        assert_eq!(metadata.ecosystem, "cargo");
        assert!(metadata.source_url.contains("crates.io"));
        assert_eq!(
            metadata.last_modified.as_deref(),
            Some("Thu, 06 Aug 2026 09:18:44 GMT")
        );
        assert_eq!(cache.metadata(Ecosystem::Cargo), Some(metadata));
    }

    #[test]
    fn a_synced_corpus_contains_nothing_the_loader_rejects() {
        // Provenance metadata must not live inside the corpus directory. If it
        // does, the loader parses it as an advisory, fails, and every scan
        // warns that coverage is incomplete when it is not. A security tool
        // that cries wolf about its own bookkeeping gets ignored.
        let dir = tempfile::tempdir().unwrap();
        let cache = CorpusCache::new(dir.path());
        let fetcher = FakeFetcher {
            head: CorpusHead::default(),
            body: zip_with(&[
                ("RUSTSEC-2026-0001.json", &advisory("RUSTSEC-2026-0001")),
                ("RUSTSEC-2026-0002.json", &advisory("RUSTSEC-2026-0002")),
            ]),
        };
        sync_ecosystem(
            &cache,
            &fetcher,
            Ecosystem::Cargo,
            DEFAULT_MAX_DOWNLOAD_BYTES,
        )
        .unwrap();

        let live = cache.resolve(Ecosystem::Cargo).unwrap();
        let loaded = crate::LocalDirSource::load(&live).unwrap();
        let described = crate::AdvisorySource::describe(&loaded);

        assert_eq!(described.unreadable_documents, 0);
        assert_eq!(described.advisory_count, 2);
    }

    #[test]
    fn syncing_refuses_an_export_over_the_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CorpusCache::new(dir.path());
        let fetcher = FakeFetcher {
            head: CorpusHead {
                // npm's real export is about this big.
                size_bytes: Some(218_085_113),
                last_modified: None,
            },
            body: Vec::new(),
        };

        let error = sync_ecosystem(&cache, &fetcher, Ecosystem::Npm, DEFAULT_MAX_DOWNLOAD_BYTES)
            .unwrap_err();

        assert!(
            matches!(error, SyncError::TooLarge { size, .. } if size == 218_085_113),
            "got {error:?}"
        );
    }

    #[test]
    fn syncing_an_unsupported_ecosystem_is_refused_before_any_request() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CorpusCache::new(dir.path());
        let fetcher = FakeFetcher {
            head: CorpusHead::default(),
            body: Vec::new(),
        };

        let error = sync_ecosystem(
            &cache,
            &fetcher,
            Ecosystem::Generic,
            DEFAULT_MAX_DOWNLOAD_BYTES,
        )
        .unwrap_err();

        assert!(matches!(error, SyncError::UnsupportedEcosystem(_)));
    }

    #[test]
    fn a_resync_replaces_documents_that_upstream_withdrew() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CorpusCache::new(dir.path());
        let first = FakeFetcher {
            head: CorpusHead::default(),
            body: zip_with(&[
                ("A.json", &advisory("A")),
                ("REMOVED.json", &advisory("REMOVED")),
            ]),
        };
        sync_ecosystem(&cache, &first, Ecosystem::Cargo, DEFAULT_MAX_DOWNLOAD_BYTES).unwrap();

        let second = FakeFetcher {
            head: CorpusHead::default(),
            body: zip_with(&[("A.json", &advisory("A"))]),
        };
        let metadata = sync_ecosystem(
            &cache,
            &second,
            Ecosystem::Cargo,
            DEFAULT_MAX_DOWNLOAD_BYTES,
        )
        .unwrap();

        assert_eq!(metadata.advisory_count, 1);
        let live = cache.resolve(Ecosystem::Cargo).unwrap();
        assert!(
            !live.join("REMOVED.json").exists(),
            "a stale document must not survive a resync"
        );
        assert!(live.join("A.json").exists(), "the live corpus is complete");
    }

    #[test]
    fn a_reader_holding_a_generation_still_sees_all_of_it_after_a_resync() {
        // The gap this closes. The previous implementation removed the live
        // corpus directory and then renamed staging into its place, so a scan
        // running during a sync saw a missing or half-populated corpus and
        // reported documents as unreadable. A published generation is now
        // immutable, so a reader that resolved one is unaffected by the next.
        let dir = tempfile::tempdir().unwrap();
        let cache = CorpusCache::new(dir.path());
        let first = FakeFetcher {
            head: CorpusHead::default(),
            body: zip_with(&[("A.json", &advisory("A")), ("B.json", &advisory("B"))]),
        };
        sync_ecosystem(&cache, &first, Ecosystem::Cargo, DEFAULT_MAX_DOWNLOAD_BYTES).unwrap();

        // A scan resolves the corpus once and reads from that path, as it does.
        let held = cache.resolve(Ecosystem::Cargo).unwrap();

        let second = FakeFetcher {
            head: CorpusHead::default(),
            body: zip_with(&[("C.json", &advisory("C"))]),
        };
        sync_ecosystem(
            &cache,
            &second,
            Ecosystem::Cargo,
            DEFAULT_MAX_DOWNLOAD_BYTES,
        )
        .unwrap();

        let held_source = crate::LocalDirSource::load(&held).unwrap();
        let described = crate::AdvisorySource::describe(&held_source);
        assert_eq!(described.advisory_count, 2, "the held generation is intact");
        assert_eq!(
            described.unreadable_documents, 0,
            "no document became unreadable underneath the reader"
        );

        // A reader that resolves afresh gets the new corpus.
        let fresh = cache.resolve(Ecosystem::Cargo).unwrap();
        assert_ne!(fresh, held, "the pointer moved to a new generation");
        let fresh_source = crate::LocalDirSource::load(&fresh).unwrap();
        assert_eq!(
            crate::AdvisorySource::describe(&fresh_source).advisory_count,
            1
        );
    }

    #[test]
    fn a_corpus_written_by_an_older_release_is_read_without_a_resync() {
        // Upgrading must not silently invalidate a cache that took a 218 MB
        // download to populate.
        let dir = tempfile::tempdir().unwrap();
        let cache = CorpusCache::new(dir.path());
        let legacy = cache.ecosystem_dir(Ecosystem::Cargo);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("A.json"), advisory("A")).unwrap();

        let resolved = cache.resolve(Ecosystem::Cargo).unwrap();

        assert_eq!(resolved, legacy, "a flat corpus is its own generation");
        let source = crate::LocalDirSource::load(&resolved).unwrap();
        assert_eq!(crate::AdvisorySource::describe(&source).advisory_count, 1);
    }

    #[test]
    fn a_lost_pointer_recovers_the_newest_generation() {
        // Losing the pointer must not cost a re-download. For npm that is
        // 218 MB, which is enough to make an operator turn the scan off.
        let dir = tempfile::tempdir().unwrap();
        let cache = CorpusCache::new(dir.path());
        let fetcher = FakeFetcher {
            head: CorpusHead::default(),
            body: zip_with(&[("A.json", &advisory("A"))]),
        };
        sync_ecosystem(
            &cache,
            &fetcher,
            Ecosystem::Cargo,
            DEFAULT_MAX_DOWNLOAD_BYTES,
        )
        .unwrap();
        let published = cache.resolve(Ecosystem::Cargo).unwrap();

        std::fs::remove_file(cache.ecosystem_dir(Ecosystem::Cargo).join("current")).unwrap();

        assert_eq!(
            cache.resolve(Ecosystem::Cargo).as_deref(),
            Some(published.as_path())
        );
    }

    #[test]
    fn a_sync_that_fails_while_unpacking_leaves_nothing_that_reads_as_a_corpus() {
        // The first sync for an ecosystem writes no pointer, so recovery falls
        // back to "the newest generation on disk". A generation that was still
        // being unpacked when the sync failed must not be that generation: a
        // partial corpus reporting no findings reads exactly like a clean one.
        let dir = tempfile::tempdir().unwrap();
        let cache = CorpusCache::new(dir.path());
        let corrupt = FakeFetcher {
            head: CorpusHead::default(),
            body: b"not a zip archive".to_vec(),
        };

        let failed = sync_ecosystem(
            &cache,
            &corrupt,
            Ecosystem::Cargo,
            DEFAULT_MAX_DOWNLOAD_BYTES,
        );

        assert!(failed.is_err());
        assert_eq!(
            cache.resolve(Ecosystem::Cargo),
            None,
            "an unfinished generation is not a corpus"
        );
    }

    #[test]
    fn resolving_an_ecosystem_that_was_never_synced_finds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CorpusCache::new(dir.path());

        assert_eq!(cache.resolve(Ecosystem::Cargo), None);
    }

    #[test]
    fn a_failed_sync_leaves_the_live_corpus_serving() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CorpusCache::new(dir.path());
        let good = FakeFetcher {
            head: CorpusHead::default(),
            body: zip_with(&[("A.json", &advisory("A"))]),
        };
        sync_ecosystem(&cache, &good, Ecosystem::Cargo, DEFAULT_MAX_DOWNLOAD_BYTES).unwrap();
        let before = cache.resolve(Ecosystem::Cargo).unwrap();

        struct FailingFetcher;
        impl CorpusFetcher for FailingFetcher {
            fn head(&self, _url: &str) -> Result<CorpusHead, SyncError> {
                Ok(CorpusHead::default())
            }
            fn get(&self, url: &str) -> Result<Vec<u8>, SyncError> {
                Err(SyncError::Transport {
                    url: url.into(),
                    message: "connection reset".into(),
                })
            }
        }
        let failed = sync_ecosystem(
            &cache,
            &FailingFetcher,
            Ecosystem::Cargo,
            DEFAULT_MAX_DOWNLOAD_BYTES,
        );

        assert!(failed.is_err());
        assert_eq!(
            cache.resolve(Ecosystem::Cargo).as_deref(),
            Some(before.as_path()),
            "a failed sync must not retire the corpus that still works"
        );
        let source = crate::LocalDirSource::load(&before).unwrap();
        assert_eq!(crate::AdvisorySource::describe(&source).advisory_count, 1);
    }

    #[test]
    fn the_generation_is_recorded_in_the_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CorpusCache::new(dir.path());
        let fetcher = FakeFetcher {
            head: CorpusHead::default(),
            body: zip_with(&[("A.json", &advisory("A"))]),
        };

        let metadata = sync_ecosystem(
            &cache,
            &fetcher,
            Ecosystem::Cargo,
            DEFAULT_MAX_DOWNLOAD_BYTES,
        )
        .unwrap();

        let generation = metadata.generation.expect("a sync records its generation");
        assert!(
            cache
                .resolve(Ecosystem::Cargo)
                .unwrap()
                .ends_with(&generation),
            "the recorded generation is the one being served"
        );
    }

    #[test]
    fn an_absent_cache_needs_syncing() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CorpusCache::new(dir.path());

        assert!(cache.needs_sync(Ecosystem::Cargo, 1_000, DEFAULT_STALE_AFTER_SECONDS));
        assert_eq!(cache.metadata(Ecosystem::Cargo), None);
    }

    #[test]
    fn a_fresh_cache_does_not_need_syncing_but_an_old_one_does() {
        let metadata = CorpusMetadata {
            ecosystem: "cargo".into(),
            source_url: "u".into(),
            fetched_at: 1_000_000,
            last_modified: None,
            advisory_count: 1,
            generation: None,
        };

        assert!(!metadata.is_stale(1_000_000 + 60, DEFAULT_STALE_AFTER_SECONDS));
        assert!(metadata.is_stale(
            1_000_000 + DEFAULT_STALE_AFTER_SECONDS + 1,
            DEFAULT_STALE_AFTER_SECONDS
        ));
    }
}
