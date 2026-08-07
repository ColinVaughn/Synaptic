//! Agent-facing vulnerability tools.
//!
//! These expose the dependency-safety guardrail and the findings ledger to
//! assistants so generated code does not reach for a known-vulnerable version.
//!
//! The ledger tools read local state only. `vuln_check_dependency` asks OSV
//! about the one package it was given, unless an operator configured a corpus
//! or set `SYNAPTIC_OFFLINE=1`, in which case it reads that instead. It is the
//! only tool in the server that reaches the network.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use synaptic_vuln::{check_dependency, AdvisorySource, FindingStore, LocalDirSource, VulnPolicy};

/// Environment variable naming the OSV advisory directory.
pub(crate) const ADVISORY_DIR_ENV: &str = "SYNAPTIC_VULN_ADVISORIES";

/// Conventional in-repository advisory location.
const CONVENTIONAL_ADVISORY_DIR: &str = ".synaptic/vuln/advisories";

/// The repository root implied by a graph path (`<root>/synaptic-out/graph.json`).
///
/// A relative graph path such as `synaptic-out/graph.json` has an empty
/// grandparent. An empty `Path` is not the current directory: it does not
/// exist and cannot be read, so it is normalized to `.` here.
pub(crate) fn repository_root(graph_path: Option<&Path>) -> Option<PathBuf> {
    let root = graph_path?.parent().and_then(Path::parent)?;
    Some(if root.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        root.to_path_buf()
    })
}

/// Where advisories live, if anywhere.
pub(crate) fn advisory_dir(root: &Path) -> Option<PathBuf> {
    if let Ok(configured) = std::env::var(ADVISORY_DIR_ENV) {
        let path = PathBuf::from(configured);
        if path.is_dir() {
            return Some(path);
        }
        return None;
    }
    let conventional = root.join(CONVENTIONAL_ADVISORY_DIR);
    conventional.is_dir().then_some(conventional)
}

/// Obtain advisories for one package, preferring the live OSV API.
///
/// Checking a single package is a question about that package, so it is asked
/// directly. The guardrails matter more here than in the CLI, because an
/// assistant cannot see a warning on stderr:
///
/// - `SYNAPTIC_OFFLINE=1` disables the query outright.
/// - An explicitly configured corpus wins, because the operator chose it.
/// - A network failure degrades to that corpus rather than failing, and the
///   returned message says the answer is degraded. It never reports "safe".
#[allow(clippy::type_complexity)]
fn resolve_check_source(
    root: &Path,
    coordinate: &synaptic_vuln::PackageCoordinate,
    transport: Option<&dyn synaptic_vuln::OsvTransport>,
    synced: Option<&Path>,
) -> Result<(LocalDirSource, Option<String>), (String, Value)> {
    // An operator who configured a corpus chose their data source, exactly as
    // `--advisories` does on the command line. That choice wins, and it keeps
    // this path deterministic and network-free.
    if let Some(directory) = advisory_dir(root) {
        return match LocalDirSource::load(&directory) {
            Ok(source) => Ok((source, None)),
            Err(_) => Err((
                format!(
                    "The advisory corpus at {} could not be read.",
                    directory.display()
                ),
                json!({ "error": "unreadable_advisory_corpus" }),
            )),
        };
    }

    let mut live_error = None;
    if let Some(transport) = transport {
        let cache = synaptic_vuln::CorpusCache::user_default().map(|cache| cache.live_dir());
        match synaptic_vuln::fetch_advisories_for_package(transport, coordinate, cache.as_deref()) {
            Ok(source) => return Ok((source, None)),
            // Remember why, then fall back to whatever is already on disk. The
            // caller reports both the answer and the fact that it is degraded.
            Err(error) => live_error = Some(error.to_string()),
        }
    }

    // The shared corpus a `synaptic vuln sync` left behind. It may be days old,
    // which is why an answer from it is labelled when the API was meant to
    // supply one.
    if let Some(source) = synced.and_then(|directory| LocalDirSource::load(directory).ok()) {
        return Ok((source, live_error));
    }

    let reason = match &live_error {
        Some(error) => format!("OSV could not answer ({error})"),
        None => "querying OSV is disabled".to_string(),
    };
    Err((
        format!(
            "No advisory corpus is configured and {reason}, so {coordinate} could not be \
             checked. Set {ADVISORY_DIR_ENV} to a directory of OSV JSON documents, place one \
             at {CONVENTIONAL_ADVISORY_DIR}, or run `synaptic vuln sync`. Treat this as \
             UNKNOWN, not as safe."
        ),
        json!({ "error": "no_advisory_corpus" }),
    ))
}

/// Answer whether a package is safe to use, and at what version.
pub(crate) fn check_dependency_tool(
    root: &Path,
    package: &str,
    version: Option<&str>,
) -> (String, Value) {
    // Built here rather than inside the resolver so tests can drive the whole
    // tool without a network, and so `SYNAPTIC_OFFLINE` is read in exactly one
    // place.
    let transport = (!synaptic_vuln::offline_forced())
        .then(synaptic_vuln::SystemOsvTransport::new)
        .and_then(Result::ok);
    let synced = synaptic_vuln::CorpusCache::user_default()
        .and_then(|cache| cache.resolve(coordinate_ecosystem(package)?));
    check_dependency_tool_with(
        root,
        package,
        version,
        transport
            .as_ref()
            .map(|transport| transport as &dyn synaptic_vuln::OsvTransport),
        synced.as_deref(),
    )
}

/// The ecosystem a coordinate names, for locating its synced corpus.
fn coordinate_ecosystem(package: &str) -> Option<synaptic_vuln::Ecosystem> {
    package
        .parse::<synaptic_vuln::PackageCoordinate>()
        .ok()
        .map(|coordinate| coordinate.ecosystem)
}

fn check_dependency_tool_with(
    root: &Path,
    package: &str,
    version: Option<&str>,
    transport: Option<&dyn synaptic_vuln::OsvTransport>,
    synced: Option<&Path>,
) -> (String, Value) {
    let Ok(coordinate) = package.parse() else {
        return (
            format!(
                "{package:?} is not a package coordinate. Use <ecosystem>:<name>, \
                 for example cargo:serde or npm:@acme/sdk."
            ),
            json!({ "error": "invalid_package_coordinate" }),
        );
    };

    let (source, live_error) = match resolve_check_source(root, &coordinate, transport, synced) {
        Ok(resolved) => resolved,
        Err(message) => return (message.0, message.1),
    };
    let policy = VulnPolicy::load(root).ok().flatten();
    let safety = check_dependency(&coordinate, version, &source, policy.as_ref());
    let corpus = source.describe();

    let mut text = format!("{:?} {}", safety.verdict, safety.package);
    if let Some(version) = &safety.requested_version {
        text.push_str(&format!(" at {version}"));
    }
    text.push('\n');
    if let Some(constraint) = &safety.approved_constraint {
        text.push_str(&format!(
            "Use {constraint}. This constraint comes from advisory metadata and has NOT been \
             checked against a registry, so confirm the version resolves.\n"
        ));
    }
    for alternative in &safety.alternatives {
        text.push_str(&format!("Alternative: {alternative}\n"));
    }
    for reason in &safety.reasons {
        text.push_str(&format!("- {reason}\n"));
    }
    if safety.advisories.is_empty() && safety.reasons.is_empty() {
        text.push_str(&format!(
            "No advisory in the corpus names this package ({} advisories, newest {}).\n",
            corpus.advisory_count,
            corpus.newest_modified.as_deref().unwrap_or("unknown")
        ));
    }
    // A degraded answer must be unmistakable in the text, because that is what
    // the model reads. A local corpus can be days old; "nothing found" against
    // a stale corpus is a weaker claim than "nothing found" against OSV.
    if let Some(error) = &live_error {
        text.push_str(&format!(
            "DEGRADED: OSV could not answer ({error}), so this answer comes from \
             the local corpus at {} and may be out of date.\n",
            corpus.origin
        ));
    }

    let structured = json!({
        "verdict": safety.verdict,
        "package": safety.package.to_string(),
        "requested_version": safety.requested_version,
        "advisories": safety.advisories,
        "approved_constraint": safety.approved_constraint,
        "constraint_availability": safety.constraint_availability,
        "alternatives": safety.alternatives,
        "reasons": safety.reasons,
        "corpus": corpus,
        "degraded": live_error,
    });
    (text, structured)
}

/// List findings recorded in the repository's ledger.
pub(crate) fn findings_tool(root: &Path, state: Option<&str>, limit: usize) -> (String, Value) {
    let store = FindingStore::new(root);
    let Ok(records) = store.list() else {
        return (
            "The findings ledger could not be read.".into(),
            json!({ "error": "unreadable_ledger" }),
        );
    };
    let filtered = records
        .into_iter()
        .filter(|record| {
            state.is_none_or(|wanted| {
                serde_json::to_value(record.state)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .is_some_and(|actual| actual == wanted)
            })
        })
        .collect::<Vec<_>>();
    let total = filtered.len();
    let shown = filtered.into_iter().take(limit).collect::<Vec<_>>();

    if shown.is_empty() {
        return (
            "No vulnerability findings are recorded. Note that an empty ledger means no scan \
             has been recorded, not that the repository is clean; run `synaptic vuln scan \
             --record` to populate it."
                .into(),
            json!({ "total": 0, "findings": [] }),
        );
    }

    let mut text = format!("{total} finding(s) recorded:\n");
    let mut entries = Vec::new();
    for record in &shown {
        text.push_str(&format!(
            "{} [{:?}] {:?} {} {}@{} -> {}\n",
            record.id,
            record.finding.priority,
            record.state,
            record.finding.advisory_id,
            record.finding.package,
            record.finding.resolved_version,
            record
                .finding
                .remediation
                .recommended_version
                .as_deref()
                .unwrap_or("no fix available")
        ));
        entries.push(json!({
            "id": record.id,
            "state": record.state,
            "priority": record.finding.priority,
            "advisory_id": record.finding.advisory_id,
            "package": record.finding.package.to_string(),
            "resolved_version": record.finding.resolved_version,
            "applicability": record.finding.verdict.state,
            "severity": record.finding.severity.band,
            "recommended_version": record.finding.remediation.recommended_version,
        }));
    }
    (text, json!({ "total": total, "findings": entries }))
}

/// Explain one finding: evidence, dependency path, remediation, history.
pub(crate) fn explain_tool(root: &Path, finding: &str) -> (String, Value) {
    let store = FindingStore::new(root);
    let record = match store.get(finding) {
        Ok(Some(record)) => record,
        Ok(None) => {
            return (
                format!("Finding {finding} is not in the ledger."),
                json!({ "error": "unknown_finding" }),
            )
        }
        Err(error) => {
            return (
                format!("Finding {finding} could not be read: {error}"),
                json!({ "error": "unreadable_finding" }),
            )
        }
    };

    let path = record
        .finding
        .dependency_path
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let mut text = format!(
        "{} {}@{}\nstate: {:?}  priority: {:?}  severity: {:?}\n",
        record.finding.advisory_id,
        record.finding.package,
        record.finding.resolved_version,
        record.finding.verdict.state,
        record.finding.priority,
        record.finding.severity.band,
    );
    if let Some(summary) = &record.finding.summary {
        text.push_str(&format!("{summary}\n"));
    }
    if !path.is_empty() {
        text.push_str(&format!("path: {}\n", path.join(" -> ")));
    }
    text.push_str("evidence:\n");
    for item in &record.finding.verdict.evidence {
        text.push_str(&format!(
            "  [{:?}] {:?}: {}\n",
            item.direction, item.kind, item.detail
        ));
    }
    for note in &record.finding.remediation.notes {
        text.push_str(&format!("note: {note}\n"));
    }

    let structured = json!({
        "id": record.id,
        "state": record.state,
        "finding": record.finding,
        "decisions": record.decisions,
    });
    (text, structured)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_advisory(dir: &Path, id: &str, package: &str, fixed: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(format!("{id}.json")),
            format!(
                r#"{{
                    "id": "{id}",
                    "summary": "{package} is vulnerable",
                    "affected": [
                        {{
                            "package": {{ "ecosystem": "crates.io", "name": "{package}" }},
                            "ranges": [
                                {{ "type": "SEMVER", "events": [
                                    {{ "introduced": "0" }}, {{ "fixed": "{fixed}" }}
                                ] }}
                            ]
                        }}
                    ]
                }}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn derives_the_repository_root_from_the_graph_path() {
        let root = repository_root(Some(Path::new("/repo/synaptic-out/graph.json")));

        assert_eq!(root, Some(PathBuf::from("/repo")));
    }

    #[test]
    fn a_relative_graph_path_resolves_to_the_working_directory_not_an_empty_path() {
        // `synaptic-out/graph.json` has an empty grandparent. An empty `Path`
        // is not the current directory: it does not exist and cannot be read,
        // which silently broke ledger listing when the server was started with
        // a relative --graph.
        let root = repository_root(Some(Path::new("synaptic-out/graph.json")));

        assert_eq!(root, Some(PathBuf::from(".")));
        assert!(
            root.as_deref().is_some_and(Path::exists),
            "the derived root must be a readable directory"
        );
    }

    /// A transport that always fails, standing in for a machine with no route
    /// to OSV.
    struct UnreachableOsv;

    impl synaptic_vuln::OsvTransport for UnreachableOsv {
        fn post_json(&self, url: &str, _body: &str) -> Result<String, synaptic_vuln::SourceError> {
            Err(synaptic_vuln::SourceError::Transport {
                url: url.into(),
                message: "network is unreachable".into(),
            })
        }

        fn get_json(&self, url: &str) -> Result<String, synaptic_vuln::SourceError> {
            Err(synaptic_vuln::SourceError::Transport {
                url: url.into(),
                message: "network is unreachable".into(),
            })
        }
    }

    #[test]
    fn an_unreachable_api_falls_back_to_the_synced_corpus_and_says_the_answer_is_degraded() {
        // An assistant cannot see a warning on stderr, so the degradation has
        // to be in the text it reads. A stale corpus finding nothing is a much
        // weaker claim than OSV finding nothing, and the two must not read the
        // same.
        let repo = tempfile::tempdir().unwrap();
        let synced = tempfile::tempdir().unwrap();
        write_advisory(synced.path(), "RUSTSEC-2026-0001", "example", "1.5.0");

        let (text, structured) = check_dependency_tool_with(
            repo.path(),
            "cargo:example",
            Some("1.0.0"),
            Some(&UnreachableOsv),
            Some(synced.path()),
        );

        assert!(
            text.contains("DEGRADED"),
            "a fallback answer must announce itself: {text}"
        );
        assert!(
            structured["degraded"].is_string(),
            "and be machine-readable: {structured}"
        );
        assert_eq!(
            structured["verdict"], "blocked",
            "the fallback still answers the question"
        );
    }

    #[test]
    fn a_reachable_api_answer_is_not_labelled_degraded() {
        let repo = tempfile::tempdir().unwrap();
        let synced = tempfile::tempdir().unwrap();
        write_advisory(synced.path(), "RUSTSEC-2026-0001", "example", "1.5.0");

        let (text, structured) = check_dependency_tool_with(
            repo.path(),
            "cargo:example",
            Some("1.0.0"),
            None,
            Some(synced.path()),
        );

        assert!(!text.contains("DEGRADED"), "{text}");
        assert!(structured["degraded"].is_null());
    }

    /// A transport that would answer, if it were ever asked.
    struct NeverAsked;

    impl synaptic_vuln::OsvTransport for NeverAsked {
        fn post_json(&self, _url: &str, _body: &str) -> Result<String, synaptic_vuln::SourceError> {
            Ok(r#"{"results":[{}]}"#.into())
        }

        fn get_json(&self, _url: &str) -> Result<String, synaptic_vuln::SourceError> {
            Ok("{}".into())
        }
    }

    #[test]
    fn an_ecosystem_osv_does_not_publish_reports_unknown_rather_than_safe() {
        // OSV has no name for this ecosystem, so the query would be dropped and
        // come back empty. An empty answer is indistinguishable from "nothing
        // is wrong", which is the one thing this tool must never say by
        // accident.
        let repo = tempfile::tempdir().unwrap();

        let (text, structured) = check_dependency_tool_with(
            repo.path(),
            "swift:Alamofire",
            Some("1.0.0"),
            Some(&NeverAsked),
            None,
        );

        assert_eq!(structured["error"], "no_advisory_corpus");
        assert!(text.contains("UNKNOWN"), "{text}");
    }

    #[test]
    fn a_missing_corpus_reports_unknown_rather_than_safe() {
        let dir = tempfile::tempdir().unwrap();

        let (text, structured) =
            check_dependency_tool_with(dir.path(), "cargo:example", Some("1.0.0"), None, None);

        assert_eq!(structured["error"], "no_advisory_corpus");
        assert!(
            text.contains("UNKNOWN, not as safe"),
            "an agent must not read a missing corpus as an all-clear: {text}"
        );
    }

    #[test]
    fn blocks_a_vulnerable_version_from_the_conventional_corpus() {
        let dir = tempfile::tempdir().unwrap();
        write_advisory(
            &dir.path().join(CONVENTIONAL_ADVISORY_DIR),
            "RUSTSEC-2026-0001",
            "example",
            "1.5.0",
        );

        let (text, structured) = check_dependency_tool(dir.path(), "cargo:example", Some("1.2.0"));

        assert_eq!(structured["verdict"], "blocked");
        assert_eq!(structured["approved_constraint"], ">=1.5.0");
        assert!(text.contains("RUSTSEC-2026-0001"));
        assert!(
            text.contains("NOT been checked against a registry"),
            "the tool must not imply it verified availability"
        );
    }

    #[test]
    fn allows_a_package_no_advisory_names() {
        let dir = tempfile::tempdir().unwrap();
        write_advisory(
            &dir.path().join(CONVENTIONAL_ADVISORY_DIR),
            "RUSTSEC-2026-0001",
            "example",
            "1.5.0",
        );

        let (_, structured) = check_dependency_tool(dir.path(), "cargo:unrelated", Some("1.0.0"));

        assert_eq!(structured["verdict"], "allowed");
    }

    #[test]
    fn rejects_a_malformed_package_coordinate() {
        let dir = tempfile::tempdir().unwrap();

        let (_, structured) = check_dependency_tool(dir.path(), "just-a-name", None);

        assert_eq!(structured["error"], "invalid_package_coordinate");
    }

    #[test]
    fn an_empty_ledger_says_so_without_implying_cleanliness() {
        let dir = tempfile::tempdir().unwrap();

        let (text, structured) = findings_tool(dir.path(), None, 20);

        assert_eq!(structured["total"], 0);
        assert!(
            text.contains("not that the repository is clean"),
            "an empty ledger must not read as an all-clear: {text}"
        );
    }

    #[test]
    fn an_unknown_finding_is_reported_as_unknown() {
        let dir = tempfile::tempdir().unwrap();

        let (_, structured) = explain_tool(dir.path(), "vuln_finding_missing");

        assert_eq!(structured["error"], "unknown_finding");
    }
}
