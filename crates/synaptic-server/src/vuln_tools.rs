//! Agent-facing vulnerability tools.
//!
//! These expose the dependency-safety guardrail and the findings ledger to
//! assistants so generated code does not reach for a known-vulnerable version.
//! Everything here reads local state only: a checked-out advisory corpus and
//! the repository's own ledger.

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

/// Answer whether a package is safe to use, and at what version.
pub(crate) fn check_dependency_tool(
    root: &Path,
    package: &str,
    version: Option<&str>,
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

    let Some(directory) = advisory_dir(root) else {
        return (
            format!(
                "No advisory corpus is configured, so dependency safety cannot be checked. \
                 Set {ADVISORY_DIR_ENV} to a directory of OSV JSON documents, or place one \
                 at {CONVENTIONAL_ADVISORY_DIR}. Treat this as UNKNOWN, not as safe."
            ),
            json!({ "error": "no_advisory_corpus" }),
        );
    };
    let Ok(source) = LocalDirSource::load(&directory) else {
        return (
            format!(
                "The advisory corpus at {} could not be read.",
                directory.display()
            ),
            json!({ "error": "unreadable_advisory_corpus" }),
        );
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

    #[test]
    fn a_missing_corpus_reports_unknown_rather_than_safe() {
        let dir = tempfile::tempdir().unwrap();

        let (text, structured) = check_dependency_tool(dir.path(), "cargo:example", Some("1.0.0"));

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
