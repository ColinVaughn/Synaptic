//! Querying OSV for individual packages.
//!
//! The bulk export remains the default and the better choice for a whole
//! repository: one request, offline afterwards, and it tells nobody what this
//! repository depends on. This module exists for the cases the export cannot
//! serve, chiefly an ecosystem whose export is too large to be worth
//! downloading for the handful of packages a repository actually uses.
//!
//! Nothing here runs unless a caller asks for it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use synaptic_api::PackageCoordinate;

use crate::advisory::Advisory;
use crate::source::{LocalDirSource, SourceError};

/// Public OSV API root.
pub const OSV_API_BASE: &str = "https://api.osv.dev/v1";

/// OSV caps a `querybatch` at 1000 queries.
pub const OSV_BATCH_LIMIT: usize = 1000;

/// How long a request is given before the caller is told the network did not
/// answer. Short on purpose: a scan that hangs is worse than one that says it
/// could not reach OSV.
pub const OSV_TIMEOUT_SECONDS: u64 = 5;

/// How many advisory documents are fetched at once.
///
/// A batch query over a real repository names a few hundred advisories, and
/// fetching those one after another takes minutes, which reads as a hang. Kept
/// deliberately small: OSV is a free public service, and this is a burst of
/// requests nobody asked them for.
pub const OSV_FETCH_CONCURRENCY: usize = 6;

/// Carries requests to OSV. Injectable so every decision here is testable
/// without a network.
///
/// `Sync` because documents are fetched from several threads at once.
pub trait OsvTransport: Sync {
    fn post_json(&self, url: &str, body: &str) -> Result<String, SourceError>;
    fn get_json(&self, url: &str) -> Result<String, SourceError>;
}

/// OSV's name for an ecosystem, when it publishes one.
///
/// An ecosystem that is not confidently known returns `None` rather than a
/// guessed string, which would silently query for a package OSV has never heard
/// of and return an empty result that reads like a clean bill of health.
pub fn osv_ecosystem_name(ecosystem: synaptic_api::Ecosystem) -> Option<&'static str> {
    use synaptic_api::Ecosystem;
    Some(match ecosystem {
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
    })
}

/// One advisory the batch query named, with the version of it OSV holds.
struct Named {
    id: String,
    modified: String,
}

/// Fetch every advisory OSV holds for these packages.
///
/// `cache` is a directory of advisory documents keyed by id. The batch query
/// always runs, so a newly published advisory is always seen; the cache only
/// avoids re-downloading a document whose `modified` stamp has not changed.
///
/// A transport failure is an error, never an empty result. "We could not ask"
/// and "there is nothing" must not produce the same report.
pub fn fetch_advisories(
    transport: &dyn OsvTransport,
    coordinates: &[PackageCoordinate],
    cache: Option<&Path>,
) -> Result<LocalDirSource, SourceError> {
    // Deduplicate and drop ecosystems OSV cannot answer for.
    let queryable: Vec<&PackageCoordinate> = coordinates
        .iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|coordinate| osv_ecosystem_name(coordinate.ecosystem).is_some())
        .collect();

    let mut named: BTreeMap<String, Named> = BTreeMap::new();
    for chunk in queryable.chunks(OSV_BATCH_LIMIT.max(1)) {
        let body = batch_query_body(chunk);
        let response = transport.post_json(&format!("{OSV_API_BASE}/querybatch"), &body)?;
        collect_named(&response, &mut named);
    }

    // Serve what the cache already holds, and fetch the rest concurrently.
    let entries: Vec<&Named> = named.values().collect();
    let mut bodies: BTreeMap<&str, String> = BTreeMap::new();
    let mut outstanding: Vec<&Named> = Vec::new();
    for entry in &entries {
        match cached(cache, entry) {
            Some(body) => {
                bodies.insert(entry.id.as_str(), body);
            }
            None => outstanding.push(entry),
        }
    }

    if !outstanding.is_empty() {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let fetched: std::sync::Mutex<Vec<(&str, String)>> = std::sync::Mutex::new(Vec::new());
        let failure: std::sync::Mutex<Option<SourceError>> = std::sync::Mutex::new(None);
        let workers = OSV_FETCH_CONCURRENCY.min(outstanding.len()).max(1);

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(entry) = outstanding.get(index) else {
                            return;
                        };
                        // Another worker already failed; stop rather than pile more
                        // requests onto a service that is not answering.
                        if failure.lock().is_ok_and(|slot| slot.is_some()) {
                            return;
                        }
                        match transport.get_json(&format!("{OSV_API_BASE}/vulns/{}", entry.id)) {
                            Ok(body) => {
                                store(cache, entry, &body);
                                if let Ok(mut slot) = fetched.lock() {
                                    slot.push((entry.id.as_str(), body));
                                }
                            }
                            Err(error) => {
                                if let Ok(mut slot) = failure.lock() {
                                    slot.get_or_insert(error);
                                }
                            }
                        }
                    }
                });
            }
        });

        if let Some(error) = failure.into_inner().ok().and_then(|slot| slot) {
            return Err(error);
        }
        bodies.extend(fetched.into_inner().unwrap_or_default());
    }

    // Parsed in id order, so a report is reproducible regardless of the order
    // the network happened to answer in.
    let mut advisories = Vec::new();
    let mut unreadable = 0;
    for entry in &entries {
        let Some(body) = bodies.get(entry.id.as_str()) else {
            continue;
        };
        match Advisory::parse(body) {
            Ok(advisory) => advisories.push(advisory),
            // One malformed document must not abandon the rest, exactly as a
            // malformed file in a local corpus does not.
            Err(_) => unreadable += 1,
        }
    }

    let mut source = LocalDirSource::from_advisories(
        format!("OSV API ({} package(s) queried)", queryable.len()),
        advisories,
    );
    source.set_unreadable_documents(unreadable);
    Ok(source)
}

/// Ask OSV about one package, for `vuln check`.
///
/// An ecosystem OSV publishes no name for is an error rather than an empty
/// corpus. [`fetch_advisories`] drops such coordinates, which is right when it
/// is handed a whole repository, but here it would leave the one package that
/// was asked about unqueried and answer "no advisory names this package" --
/// which reads as safe.
pub fn fetch_advisories_for_package(
    transport: &dyn OsvTransport,
    coordinate: &PackageCoordinate,
    cache: Option<&Path>,
) -> Result<LocalDirSource, SourceError> {
    if osv_ecosystem_name(coordinate.ecosystem).is_none() {
        return Err(SourceError::UnsupportedEcosystem(
            coordinate.ecosystem.to_string(),
        ));
    }
    fetch_advisories(transport, std::slice::from_ref(coordinate), cache)
}

fn batch_query_body(coordinates: &[&PackageCoordinate]) -> String {
    let queries: Vec<serde_json::Value> = coordinates
        .iter()
        .filter_map(|coordinate| {
            let ecosystem = osv_ecosystem_name(coordinate.ecosystem)?;
            Some(serde_json::json!({
                "package": { "ecosystem": ecosystem, "name": coordinate.name }
            }))
        })
        .collect();
    serde_json::json!({ "queries": queries }).to_string()
}

fn collect_named(response: &str, out: &mut BTreeMap<String, Named>) {
    let Ok(document) = serde_json::from_str::<serde_json::Value>(response) else {
        return;
    };
    let Some(results) = document.get("results").and_then(|value| value.as_array()) else {
        return;
    };
    for result in results {
        let Some(vulns) = result.get("vulns").and_then(|value| value.as_array()) else {
            continue;
        };
        for vuln in vulns {
            let Some(id) = vuln.get("id").and_then(|value| value.as_str()) else {
                continue;
            };
            let modified = vuln
                .get("modified")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            out.entry(id.to_string()).or_insert_with(|| Named {
                id: id.to_string(),
                modified,
            });
        }
    }
}

/// A cached document is keyed on the id and the version OSV reports, so a
/// republished advisory is never served stale.
fn cache_path(cache: &Path, entry: &Named) -> std::path::PathBuf {
    let stamp = blake3::hash(entry.modified.as_bytes()).to_hex();
    let safe: String = entry
        .id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '.' {
                character
            } else {
                '_'
            }
        })
        .collect();
    cache.join(format!("{safe}.{}.json", &stamp[..16]))
}

fn cached(cache: Option<&Path>, entry: &Named) -> Option<String> {
    std::fs::read_to_string(cache_path(cache?, entry)).ok()
}

fn store(cache: Option<&Path>, entry: &Named, body: &str) {
    let Some(cache) = cache else { return };
    if std::fs::create_dir_all(cache).is_err() {
        return;
    }
    // A cache miss costs one request; a failed write is not worth an error.
    let _ = std::fs::write(cache_path(cache, entry), body);
}

/// The real transport, over HTTPS.
pub struct SystemOsvTransport {
    client: reqwest::blocking::Client,
}

impl SystemOsvTransport {
    pub fn new() -> Result<Self, SourceError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(OSV_TIMEOUT_SECONDS))
            .user_agent(concat!("synaptic/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| SourceError::Transport {
                url: OSV_API_BASE.into(),
                message: error.to_string(),
            })?;
        Ok(Self { client })
    }
}

impl OsvTransport for SystemOsvTransport {
    fn post_json(&self, url: &str, body: &str) -> Result<String, SourceError> {
        self.client
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::text)
            .map_err(|error| SourceError::Transport {
                url: url.into(),
                message: error.to_string(),
            })
    }

    fn get_json(&self, url: &str) -> Result<String, SourceError> {
        self.client
            .get(url)
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::text)
            .map_err(|error| SourceError::Transport {
                url: url.into(),
                message: error.to_string(),
            })
    }
}

/// Whether the environment forbids reaching the network.
///
/// Honoured everywhere an online path exists, so one variable turns the whole
/// tool offline regardless of which command or flag is in play.
pub fn offline_forced() -> bool {
    std::env::var("SYNAPTIC_OFFLINE")
        .map(|value| value != "0" && !value.is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use synaptic_api::{Ecosystem, PackageCoordinate};

    fn cargo(name: &str) -> PackageCoordinate {
        PackageCoordinate::new(Ecosystem::Cargo, name)
    }

    /// A transport that answers from a script rather than the network.
    #[derive(Default)]
    struct FakeTransport {
        batch: String,
        vulns: std::collections::BTreeMap<String, String>,
        posts: std::sync::Mutex<Vec<String>>,
        gets: std::sync::Mutex<Vec<String>>,
        fail: bool,
    }

    impl OsvTransport for FakeTransport {
        fn post_json(&self, url: &str, body: &str) -> Result<String, SourceError> {
            if self.fail {
                return Err(SourceError::Transport {
                    url: url.into(),
                    message: "connection reset".into(),
                });
            }
            self.posts.lock().unwrap().push(body.to_string());
            Ok(self.batch.clone())
        }

        fn get_json(&self, url: &str) -> Result<String, SourceError> {
            if self.fail {
                return Err(SourceError::Transport {
                    url: url.into(),
                    message: "connection reset".into(),
                });
            }
            self.gets.lock().unwrap().push(url.to_string());
            let id = url.rsplit('/').next().unwrap_or_default();
            self.vulns
                .get(id)
                .cloned()
                .ok_or_else(|| SourceError::Transport {
                    url: url.into(),
                    message: "404".into(),
                })
        }
    }

    fn advisory_json(id: &str) -> String {
        format!(
            r#"{{"id":"{id}","summary":"s","affected":[{{"package":{{"ecosystem":"crates.io","name":"leaf"}}}}]}}"#
        )
    }

    #[test]
    fn a_batch_query_resolves_every_advisory_it_names() {
        let transport = FakeTransport {
            batch: r#"{"results":[{"vulns":[{"id":"RUSTSEC-1","modified":"2026-01-01"}]}]}"#.into(),
            vulns: [("RUSTSEC-1".to_string(), advisory_json("RUSTSEC-1"))]
                .into_iter()
                .collect(),
            ..Default::default()
        };

        let source = fetch_advisories(&transport, &[cargo("leaf")], None).unwrap();

        assert_eq!(
            crate::AdvisorySource::describe(&source).advisory_count,
            1,
            "the advisory the batch named was fetched"
        );
    }

    #[test]
    fn the_query_uses_osv_ecosystem_names_not_synaptic_ones() {
        let transport = FakeTransport {
            batch: r#"{"results":[{"vulns":[]}]}"#.into(),
            ..Default::default()
        };

        fetch_advisories(&transport, &[cargo("leaf")], None).unwrap();

        let body = transport.posts.lock().unwrap()[0].clone();
        assert!(
            body.contains("crates.io"),
            "OSV spells cargo's ecosystem crates.io: {body}"
        );
    }

    #[test]
    fn an_ecosystem_osv_does_not_publish_is_not_queried() {
        let transport = FakeTransport {
            batch: r#"{"results":[]}"#.into(),
            ..Default::default()
        };

        let source = fetch_advisories(
            &transport,
            &[PackageCoordinate::new(Ecosystem::Generic, "thing")],
            None,
        )
        .unwrap();

        assert!(
            transport.posts.lock().unwrap().is_empty(),
            "no request is made for an ecosystem OSV has no name for"
        );
        assert_eq!(crate::AdvisorySource::describe(&source).advisory_count, 0);
    }

    #[test]
    fn the_same_advisory_named_by_two_packages_is_fetched_once() {
        let transport = FakeTransport {
            batch: r#"{"results":[
                {"vulns":[{"id":"RUSTSEC-1","modified":"2026-01-01"}]},
                {"vulns":[{"id":"RUSTSEC-1","modified":"2026-01-01"}]}
            ]}"#
            .into(),
            vulns: [("RUSTSEC-1".to_string(), advisory_json("RUSTSEC-1"))]
                .into_iter()
                .collect(),
            ..Default::default()
        };

        fetch_advisories(&transport, &[cargo("leaf"), cargo("other")], None).unwrap();

        assert_eq!(transport.gets.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_transport_failure_is_reported_rather_than_read_as_no_vulnerabilities() {
        let transport = FakeTransport {
            fail: true,
            ..Default::default()
        };

        let result = fetch_advisories(&transport, &[cargo("leaf")], None);

        assert!(
            result.is_err(),
            "a failed query must never look like a clean one"
        );
    }

    #[test]
    fn an_advisory_document_that_does_not_parse_is_skipped_not_fatal() {
        let transport = FakeTransport {
            batch: r#"{"results":[{"vulns":[
                {"id":"GOOD","modified":"2026-01-01"},
                {"id":"BAD","modified":"2026-01-01"}
            ]}]}"#
                .into(),
            vulns: [
                ("GOOD".to_string(), advisory_json("GOOD")),
                ("BAD".to_string(), "{ not json".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let source = fetch_advisories(&transport, &[cargo("leaf")], None).unwrap();
        let described = crate::AdvisorySource::describe(&source);

        assert_eq!(described.advisory_count, 1);
        assert_eq!(
            described.unreadable_documents, 1,
            "the gap stays visible rather than being swallowed"
        );
    }

    #[test]
    fn the_origin_says_the_answer_came_from_the_live_api() {
        // An agent reading a report must never mistake a live answer for a
        // corpus one, or the other way round.
        let transport = FakeTransport {
            batch: r#"{"results":[{"vulns":[]}]}"#.into(),
            ..Default::default()
        };

        let source = fetch_advisories(&transport, &[cargo("leaf")], None).unwrap();

        assert!(
            crate::AdvisorySource::describe(&source)
                .origin
                .contains("OSV API")
        );
    }

    #[test]
    fn a_cached_document_is_not_fetched_again_while_its_version_is_unchanged() {
        let cache = tempfile::tempdir().unwrap();
        let transport = FakeTransport {
            batch: r#"{"results":[{"vulns":[{"id":"RUSTSEC-1","modified":"2026-01-01"}]}]}"#.into(),
            vulns: [("RUSTSEC-1".to_string(), advisory_json("RUSTSEC-1"))]
                .into_iter()
                .collect(),
            ..Default::default()
        };

        fetch_advisories(&transport, &[cargo("leaf")], Some(cache.path())).unwrap();
        assert_eq!(transport.gets.lock().unwrap().len(), 1);

        // A second scan hits the cache: the batch still runs, so a newly
        // published advisory is still seen, but an unchanged document is not
        // downloaded twice.
        let source = fetch_advisories(&transport, &[cargo("leaf")], Some(cache.path())).unwrap();

        assert_eq!(
            transport.gets.lock().unwrap().len(),
            1,
            "served from the cache"
        );
        assert_eq!(crate::AdvisorySource::describe(&source).advisory_count, 1);
    }

    #[test]
    fn a_changed_advisory_version_invalidates_the_cached_copy() {
        let cache = tempfile::tempdir().unwrap();
        let first = FakeTransport {
            batch: r#"{"results":[{"vulns":[{"id":"RUSTSEC-1","modified":"2026-01-01"}]}]}"#.into(),
            vulns: [("RUSTSEC-1".to_string(), advisory_json("RUSTSEC-1"))]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        fetch_advisories(&first, &[cargo("leaf")], Some(cache.path())).unwrap();

        let second = FakeTransport {
            batch: r#"{"results":[{"vulns":[{"id":"RUSTSEC-1","modified":"2026-06-01"}]}]}"#.into(),
            vulns: [("RUSTSEC-1".to_string(), advisory_json("RUSTSEC-1"))]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        fetch_advisories(&second, &[cargo("leaf")], Some(cache.path())).unwrap();

        assert_eq!(
            second.gets.lock().unwrap().len(),
            1,
            "a republished advisory is downloaded again"
        );
    }

    #[test]
    fn a_query_larger_than_one_batch_is_split() {
        let transport = FakeTransport {
            batch: r#"{"results":[]}"#.into(),
            ..Default::default()
        };
        let coordinates: Vec<_> = (0..OSV_BATCH_LIMIT + 5)
            .map(|index| cargo(&format!("crate{index}")))
            .collect();

        fetch_advisories(&transport, &coordinates, None).unwrap();

        assert_eq!(
            transport.posts.lock().unwrap().len(),
            2,
            "OSV caps a querybatch, so a large repository needs several"
        );
    }
}
