use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use synaptic_api::{Dependency, DependencyScope, Ecosystem, PackageCoordinate};
use synaptic_core::GraphData;

use crate::advisory::Advisory;
use crate::applicability::{assess_applicability, ApplicabilityInput};
use crate::finding::{finding_id, Finding};
use crate::lockgraph::PackageGraph;
use crate::matching::{match_version, VersionMatch};
use crate::plan::plan_remediation;
use crate::policy::VulnPolicy;
use crate::severity::{assess_severity, prioritize, PriorityInputs};
use crate::source::{AdvisorySource, SourceDescription};

/// Supplies the graph-derived signals the applicability combiner uses.
///
/// Kept behind a trait so the scan can run, and be tested, with no graph at
/// all. A repository without a Synaptic graph still gets version and
/// dependency-path analysis; it simply gets fewer raising signals.
pub trait UsageOracle {
    /// Whether first-party code imports or calls the package.
    fn first_party_usage(&self, package: &PackageCoordinate) -> bool;

    /// Which of the advisory's named functions appear reachable.
    fn reachable_functions(
        &self,
        package: &PackageCoordinate,
        candidates: &[String],
    ) -> Vec<String>;

    /// Whether dynamic dispatch or reflection makes reachability unreliable.
    fn dynamic_hazard(&self, package: &PackageCoordinate) -> bool;
}

/// The oracle used when no graph is available.
///
/// Returns no evidence at all, which by the combiner's rules leaves findings at
/// `ReviewRequired` rather than dismissing them.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoUsageEvidence;

impl UsageOracle for NoUsageEvidence {
    fn first_party_usage(&self, _package: &PackageCoordinate) -> bool {
        false
    }

    fn reachable_functions(
        &self,
        _package: &PackageCoordinate,
        _candidates: &[String],
    ) -> Vec<String> {
        Vec::new()
    }

    fn dynamic_hazard(&self, _package: &PackageCoordinate) -> bool {
        false
    }
}

/// Reads usage signals out of a Synaptic graph.
///
/// External packages appear in the graph as SDK stub nodes labelled
/// `Sdk: <ecosystem>:<package>#<member>` with an empty source file. An incoming
/// edge from a node that does have a source file is first-party usage.
#[derive(Debug, Clone, Copy)]
pub struct GraphUsageOracle<'a> {
    graph: &'a GraphData,
}

impl<'a> GraphUsageOracle<'a> {
    pub fn new(graph: &'a GraphData) -> Self {
        Self { graph }
    }

    /// SDK stub nodes for a package, paired with whether first-party code
    /// reaches them.
    fn stub_usage(&self, package: &PackageCoordinate) -> Vec<(String, bool, bool)> {
        let wanted = normalize_package_ident(&package.name);
        let mut stubs = BTreeMap::new();
        for node in &self.graph.nodes {
            if !node.is_external_stub() {
                continue;
            }
            let Some((ecosystem, name, member)) = parse_sdk_label(&node.label) else {
                continue;
            };
            if ecosystem != package.ecosystem.as_str() {
                continue;
            }
            if normalize_package_ident(&name) != wanted {
                continue;
            }
            stubs.insert(node.id.0.clone(), member);
        }
        if stubs.is_empty() {
            return Vec::new();
        }

        // An edge whose source has a real source file is first-party usage.
        let sources: BTreeMap<&str, &synaptic_core::Node> = self
            .graph
            .nodes
            .iter()
            .map(|node| (node.id.0.as_str(), node))
            .collect();

        let mut reached: BTreeMap<String, (bool, bool)> = BTreeMap::new();
        for edge in &self.graph.links {
            let Some(member) = stubs.get(&edge.target.0) else {
                continue;
            };
            let Some(source) = sources.get(edge.source.0.as_str()) else {
                continue;
            };
            if source.is_external_stub() {
                continue;
            }
            let entry = reached.entry(member.clone()).or_insert((false, false));
            entry.0 = true;
            entry.1 |= source.dynamically_referenced();
        }

        stubs
            .into_values()
            .map(|member| {
                let (used, hazard) = reached.get(&member).copied().unwrap_or((false, false));
                (member, used, hazard)
            })
            .collect()
    }
}

impl UsageOracle for GraphUsageOracle<'_> {
    fn first_party_usage(&self, package: &PackageCoordinate) -> bool {
        self.stub_usage(package).iter().any(|(_, used, _)| *used)
    }

    fn reachable_functions(
        &self,
        package: &PackageCoordinate,
        candidates: &[String],
    ) -> Vec<String> {
        let reached = self.stub_usage(package);
        candidates
            .iter()
            .filter(|candidate| {
                let wanted = final_segment(candidate);
                reached.iter().any(|(member, used, _)| {
                    *used && final_segment(member).eq_ignore_ascii_case(&wanted)
                })
            })
            .cloned()
            .collect()
    }

    fn dynamic_hazard(&self, package: &PackageCoordinate) -> bool {
        self.stub_usage(package)
            .iter()
            .any(|(_, _, hazard)| *hazard)
    }
}

/// `Sdk: cargo:serde_json#Value.get` -> (`cargo`, `serde_json`, `Value.get`).
fn parse_sdk_label(label: &str) -> Option<(String, String, String)> {
    let body = label.strip_prefix("Sdk: ")?;
    let (coordinate, member) = body.split_once('#')?;
    let (ecosystem, name) = coordinate.split_once(':')?;
    Some((
        ecosystem.trim().to_ascii_lowercase(),
        name.trim().to_string(),
        member.trim().to_string(),
    ))
}

/// Rust identifiers replace hyphens with underscores, so a `cargo` package can
/// appear either way depending on whether the name came from a manifest or from
/// source. Compare on a single normal form.
fn normalize_package_ident(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('-', "_")
}

fn final_segment(path: &str) -> String {
    path.rsplit([':', '.', '/'])
        .next()
        .unwrap_or(path)
        .to_string()
}

/// A finding hidden by an unexpired policy exception.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuppressedFinding {
    pub finding_id: String,
    pub advisory_id: String,
    pub reason: String,
    pub expires: String,
    pub approved_by: String,
}

/// Everything one scan needs. Plain data, so a scan is reproducible from it.
pub struct ScanRequest<'a> {
    pub repository_identity: &'a str,
    /// The resolved dependency graph, from every lockfile in the repository.
    pub packages: &'a PackageGraph,
    /// Direct dependencies with their declared scope, from
    /// `synaptic_api::scan_dependencies`.
    pub direct_dependencies: &'a [Dependency],
    pub source: &'a dyn AdvisorySource,
    pub policy: Option<&'a VulnPolicy>,
    pub usage: &'a dyn UsageOracle,
    pub validation_commands: Vec<String>,
    /// Today's date as `YYYY-MM-DD`, used to expire policy exceptions.
    pub today: String,
    /// Ecosystems an advisory corpus was actually obtained for. Packages
    /// outside these are counted as unaudited rather than scanned.
    pub covered_ecosystems: std::collections::BTreeSet<Ecosystem>,
}

/// The result of one scan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanReport {
    pub version: u32,
    /// Provenance of the advisory corpus, always reported so an empty result is
    /// never mistaken for a clean bill of health.
    pub corpus: SourceDescription,
    pub packages_scanned: usize,
    /// Packages skipped because their ecosystem had no corpus. These were not
    /// checked at all; they are not known to be clean.
    pub packages_unaudited: usize,
    /// Ecosystems present in the lockfiles but lacking a corpus.
    pub uncovered_ecosystems: std::collections::BTreeSet<Ecosystem>,
    /// Findings that need attention, ordered by priority then id.
    pub findings: Vec<Finding>,
    /// Findings an unexpired exception is currently suppressing.
    pub suppressed: Vec<SuppressedFinding>,
}

impl ScanReport {
    pub const VERSION: u32 = 1;

    /// Findings whose applicability was confirmed rather than merely possible.
    pub fn applicable(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|finding| finding.verdict.state == synaptic_api::ApplicabilityState::Applicable)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error(transparent)]
    LockGraph(#[from] crate::lockgraph::LockGraphError),
}

/// Detect, assess, prioritise and plan every applicable vulnerability.
pub fn scan(request: &ScanRequest) -> Result<ScanReport, ScanError> {
    let graph = request.packages;
    let direct: BTreeMap<&PackageCoordinate, &Dependency> = request
        .direct_dependencies
        .iter()
        .map(|dependency| (&dependency.package, dependency))
        .collect();

    let mut findings = Vec::new();
    let mut suppressed = Vec::new();
    let mut packages_scanned = 0;

    let mut packages_unaudited = 0;
    let mut uncovered_ecosystems = std::collections::BTreeSet::new();
    let mut seen_findings = std::collections::BTreeSet::new();

    for resolved in graph.scan_targets() {
        if !request
            .covered_ecosystems
            .contains(&resolved.key.coordinate.ecosystem)
        {
            packages_unaudited += 1;
            uncovered_ecosystems.insert(resolved.key.coordinate.ecosystem);
            continue;
        }
        packages_scanned += 1;
        let package = &resolved.key.coordinate;
        let declared = direct.get(package).copied();

        for advisory in request.source.advisories_for(package) {
            for affected in &advisory.affected {
                if &affected.package != package {
                    continue;
                }
                let version_match = match_version(&resolved.key.version, affected);
                if matches!(version_match, VersionMatch::Unaffected) && !advisory.is_withdrawn() {
                    continue;
                }

                let advisory_functions = affected.affected_functions.clone();
                let reachable_functions = request
                    .usage
                    .reachable_functions(package, &advisory_functions);
                // Cargo lockfiles record no dependency kind, so a transitive
                // package is assumed runtime-reachable. Only a directly
                // declared dev dependency is known not to be.
                let runtime_reachable = declared
                    .map(|dependency| dependency.scope != DependencyScope::Development)
                    .unwrap_or(true);

                let verdict = assess_applicability(&ApplicabilityInput {
                    version_match,
                    advisory_withdrawn: advisory.is_withdrawn(),
                    advisory_functions,
                    reachable_functions,
                    first_party_usage_observed: request.usage.first_party_usage(package),
                    is_direct_dependency: declared.is_some(),
                    runtime_reachable,
                    dynamic_hazard_present: request.usage.dynamic_hazard(package),
                });

                if verdict.state == synaptic_api::ApplicabilityState::NotApplicable {
                    continue;
                }

                let severity = assess_severity(advisory);
                let priority = prioritize(&PriorityInputs {
                    severity: severity.band,
                    applicability: verdict.state,
                    runtime_reachable: verdict.runtime_reachable,
                });
                let id = finding_id(
                    request.repository_identity,
                    &advisory.id,
                    package,
                    &resolved.key.version,
                );

                if let Some(exception) = request
                    .policy
                    .and_then(|policy| policy.active_exception(&id, &request.today))
                {
                    suppressed.push(SuppressedFinding {
                        finding_id: id,
                        advisory_id: advisory.id.clone(),
                        reason: exception.reason.clone(),
                        expires: exception.expires.clone(),
                        approved_by: exception.approved_by.clone(),
                    });
                    continue;
                }

                if !seen_findings.insert(id.clone()) {
                    // The same advisory can name a package in several affected
                    // entries; they are one finding, not several.
                    continue;
                }
                findings.push(Finding {
                    version: Finding::VERSION,
                    id,
                    advisory_id: advisory.id.clone(),
                    aliases: advisory.aliases.clone(),
                    summary: advisory.summary.clone(),
                    package: package.clone(),
                    resolved_version: resolved.key.version.clone(),
                    dependency_path: graph
                        .shortest_path_from_root(&resolved.key)
                        .unwrap_or_default(),
                    is_direct_dependency: declared.is_some(),
                    verdict,
                    severity,
                    priority,
                    remediation: plan_remediation(
                        &resolved.key.version,
                        affected,
                        &request.validation_commands,
                    ),
                    references: advisory.references.clone(),
                });
            }
        }
    }

    findings.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.id.cmp(&right.id))
    });
    suppressed.sort_by(|left, right| left.finding_id.cmp(&right.finding_id));

    Ok(ScanReport {
        version: ScanReport::VERSION,
        corpus: request.source.describe(),
        packages_scanned,
        packages_unaudited,
        uncovered_ecosystems,
        findings,
        suppressed,
    })
}

/// Advisories in the corpus that name a package, regardless of version. Used by
/// the agent-facing check when no version is supplied.
pub fn advisories_for<'a>(
    source: &'a dyn AdvisorySource,
    package: &PackageCoordinate,
) -> Vec<&'a Advisory> {
    source.advisories_for(package)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisory::Advisory;
    use crate::severity::Priority;
    use crate::source::LocalDirSource;
    use synaptic_api::ApplicabilityState;

    const LOCK: &str = r#"
version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = ["middle"]

[[package]]
name = "middle"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = ["leaf"]

[[package]]
name = "leaf"
version = "0.9.18"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;

    const LEAF_ADVISORY: &str = r#"{
        "id": "RUSTSEC-2026-0001",
        "summary": "leaf is vulnerable",
        "severity": [
            { "type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H" }
        ],
        "affected": [
            {
                "package": { "ecosystem": "crates.io", "name": "leaf" },
                "ranges": [
                    { "type": "SEMVER", "events": [{ "introduced": "0" }, { "fixed": "0.9.20" }] }
                ]
            }
        ]
    }"#;

    fn corpus(documents: &[&str]) -> LocalDirSource {
        LocalDirSource::from_advisories(
            "test-corpus",
            documents
                .iter()
                .map(|body| Advisory::parse(body).unwrap())
                .collect(),
        )
    }

    fn graph_of(lockfile: &str) -> PackageGraph {
        PackageGraph::from_cargo_lock(lockfile).unwrap()
    }

    fn request<'a>(
        source: &'a LocalDirSource,
        usage: &'a dyn UsageOracle,
        policy: Option<&'a VulnPolicy>,
        direct: &'a [Dependency],
        packages: &'a PackageGraph,
    ) -> ScanRequest<'a> {
        ScanRequest {
            repository_identity: "test-repo",
            packages,
            direct_dependencies: direct,
            source,
            policy,
            usage,
            validation_commands: vec!["cargo test".into()],
            today: "2026-08-05".into(),
            covered_ecosystems: [Ecosystem::Cargo].into_iter().collect(),
        }
    }

    #[test]
    fn finds_a_vulnerable_transitive_dependency() {
        let source = corpus(&[LEAF_ADVISORY]);
        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            None,
            &[],
            &graph_of(LOCK),
        ))
        .unwrap();

        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].advisory_id, "RUSTSEC-2026-0001");
        assert_eq!(report.findings[0].resolved_version, "0.9.18");
    }

    #[test]
    fn reports_the_dependency_path_from_the_workspace_root() {
        let source = corpus(&[LEAF_ADVISORY]);
        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            None,
            &[],
            &graph_of(LOCK),
        ))
        .unwrap();

        let path = report.findings[0]
            .dependency_path
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        assert_eq!(
            path,
            vec![
                "cargo:app@0.1.0".to_string(),
                "cargo:middle@1.0.0".to_string(),
                "cargo:leaf@0.9.18".to_string()
            ]
        );
    }

    #[test]
    fn a_transitive_finding_without_usage_evidence_requires_review() {
        let source = corpus(&[LEAF_ADVISORY]);
        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            None,
            &[],
            &graph_of(LOCK),
        ))
        .unwrap();

        assert_eq!(
            report.findings[0].verdict.state,
            ApplicabilityState::ReviewRequired
        );
        assert_eq!(report.applicable().count(), 0);
    }

    #[test]
    fn workspace_members_are_not_scanned_as_dependencies() {
        let source = corpus(&[LEAF_ADVISORY]);
        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            None,
            &[],
            &graph_of(LOCK),
        ))
        .unwrap();

        assert_eq!(report.packages_scanned, 2, "app is a workspace member");
    }

    #[test]
    fn a_package_at_a_fixed_version_produces_no_finding() {
        let fixed_lock = LOCK.replace("0.9.18", "0.9.20");
        let source = corpus(&[LEAF_ADVISORY]);
        let fixed = graph_of(&fixed_lock);
        let report = scan(&request(&source, &NoUsageEvidence, None, &[], &fixed)).unwrap();

        assert!(report.findings.is_empty());
    }

    #[test]
    fn the_report_always_names_its_corpus() {
        let source = corpus(&[]);
        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            None,
            &[],
            &graph_of(LOCK),
        ))
        .unwrap();

        assert_eq!(report.corpus.origin, "test-corpus");
        assert_eq!(report.corpus.advisory_count, 0);
    }

    #[test]
    fn a_remediation_plan_is_attached_to_every_finding() {
        let source = corpus(&[LEAF_ADVISORY]);
        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            None,
            &[],
            &graph_of(LOCK),
        ))
        .unwrap();

        assert_eq!(
            report.findings[0]
                .remediation
                .recommended_version
                .as_deref(),
            Some("0.9.20")
        );
        assert_eq!(
            report.findings[0].remediation.validation_commands,
            vec!["cargo test".to_string()]
        );
    }

    #[test]
    fn an_unexpired_exception_suppresses_a_finding_but_records_it() {
        let source = corpus(&[LEAF_ADVISORY]);
        let id = finding_id(
            "test-repo",
            "RUSTSEC-2026-0001",
            &PackageCoordinate::new(Ecosystem::Cargo, "leaf"),
            "0.9.18",
        );
        let policy = VulnPolicy::parse(&format!(
            r#"
schema = 1

[[exception]]
finding = "{id}"
reason = "not reachable in our build"
expires = "2026-12-01"
approved_by = "security-review"
"#
        ))
        .unwrap();

        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            Some(&policy),
            &[],
            &graph_of(LOCK),
        ))
        .unwrap();

        assert!(report.findings.is_empty());
        assert_eq!(report.suppressed.len(), 1);
        assert_eq!(report.suppressed[0].approved_by, "security-review");
    }

    #[test]
    fn an_expired_exception_no_longer_suppresses() {
        let source = corpus(&[LEAF_ADVISORY]);
        let id = finding_id(
            "test-repo",
            "RUSTSEC-2026-0001",
            &PackageCoordinate::new(Ecosystem::Cargo, "leaf"),
            "0.9.18",
        );
        let policy = VulnPolicy::parse(&format!(
            r#"
schema = 1

[[exception]]
finding = "{id}"
reason = "temporary"
expires = "2026-01-01"
approved_by = "security-review"
"#
        ))
        .unwrap();

        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            Some(&policy),
            &[],
            &graph_of(LOCK),
        ))
        .unwrap();

        assert_eq!(report.findings.len(), 1);
        assert!(report.suppressed.is_empty());
    }

    #[test]
    fn a_direct_development_dependency_is_de_ranked() {
        let source = corpus(&[LEAF_ADVISORY]);
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Cargo, "leaf"),
            "Cargo.toml",
            DependencyScope::Development,
        );
        dependency.resolved_version = Some("0.9.18".into());
        let direct = vec![dependency];

        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            None,
            &direct,
            &graph_of(LOCK),
        ))
        .unwrap();

        assert!(!report.findings[0].verdict.runtime_reachable);
        assert_eq!(report.findings[0].priority, Priority::P2);
    }

    #[test]
    fn findings_are_ordered_by_priority() {
        let low = r#"{
            "id": "RUSTSEC-2026-0009",
            "severity": [{ "type": "CVSS_V3", "score": "CVSS:3.1/AV:L/AC:H/PR:H/UI:R/S:U/C:L/I:L/A:L" }],
            "affected": [
                {
                    "package": { "ecosystem": "crates.io", "name": "middle" },
                    "ranges": [{ "type": "SEMVER", "events": [{ "introduced": "0" }] }]
                }
            ]
        }"#;
        let source = corpus(&[LEAF_ADVISORY, low]);

        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            None,
            &[],
            &graph_of(LOCK),
        ))
        .unwrap();

        assert_eq!(report.findings.len(), 2);
        assert!(report.findings[0].priority <= report.findings[1].priority);
    }

    #[test]
    fn an_advisory_with_several_affected_entries_for_one_package_yields_one_finding() {
        // Advisories routinely split ranges across multiple `affected` entries
        // for the same package. Emitting one finding per entry duplicates the
        // same identity, which double-counts the queue and looks like two
        // separate problems.
        let split = r#"{
            "id": "GHSA-split",
            "affected": [
                {
                    "package": { "ecosystem": "crates.io", "name": "leaf" },
                    "ranges": [{ "type": "SEMVER", "events": [
                        { "introduced": "0" }, { "fixed": "0.9.20" }
                    ] }]
                },
                {
                    "package": { "ecosystem": "crates.io", "name": "leaf" },
                    "ranges": [{ "type": "SEMVER", "events": [
                        { "introduced": "0.9.0" }, { "fixed": "0.10.0" }
                    ] }]
                }
            ]
        }"#;
        let source = corpus(&[split]);

        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            None,
            &[],
            &graph_of(LOCK),
        ))
        .unwrap();

        assert_eq!(report.findings.len(), 1, "got {:?}", report.findings);
    }

    #[test]
    fn packages_from_an_uncovered_ecosystem_are_reported_as_unaudited() {
        // A package whose ecosystem has no corpus was not checked. Counting it
        // as scanned would let "we had no advisories" read as "we found none".
        let source = corpus(&[LEAF_ADVISORY]);
        let graph = graph_of(LOCK);
        let mut req = request(&source, &NoUsageEvidence, None, &[], &graph);
        req.covered_ecosystems = Default::default();

        let report = scan(&req).unwrap();

        assert_eq!(report.packages_scanned, 0);
        assert_eq!(report.packages_unaudited, 2);
        assert!(report.uncovered_ecosystems.contains(&Ecosystem::Cargo));
        assert!(report.findings.is_empty());
    }

    #[test]
    fn a_covered_ecosystem_reports_nothing_unaudited() {
        let source = corpus(&[LEAF_ADVISORY]);
        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            None,
            &[],
            &graph_of(LOCK),
        ))
        .unwrap();

        assert_eq!(report.packages_unaudited, 0);
        assert!(report.uncovered_ecosystems.is_empty());
    }

    #[test]
    fn a_withdrawn_advisory_produces_no_finding() {
        let withdrawn = r#"{
            "id": "RUSTSEC-2026-0010",
            "withdrawn": "2026-05-01T00:00:00Z",
            "affected": [
                {
                    "package": { "ecosystem": "crates.io", "name": "leaf" },
                    "ranges": [{ "type": "SEMVER", "events": [{ "introduced": "0" }] }]
                }
            ]
        }"#;
        let source = corpus(&[withdrawn]);

        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            None,
            &[],
            &graph_of(LOCK),
        ))
        .unwrap();

        assert!(report.findings.is_empty());
    }
}
