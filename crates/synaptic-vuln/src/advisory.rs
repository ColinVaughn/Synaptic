use serde::{Deserialize, Serialize};
use synaptic_api::{Ecosystem, PackageCoordinate, PackageUrl};

/// How a severity score is encoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeverityKind {
    CvssV2,
    CvssV3,
    CvssV4,
    /// A scoring system this build does not model. The raw type is retained so
    /// nothing is silently discarded.
    Other(String),
}

/// One severity entry from an advisory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Severity {
    pub kind: SeverityKind,
    pub score: String,
}

/// A range boundary event. OSV encodes each as a single-key object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RangeEvent {
    Introduced(String),
    Fixed(String),
    LastAffected(String),
    Limit(String),
}

/// How the versions in a range are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RangeKind {
    SemVer,
    Ecosystem,
    Git,
}

/// An ordered set of boundary events describing affected versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionRange {
    pub kind: RangeKind,
    pub events: Vec<RangeEvent>,
}

/// One `affected` entry: a package plus the versions of it that are affected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Affected {
    pub package: PackageCoordinate,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub purl: Option<String>,
    #[serde(default)]
    pub ranges: Vec<VersionRange>,
    /// Explicitly enumerated affected versions, when the advisory lists them.
    #[serde(default)]
    pub versions: Vec<String>,
    /// Function paths the advisory names as vulnerable, when it names any.
    /// Their absence is meaningful: it means reachability cannot be decided
    /// from the advisory, not that nothing is reachable.
    #[serde(default)]
    pub affected_functions: Vec<String>,
}

/// A normalized OSV advisory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advisory {
    pub id: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub details: Option<String>,
    /// Present only on withdrawn advisories. Withdrawal is a positive signal
    /// that the advisory should no longer produce findings.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub withdrawn: Option<String>,
    #[serde(default)]
    pub severity: Vec<Severity>,
    #[serde(default)]
    pub affected: Vec<Affected>,
    #[serde(default)]
    pub references: Vec<String>,
    /// Publication timestamp, as the opaque string the corpus supplied.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub published: Option<String>,
    /// Last-modified timestamp. Used to report how stale a corpus is.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub modified: Option<String>,
}

impl Advisory {
    /// Whether the advisory has been withdrawn by its publisher.
    pub fn is_withdrawn(&self) -> bool {
        self.withdrawn.is_some()
    }

    /// Parse one OSV document.
    ///
    /// Parsing is deliberately tolerant of fields and enum values this build
    /// does not model: advisory corpora are third-party data, and a scan that
    /// aborts on one unrecognized document is a scan that silently stops
    /// covering everything after it.
    pub fn parse(source: &str) -> Result<Self, AdvisoryError> {
        let value: serde_json::Value = serde_json::from_str(source)?;
        let id = value
            .get("id")
            .ok_or(AdvisoryError::MissingField("id"))?
            .as_str()
            .ok_or(AdvisoryError::FieldType { field: "id" })?
            .to_string();

        let affected = value
            .get("affected")
            .and_then(serde_json::Value::as_array)
            .map(|entries| entries.iter().filter_map(parse_affected).collect())
            .unwrap_or_default();

        Ok(Self {
            id,
            aliases: string_array(&value, "aliases"),
            summary: string_field(&value, "summary"),
            details: string_field(&value, "details"),
            withdrawn: string_field(&value, "withdrawn"),
            severity: parse_severity(&value),
            affected,
            references: parse_references(&value),
            published: string_field(&value, "published"),
            modified: string_field(&value, "modified"),
        })
    }
}

fn string_array(value: &serde_json::Value, field: &str) -> Vec<String> {
    value
        .get(field)
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_severity(value: &serde_json::Value) -> Vec<Severity> {
    value
        .get("severity")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let score = entry.get("score").and_then(serde_json::Value::as_str)?;
                    let raw = entry
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    let kind = match raw.trim().to_ascii_uppercase().as_str() {
                        "CVSS_V2" => SeverityKind::CvssV2,
                        "CVSS_V3" => SeverityKind::CvssV3,
                        "CVSS_V4" => SeverityKind::CvssV4,
                        other => SeverityKind::Other(other.to_string()),
                    };
                    Some(Severity {
                        kind,
                        score: score.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_references(value: &serde_json::Value) -> Vec<String> {
    value
        .get("references")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("url").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_ranges(entry: &serde_json::Value) -> Vec<VersionRange> {
    entry
        .get("ranges")
        .and_then(serde_json::Value::as_array)
        .map(|ranges| {
            ranges
                .iter()
                .filter_map(|range| {
                    let kind = match range
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_ascii_uppercase()
                        .as_str()
                    {
                        "SEMVER" => RangeKind::SemVer,
                        "GIT" => RangeKind::Git,
                        // ECOSYSTEM is the OSV default for everything else, and
                        // an unrecognized type is treated the same way rather
                        // than dropping the range and under-reporting.
                        _ => RangeKind::Ecosystem,
                    };
                    let events = range
                        .get("events")
                        .and_then(serde_json::Value::as_array)?
                        .iter()
                        .filter_map(parse_range_event)
                        .collect::<Vec<_>>();
                    Some(VersionRange { kind, events })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_range_event(event: &serde_json::Value) -> Option<RangeEvent> {
    let object = event.as_object()?;
    for (key, raw) in object {
        let Some(version) = raw.as_str() else {
            continue;
        };
        return match key.as_str() {
            "introduced" => Some(RangeEvent::Introduced(version.to_string())),
            "fixed" => Some(RangeEvent::Fixed(version.to_string())),
            "last_affected" => Some(RangeEvent::LastAffected(version.to_string())),
            "limit" => Some(RangeEvent::Limit(version.to_string())),
            _ => continue,
        };
    }
    None
}

/// RustSec nests function paths under `ecosystem_specific.affects.functions`;
/// other databases put them directly under `ecosystem_specific.functions`.
/// Both are read so reachability data is not lost to a layout difference.
fn parse_affected_functions(entry: &serde_json::Value) -> Vec<String> {
    let Some(block) = entry.get("ecosystem_specific") else {
        return Vec::new();
    };
    let mut functions = string_array(block, "functions");
    if let Some(affects) = block.get("affects") {
        functions.extend(string_array(affects, "functions"));
    }
    functions
}

fn string_field(value: &serde_json::Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn parse_affected(entry: &serde_json::Value) -> Option<Affected> {
    let package = entry.get("package")?;
    let name = package.get("name").and_then(serde_json::Value::as_str)?;
    let ecosystem = package
        .get("ecosystem")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let purl = package
        .get("purl")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let coordinate = coordinate_from_osv(ecosystem, name, purl.as_deref())?;

    Some(Affected {
        package: coordinate,
        purl,
        ranges: parse_ranges(entry),
        versions: string_array(entry, "versions"),
        affected_functions: parse_affected_functions(entry),
    })
}

#[derive(Debug, thiserror::Error)]
pub enum AdvisoryError {
    #[error("advisory is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("advisory is missing required field {0:?}")]
    MissingField(&'static str),
    #[error("advisory field {field:?} has the wrong type")]
    FieldType { field: &'static str },
}

fn coordinate_from_osv(
    ecosystem: &str,
    name: &str,
    purl: Option<&str>,
) -> Option<PackageCoordinate> {
    if let Some(purl) = purl {
        if let Ok(parsed) = PackageUrl::parse(purl) {
            return Some(parsed.to_coordinate());
        }
    }
    // OSV ecosystem strings can carry a `:suffix` qualifier, e.g.
    // "Alpine:v3.16". Only the leading token identifies the package type.
    let head = ecosystem.split(':').next().unwrap_or(ecosystem).trim();
    let ecosystem = head.parse::<Ecosystem>().unwrap_or(Ecosystem::Generic);
    Some(PackageCoordinate::new(ecosystem, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_id_and_affected_package_from_a_minimal_document() {
        let source = r#"{
            "schema_version": "1.6.0",
            "id": "RUSTSEC-2026-0204",
            "summary": "Use-after-free in the epoch collector",
            "affected": [
                {
                    "package": { "ecosystem": "crates.io", "name": "crossbeam-epoch" }
                }
            ]
        }"#;

        let advisory = Advisory::parse(source).expect("minimal document must parse");

        assert_eq!(advisory.id, "RUSTSEC-2026-0204");
        assert_eq!(
            advisory.summary.as_deref(),
            Some("Use-after-free in the epoch collector")
        );
        assert_eq!(advisory.affected.len(), 1);
        assert_eq!(
            advisory.affected[0].package.to_string(),
            "cargo:crossbeam-epoch"
        );
    }

    #[test]
    fn parses_semver_ranges_with_introduced_and_fixed_events() {
        let source = r#"{
            "id": "OSV-1",
            "affected": [
                {
                    "package": { "ecosystem": "crates.io", "name": "example" },
                    "ranges": [
                        {
                            "type": "SEMVER",
                            "events": [{ "introduced": "0.9.0" }, { "fixed": "0.9.20" }]
                        }
                    ],
                    "versions": ["0.9.18"]
                }
            ]
        }"#;

        let advisory = Advisory::parse(source).unwrap();
        let affected = &advisory.affected[0];

        assert_eq!(affected.ranges.len(), 1);
        assert_eq!(affected.ranges[0].kind, RangeKind::SemVer);
        assert_eq!(
            affected.ranges[0].events,
            vec![
                RangeEvent::Introduced("0.9.0".into()),
                RangeEvent::Fixed("0.9.20".into()),
            ]
        );
        assert_eq!(affected.versions, vec!["0.9.18".to_string()]);
    }

    #[test]
    fn parses_cvss_severity_entries() {
        let source = r#"{
            "id": "OSV-2",
            "severity": [
                { "type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H" }
            ]
        }"#;

        let advisory = Advisory::parse(source).unwrap();

        assert_eq!(advisory.severity.len(), 1);
        assert_eq!(advisory.severity[0].kind, SeverityKind::CvssV3);
        assert!(advisory.severity[0].score.starts_with("CVSS:3.1/"));
    }

    #[test]
    fn parses_aliases_and_reference_urls() {
        let source = r#"{
            "id": "RUSTSEC-2026-0001",
            "aliases": ["CVE-2026-1111", "GHSA-aaaa-bbbb-cccc"],
            "references": [
                { "type": "ADVISORY", "url": "https://example.test/advisory" },
                { "type": "FIX", "url": "https://example.test/commit" }
            ]
        }"#;

        let advisory = Advisory::parse(source).unwrap();

        assert_eq!(
            advisory.aliases,
            vec![
                "CVE-2026-1111".to_string(),
                "GHSA-aaaa-bbbb-cccc".to_string()
            ]
        );
        assert_eq!(
            advisory.references,
            vec![
                "https://example.test/advisory".to_string(),
                "https://example.test/commit".to_string()
            ]
        );
    }

    #[test]
    fn prefers_the_purl_when_the_package_declares_one() {
        let source = r#"{
            "id": "OSV-3",
            "affected": [
                {
                    "package": {
                        "ecosystem": "npm",
                        "name": "payments",
                        "purl": "pkg:npm/%40acme/payments"
                    }
                }
            ]
        }"#;

        let advisory = Advisory::parse(source).unwrap();

        assert_eq!(
            advisory.affected[0].package.to_string(),
            "npm:@acme/payments"
        );
    }

    #[test]
    fn unknown_ecosystems_degrade_to_generic_instead_of_failing() {
        let source = r#"{
            "id": "OSV-4",
            "affected": [
                { "package": { "ecosystem": "Alpine:v3.16", "name": "openssl" } }
            ]
        }"#;

        let advisory = Advisory::parse(source).expect("unknown ecosystems must not fail the parse");

        assert_eq!(advisory.affected[0].package.to_string(), "generic:openssl");
    }

    #[test]
    fn withdrawn_advisories_report_themselves_as_withdrawn() {
        let live = Advisory::parse(r#"{ "id": "OSV-5" }"#).unwrap();
        let withdrawn =
            Advisory::parse(r#"{ "id": "OSV-6", "withdrawn": "2026-01-02T00:00:00Z" }"#).unwrap();

        assert!(!live.is_withdrawn());
        assert!(withdrawn.is_withdrawn());
    }

    #[test]
    fn collects_affected_functions_from_the_rustsec_ecosystem_block() {
        let source = r#"{
            "id": "RUSTSEC-2026-0002",
            "affected": [
                {
                    "package": { "ecosystem": "crates.io", "name": "example" },
                    "ecosystem_specific": {
                        "affects": { "functions": ["example::Collector::pin"] }
                    }
                }
            ]
        }"#;

        let advisory = Advisory::parse(source).unwrap();

        assert_eq!(
            advisory.affected[0].affected_functions,
            vec!["example::Collector::pin".to_string()]
        );
    }

    #[test]
    fn records_the_modified_timestamp_used_for_corpus_staleness() {
        let advisory = Advisory::parse(
            r#"{ "id": "OSV-7", "modified": "2026-07-30T10:00:00Z", "published": "2026-01-01T00:00:00Z" }"#,
        )
        .unwrap();

        assert_eq!(advisory.modified.as_deref(), Some("2026-07-30T10:00:00Z"));
        assert_eq!(advisory.published.as_deref(), Some("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn a_document_without_an_id_is_rejected() {
        let error = Advisory::parse(r#"{ "summary": "no id" }"#).unwrap_err();

        assert!(matches!(error, AdvisoryError::MissingField("id")));
    }
}
