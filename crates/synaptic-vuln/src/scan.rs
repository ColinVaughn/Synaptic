use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use synaptic_api::{Dependency, DependencyScope, Ecosystem, PackageCoordinate};
use synaptic_core::GraphData;

use crate::advisory::Advisory;
use crate::applicability::{assess_applicability, ApplicabilityInput};
use crate::finding::{finding_id, Finding};
use crate::lockgraph::{PackageGraph, PackageKey, PackageScope, ResolvedPackage};
use crate::matching::{match_version, VersionMatch};
use crate::plan::plan_remediation;
use crate::policy::VulnPolicy;
use crate::reach::{parse_line, remediation_scope, CallSite, ImpactIndex, ReachIndex};
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

    /// Concrete places first-party code reaches the advisory's named members.
    ///
    /// Defaulted to empty so an oracle with no graph behind it, and any
    /// out-of-tree implementation, keeps compiling. Reporting no sites is
    /// always safe: sites are corroborating detail, never a dismissal.
    fn call_sites(&self, package: &PackageCoordinate, candidates: &[String]) -> Vec<CallSite> {
        let _ = (package, candidates);
        Vec::new()
    }
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

/// One stub member of a package, and what the graph says about reaching it.
#[derive(Debug, Clone, Default)]
struct MemberUsage {
    member: String,
    used: bool,
    hazard: bool,
    /// Concrete first-party locations that reach this member, deduplicated and
    /// ordered by file then line so a scan is reproducible.
    sites: Vec<CallSite>,
}

/// Reads usage signals out of a Synaptic graph.
///
/// External packages appear in the graph as SDK stub nodes labelled
/// `Sdk: <ecosystem>:<package>#<member>` with an empty source file. An incoming
/// edge from a node that does have a source file is first-party usage.
#[derive(Debug, Clone, Default)]
pub struct GraphUsageOracle {
    /// Stub members per package, keyed `<ecosystem>:<normalized name>`.
    ///
    /// Built once. The three trait methods below are each asked about the same
    /// package, and the scan asks about a package once per matching advisory,
    /// so deriving this per call meant re-deriving one answer many times: a
    /// full pass over every node, a fresh map of every node id, and a full pass
    /// over every edge, for each question. On a 13,570-node graph that measured
    /// 563 us per call, or 1.7 ms for every advisory that named a package this
    /// repository resolves.
    stubs: BTreeMap<String, Vec<MemberUsage>>,
}

impl GraphUsageOracle {
    pub fn new(graph: &GraphData) -> Self {
        // Every node by id, and every stub node's package and member. Kept in
        // node-id order so the members reported for a package are stable.
        let mut by_id: BTreeMap<&str, &synaptic_core::Node> = BTreeMap::new();
        let mut stub_nodes: BTreeMap<&str, (String, String)> = BTreeMap::new();
        for node in &graph.nodes {
            by_id.insert(node.id.0.as_str(), node);
            if !node.is_external_stub() {
                continue;
            }
            let Some((ecosystem, name, member)) = parse_sdk_label(&node.label) else {
                continue;
            };
            stub_nodes.insert(node.id.0.as_str(), (package_key(&ecosystem, &name), member));
        }

        // An edge whose source has a real source file is first-party usage.
        // Reachability is keyed on the package as well as the member, because
        // two packages can expose a member of the same name and one being
        // reached says nothing about the other.
        //
        // The edge also carries the concrete locations that produced it, which
        // is the only place a call site exists: the stub node has no file and
        // the source node names the enclosing symbol, not the line. Dropping
        // the edge here is what left findings unable to say where to look.
        let mut reached: BTreeMap<(&str, &str), (bool, bool, Vec<CallSite>)> = BTreeMap::new();
        for edge in &graph.links {
            let Some((key, member)) = stub_nodes.get(edge.target.0.as_str()) else {
                continue;
            };
            let Some(source) = by_id.get(edge.source.0.as_str()) else {
                continue;
            };
            if source.is_external_stub() {
                continue;
            }
            let entry = reached
                .entry((key.as_str(), member.as_str()))
                .or_insert_with(|| (false, false, Vec::new()));
            entry.0 = true;
            entry.1 |= source.dynamically_referenced();
            for site in edge.sites() {
                // An edge site can omit the file even when it has a line. The
                // enclosing symbol's own file is the correct fallback; without
                // either there is nothing actionable to report.
                let file = if site.source_file.is_empty() {
                    source.source_file.clone()
                } else {
                    site.source_file.clone()
                };
                if file.is_empty() {
                    continue;
                }
                entry.2.push(CallSite {
                    symbol: source.label.clone(),
                    symbol_id: source.id.0.clone(),
                    file,
                    line: parse_line(site.source_location.as_deref()),
                    member: member.clone(),
                });
            }
        }

        // Distinct (package, member) pairs, not stub NODES.
        //
        // `stub_nodes` is keyed by node id and many nodes share one pair -- a
        // measured 10x on a large TypeScript monorepo, where 115,276 stub nodes
        // collapse to 11,584 pairs. Iterating nodes and cloning the pair's whole
        // site list for each stored the same evidence once per node: on that
        // repository 449,489,960 call sites for 115,276 distinct ones, a 3,899x
        // amplification that exhausted memory before a single advisory was read.
        //
        // The duplicates were never observable: `call_sites` flat-maps every
        // matching member and `sort_call_sites` dedups, so they were built and
        // then discarded. Deduping here instead keeps the output identical.
        let mut pairs: BTreeSet<(&str, &str)> = BTreeSet::new();
        for (key, member) in stub_nodes.values() {
            pairs.insert((key.as_str(), member.as_str()));
        }

        let mut stubs: BTreeMap<String, Vec<MemberUsage>> = BTreeMap::new();
        for (key, member) in pairs {
            // Move the sites out of `reached` rather than cloning them; nothing
            // reads the entry again.
            let (used, hazard, mut sites) =
                reached
                    .remove(&(key, member))
                    .unwrap_or((false, false, Vec::new()));
            sort_call_sites(&mut sites);
            stubs.entry(key.to_string()).or_default().push(MemberUsage {
                member: member.to_string(),
                used,
                hazard,
                sites,
            });
        }
        Self { stubs }
    }

    /// SDK stub members for a package, paired with whether first-party code
    /// reaches them.
    ///
    /// Matches the package itself and any import path nested inside it. A
    /// manifest names a distributable unit while source names an importable
    /// one, and in several ecosystems those differ: Go advisories say
    /// `golang.org/x/net` where source imports `golang.org/x/net/http2/h2c`,
    /// and npm deep imports read `lodash/merge`. Containment is required to
    /// stop on a path separator, so `golang.org/x/net` does not swallow
    /// `golang.org/x/nettools`.
    fn stub_usage(&self, package: &PackageCoordinate) -> Vec<&MemberUsage> {
        let key = package_key(package.ecosystem.as_str(), &package.name);
        let nested = format!("{key}/");
        self.stubs
            // Every candidate starts with the key, so the range is a tight
            // upper bound; the filter then enforces the segment boundary.
            .range(key.clone()..)
            .take_while(|(candidate, _)| candidate.starts_with(&key))
            .filter(|(candidate, _)| **candidate == key || candidate.starts_with(&nested))
            .flat_map(|(_, members)| members.iter())
            .collect()
    }
}

/// The key a package is indexed under, folding the spelling differences that
/// separate a manifest's name from the one that appears in source.
fn package_key(ecosystem: &str, name: &str) -> String {
    format!(
        "{}:{}",
        ecosystem.trim().to_ascii_lowercase(),
        normalize_package_ident(name)
    )
}

impl UsageOracle for GraphUsageOracle {
    fn first_party_usage(&self, package: &PackageCoordinate) -> bool {
        self.stub_usage(package).iter().any(|usage| usage.used)
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
                reached
                    .iter()
                    .any(|usage| usage.used && member_matches(&usage.member, &wanted))
            })
            .cloned()
            .collect()
    }

    fn dynamic_hazard(&self, package: &PackageCoordinate) -> bool {
        self.stub_usage(package).iter().any(|usage| usage.hazard)
    }

    fn call_sites(&self, package: &PackageCoordinate, candidates: &[String]) -> Vec<CallSite> {
        let wanted: Vec<String> = candidates.iter().map(|name| final_segment(name)).collect();
        // An advisory that names no functions cannot narrow the exposure, so
        // every reached member is in scope. Narrowing to nothing instead would
        // leave most findings with no location at all, since naming functions
        // is the exception across advisory databases.
        let unnarrowed = wanted.is_empty();
        let mut sites: Vec<CallSite> = self
            .stub_usage(package)
            .iter()
            .filter(|usage| {
                usage.used
                    && (unnarrowed
                        || wanted
                            .iter()
                            .any(|candidate| member_matches(&usage.member, candidate)))
            })
            .flat_map(|usage| usage.sites.iter().cloned())
            .collect();
        sort_call_sites(&mut sites);
        sites
    }
}

/// Whether a stub member answers to an advisory-named function.
fn member_matches(member: &str, wanted_final_segment: &str) -> bool {
    final_segment(member).eq_ignore_ascii_case(wanted_final_segment)
}

/// Order call sites by location and drop exact duplicates.
///
/// Two edges can report the same line (one per relation kind), and a scan that
/// listed a location twice would overstate the exposure.
fn sort_call_sites(sites: &mut Vec<CallSite>) {
    sites.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then(left.line.cmp(&right.line))
            .then(left.symbol.cmp(&right.symbol))
            .then(left.member.cmp(&right.member))
    });
    sites.dedup();
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
    /// Reverse-reachability index, when a graph is available. `None` leaves
    /// findings without entry-point evidence rather than claiming there is
    /// none.
    pub reach: Option<&'a ReachIndex>,
    /// Reverse-impact index for forecasting an upgrade. `None` leaves the
    /// scope without a forecast rather than reporting an empty one.
    pub impact: Option<&'a ImpactIndex>,
    pub validation_commands: Vec<String>,
    /// Today's date as `YYYY-MM-DD`, used to expire policy exceptions.
    pub today: String,
    /// Ecosystems an advisory corpus was actually obtained for. Packages
    /// outside these are counted as unaudited rather than scanned.
    pub covered_ecosystems: std::collections::BTreeSet<Ecosystem>,
    /// Directly declared dependencies no enabled feature compiles, from
    /// [`crate::feature_gated_dependencies`]. Findings reached only through one
    /// of these are de-ranked, never dismissed.
    pub feature_gated: std::collections::BTreeSet<PackageCoordinate>,
}

/// Whether a dependency was declared by an SBOM rather than a manifest.
///
/// An SBOM lists a resolved dependency set, so it stands in for a lockfile in
/// ecosystems that have none.
pub fn is_sbom_source(source_file: &str) -> bool {
    let name = source_file
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(source_file);
    synaptic_api::is_sbom_manifest(name)
}

/// How completely one ecosystem was audited.
///
/// This exists so a partial audit can never be read as a complete one. A `pom.xml`
/// yields the dependencies it declares and nothing about what those pull in, and
/// reporting that next to a lockfile-resolved ecosystem without saying so would
/// overstate the scan by exactly the part nobody can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EcosystemCoverage {
    /// A lockfile or SBOM supplied the fully resolved dependency set.
    Full,
    /// Only directly declared dependencies carrying a literal version were
    /// read. Transitive dependencies were not seen at all.
    DirectOnly,
    /// No advisory corpus was available, so nothing here was checked.
    Unaudited,
}

impl std::fmt::Display for EcosystemCoverage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Full => "fully resolved",
            Self::DirectOnly => "direct declarations only",
            Self::Unaudited => "not audited",
        };
        f.write_str(text)
    }
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
    /// Packages checked from a direct declaration rather than a resolved
    /// dependency set. Counted apart from `packages_scanned` so a partial audit
    /// never reads as a complete one.
    #[serde(default)]
    pub packages_partially_audited: usize,
    /// How completely each ecosystem present in the repository was audited.
    #[serde(default)]
    pub coverage: std::collections::BTreeMap<Ecosystem, EcosystemCoverage>,
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
    let direct: BTreeMap<&PackageCoordinate, &Dependency> = request
        .direct_dependencies
        .iter()
        .map(|dependency| (&dependency.package, dependency))
        .collect();

    // Ecosystems a lockfile or SBOM already resolved in full.
    let resolved_ecosystems: std::collections::BTreeSet<Ecosystem> = request
        .packages
        .packages()
        .map(|package| package.key.coordinate.ecosystem)
        .collect();

    // Maven and Gradle without dependency locking have no lockfile to read, so
    // an ecosystem can be present in the repository and absent from the graph.
    // A declaration that pins a literal version still says what is resolved, so
    // it is promoted to a scannable package rather than being written off.
    //
    // Deliberately narrow. Promoting every manifest-declared dependency sounds
    // more thorough and is worse: a repository that merely carries a fixture
    // `package.json` would pull in npm's corpus, which is 226,000 documents, to
    // audit two packages nobody ships. So only declarations that genuinely have
    // no lockfile behind them qualify: Maven, and an SBOM, which is a resolved
    // set in its own right.
    let promoted: Vec<ResolvedPackage> = request
        .direct_dependencies
        .iter()
        .filter(|dependency| !resolved_ecosystems.contains(&dependency.package.ecosystem))
        .filter(|dependency| {
            dependency.package.ecosystem == Ecosystem::Maven
                || is_sbom_source(&dependency.source_file)
        })
        .filter_map(|dependency| {
            let version = dependency.resolved_version.as_deref()?;
            Some(ResolvedPackage {
                key: PackageKey::new(dependency.package.clone(), version),
                dependencies: Vec::new(),
                is_workspace_member: false,
                scope: match dependency.scope {
                    DependencyScope::Development => PackageScope::Development,
                    DependencyScope::Runtime => PackageScope::Runtime,
                    DependencyScope::Optional => PackageScope::Unknown,
                },
            })
        })
        .collect();

    let partial_ecosystems: std::collections::BTreeSet<Ecosystem> = promoted
        .iter()
        .map(|package| package.key.coordinate.ecosystem)
        .collect();

    let augmented;
    let graph = if promoted.is_empty() {
        request.packages
    } else {
        let mut clone = request.packages.clone();
        clone.absorb(promoted);
        augmented = clone;
        &augmented
    };

    // Coordinates a manifest declared development-only. For a format whose
    // lockfile records no dependency kind, this plus the lockfile's edges is
    // the only way to tell a test-only subtree from a shipped one.
    //
    // Only a coordinate nothing declares for runtime qualifies. In a workspace
    // the same crate is routinely a dependency of one member and a
    // dev-dependency of another, and the union of every declaration would call
    // it development-only, de-ranking it and everything reached through it.
    // That is the direction this analysis must never err in.
    let runtime_declared: std::collections::BTreeSet<&PackageCoordinate> = request
        .direct_dependencies
        .iter()
        .filter(|dependency| dependency.scope != DependencyScope::Development)
        .map(|dependency| &dependency.package)
        .collect();
    let development: std::collections::BTreeSet<PackageCoordinate> = request
        .direct_dependencies
        .iter()
        .filter(|dependency| dependency.scope == DependencyScope::Development)
        .filter(|dependency| !runtime_declared.contains(&dependency.package))
        .map(|dependency| dependency.package.clone())
        .collect();
    let runtime_reachable_keys = graph.runtime_reachable_keys(&development);
    // Reachability with the feature-gated dependencies also removed. Anything
    // that drops out between the two is reached only through code a default
    // build does not compile.
    //
    // `None` when nothing is gated, rather than a copy of the set above:
    // cloning it cost a per-scan duplicate of every resolved package's key to
    // answer a question whose answer is already known to be "nothing".
    let compiled_reachable_keys = (!request.feature_gated.is_empty()).then(|| {
        let excluded = development
            .union(&request.feature_gated)
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        graph.runtime_reachable_keys(&excluded)
    });

    let mut findings = Vec::new();
    let mut suppressed = Vec::new();
    let mut packages_scanned = 0;

    let mut packages_unaudited = 0;
    let mut packages_partially_audited = 0;
    let mut uncovered_ecosystems = std::collections::BTreeSet::new();
    let mut coverage = std::collections::BTreeMap::new();
    let mut seen_findings = std::collections::BTreeSet::new();

    for resolved in graph.scan_targets() {
        let ecosystem = resolved.key.coordinate.ecosystem;
        if !request.covered_ecosystems.contains(&ecosystem) {
            packages_unaudited += 1;
            uncovered_ecosystems.insert(ecosystem);
            coverage.insert(ecosystem, EcosystemCoverage::Unaudited);
            continue;
        }
        if partial_ecosystems.contains(&ecosystem) {
            packages_partially_audited += 1;
            coverage.insert(ecosystem, EcosystemCoverage::DirectOnly);
        } else {
            packages_scanned += 1;
            coverage.insert(ecosystem, EcosystemCoverage::Full);
        }
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
                // Where the reaching actually happens. Gathered here rather
                // than after the verdict because a `ReviewRequired` finding is
                // exactly the one a reviewer needs the locations for.
                let call_sites = request.usage.call_sites(package, &advisory_functions);
                let entry_points = request
                    .reach
                    .map(|index| index.entry_points(&call_sites))
                    .unwrap_or_default();
                // Forecast from the calling symbols, so the blast radius is the
                // one this upgrade actually carries rather than the package's
                // whole dependent set.
                let seed_ids: Vec<String> = call_sites
                    .iter()
                    .map(|site| site.symbol_id.clone())
                    .collect();
                let impact = request
                    .impact
                    .filter(|_| !seed_ids.is_empty())
                    .map(|index| index.forecast(&seed_ids));
                let scope = remediation_scope(
                    &call_sites,
                    &entry_points,
                    impact.as_ref(),
                    request.reach.is_some(),
                );
                // Reachability now comes from the resolved graph: a package is
                // development-only when the lockfile says so outright, or when
                // every path to it from a root runs through something a
                // manifest declared development-only.
                let runtime_reachable = runtime_reachable_keys.contains(&resolved.key);
                // Whether that conclusion rests on a reading or an assumption.
                let scope_recorded = resolved.scope != PackageScope::Unknown
                    || declared.is_some()
                    || !runtime_reachable;
                let feature_gated = runtime_reachable
                    && compiled_reachable_keys
                        .as_ref()
                        .is_some_and(|compiled| !compiled.contains(&resolved.key));

                let verdict = assess_applicability(&ApplicabilityInput {
                    version_match,
                    advisory_withdrawn: advisory.is_withdrawn(),
                    advisory_functions,
                    reachable_functions,
                    first_party_usage_observed: request.usage.first_party_usage(package),
                    is_direct_dependency: declared.is_some(),
                    runtime_reachable,
                    scope_recorded,
                    feature_gated,
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

                // Deduplicate before deciding what to do with the finding. The
                // same advisory can be named by several affected entries and,
                // once the local corpus is composed with the OSV API, by
                // several sources; either way it is one finding, and a
                // suppressed one must be listed once too.
                if !seen_findings.insert(id.clone()) {
                    continue;
                }

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
                    call_sites,
                    entry_points,
                    scope,
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
        packages_partially_audited,
        coverage,
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
    use crate::source::{CompositeSource, LocalDirSource};
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

    /// Same advisory, but naming the vulnerable function so reachability and
    /// call sites can be decided from it.
    const LEAF_ADVISORY_WITH_FUNCTIONS: &str = r#"{
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
                ],
                "ecosystem_specific": { "affects": { "functions": ["leaf::parse"] } }
            }
        ]
    }"#;

    /// A graph where an HTTP route reaches `leaf::parse` through one handler.
    fn reaching_graph() -> GraphData {
        let mut route = source_node("route_items", false);
        route.label = "/items".into();
        route.source_file = String::new();
        route
            .extra
            .insert("_node_type".into(), "route".to_string().into());
        let mut handler = source_node("handle_items", false);
        handler.label = "handle_items()".into();
        handler.source_file = "src/api.rs".into();

        graph_with(
            vec![
                stub_node("s_parse", "Sdk: cargo:leaf#parse"),
                route,
                handler,
            ],
            vec![
                {
                    let mut edge = edge("route_items", "handle_items");
                    edge.relation = "handled_by".into();
                    edge
                },
                edge_at("handle_items", "s_parse", "src/api.rs", Some("L31")),
            ],
        )
    }

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
            reach: None,
            impact: None,
            validation_commands: vec!["cargo test".into()],
            today: "2026-08-05".into(),
            covered_ecosystems: [Ecosystem::Cargo].into_iter().collect(),
            feature_gated: Default::default(),
        }
    }

    // ------------------------------------------------------- usage oracle
    //
    // These pin what the graph oracle answers, so the index it builds can be
    // changed without changing what a scan concludes from it.

    fn stub_node(id: &str, label: &str) -> synaptic_core::Node {
        synaptic_core::Node {
            id: synaptic_core::NodeId(id.into()),
            label: label.into(),
            file_type: synaptic_core::FileType::Code,
            // An empty source file is what marks a node as an external stub.
            source_file: String::new(),
            source_location: None,
            community: None,
            repo: None,
            extra: Default::default(),
            ..Default::default()
        }
    }

    fn source_node(id: &str, dynamic: bool) -> synaptic_core::Node {
        let mut node = synaptic_core::Node {
            id: synaptic_core::NodeId(id.into()),
            label: id.into(),
            file_type: synaptic_core::FileType::Code,
            source_file: "src/main.rs".into(),
            source_location: None,
            community: None,
            repo: None,
            extra: Default::default(),
            ..Default::default()
        };
        node.set_dynamically_referenced(dynamic);
        node
    }

    fn edge(source: &str, target: &str) -> synaptic_core::Edge {
        synaptic_core::Edge {
            source: synaptic_core::NodeId(source.into()),
            target: synaptic_core::NodeId(target.into()),
            relation: "calls".into(),
            confidence: synaptic_core::Confidence::Extracted,
            source_file: "src/main.rs".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Default::default(),
        }
    }

    fn edge_at(
        source: &str,
        target: &str,
        file: &str,
        location: Option<&str>,
    ) -> synaptic_core::Edge {
        let mut edge = edge(source, target);
        edge.source_file = file.into();
        edge.source_location = location.map(Into::into);
        edge
    }

    fn graph_with(nodes: Vec<synaptic_core::Node>, links: Vec<synaptic_core::Edge>) -> GraphData {
        GraphData {
            nodes,
            links,
            ..Default::default()
        }
    }

    #[test]
    fn a_reached_member_reports_the_call_site_that_reached_it() {
        let graph = graph_with(
            vec![
                stub_node("s1", "Sdk: cargo:serde#Value.get"),
                source_node("caller", false),
            ],
            vec![edge_at("caller", "s1", "src/handler.rs", Some("L42"))],
        );
        let oracle = GraphUsageOracle::new(&graph);

        let sites = oracle.call_sites(
            &PackageCoordinate::new(Ecosystem::Cargo, "serde"),
            &["serde::Value::get".to_string()],
        );

        assert_eq!(sites.len(), 1, "expected one call site, got {sites:?}");
        assert_eq!(sites[0].file, "src/handler.rs");
        assert_eq!(sites[0].line, Some(42));
        assert_eq!(sites[0].symbol, "caller");
        assert_eq!(sites[0].member, "Value.get");
    }

    #[test]
    fn an_advisory_naming_no_functions_reports_every_site_touching_the_package() {
        // Most advisories name no functions at all. Returning nothing here
        // would leave the overwhelming majority of findings with no location
        // to review, so an empty candidate list means "wherever it is used".
        let graph = graph_with(
            vec![
                stub_node("s1", "Sdk: cargo:serde#Value.get"),
                stub_node("s2", "Sdk: cargo:serde#from_str"),
                source_node("caller", false),
            ],
            vec![
                edge_at("caller", "s1", "src/a.rs", Some("L1")),
                edge_at("caller", "s2", "src/b.rs", Some("L2")),
            ],
        );
        let oracle = GraphUsageOracle::new(&graph);

        let sites = oracle.call_sites(&PackageCoordinate::new(Ecosystem::Cargo, "serde"), &[]);

        let files: Vec<_> = sites.iter().map(|site| site.file.as_str()).collect();
        assert_eq!(files, vec!["src/a.rs", "src/b.rs"], "got {sites:?}");
    }

    /// Several stub nodes routinely name the same (package, member) -- 115,276
    /// nodes collapsed to 11,584 pairs on a large TypeScript monorepo. The
    /// oracle must hold ONE entry per pair; holding one per node stored that
    /// pair's whole site list once per node (a measured 3,899x amplification)
    /// only for `sort_call_sites` to dedup it away later.
    #[test]
    fn stub_nodes_sharing_a_member_are_held_once_not_once_per_node() {
        let graph = graph_with(
            vec![
                stub_node("s1", "Sdk: cargo:serde#Value.get"),
                stub_node("s2", "Sdk: cargo:serde#Value.get"),
                stub_node("s3", "Sdk: cargo:serde#Value.get"),
                source_node("caller", false),
            ],
            vec![
                edge_at("caller", "s1", "src/a.rs", Some("L1")),
                edge_at("caller", "s2", "src/b.rs", Some("L2")),
                edge_at("caller", "s3", "src/c.rs", Some("L3")),
            ],
        );
        let oracle = GraphUsageOracle::new(&graph);
        let package = PackageCoordinate::new(Ecosystem::Cargo, "serde");

        let members = oracle.stub_usage(&package);
        assert_eq!(
            members.len(),
            1,
            "three stub nodes name one member; got {members:?}"
        );

        // The evidence itself is unchanged: every distinct location still shows.
        let sites = oracle.call_sites(&package, &[]);
        let files: Vec<_> = sites.iter().map(|s| s.file.as_str()).collect();
        assert_eq!(
            files,
            vec!["src/a.rs", "src/b.rs", "src/c.rs"],
            "got {sites:?}"
        );
    }

    #[test]
    fn an_unreached_package_reports_no_sites_even_with_no_named_functions() {
        let graph = graph_with(
            vec![stub_node("s1", "Sdk: cargo:serde#Value.get")],
            Vec::new(),
        );
        let oracle = GraphUsageOracle::new(&graph);

        let sites = oracle.call_sites(&PackageCoordinate::new(Ecosystem::Cargo, "serde"), &[]);

        assert!(sites.is_empty(), "unused package reported sites: {sites:?}");
    }

    #[test]
    fn a_member_the_advisory_does_not_name_reports_no_call_site() {
        let graph = graph_with(
            vec![
                stub_node("s1", "Sdk: cargo:serde#Value.get"),
                source_node("caller", false),
            ],
            vec![edge_at("caller", "s1", "src/handler.rs", Some("L42"))],
        );
        let oracle = GraphUsageOracle::new(&graph);

        let sites = oracle.call_sites(
            &PackageCoordinate::new(Ecosystem::Cargo, "serde"),
            &["serde::Value::take".to_string()],
        );

        assert!(sites.is_empty(), "unrelated member matched: {sites:?}");
    }

    #[test]
    fn call_sites_are_reported_once_per_distinct_location() {
        let graph = graph_with(
            vec![
                stub_node("s1", "Sdk: cargo:serde#Value.get"),
                source_node("caller", false),
            ],
            vec![
                edge_at("caller", "s1", "src/handler.rs", Some("L42")),
                edge_at("caller", "s1", "src/handler.rs", Some("L42")),
                edge_at("caller", "s1", "src/handler.rs", Some("L99")),
            ],
        );
        let oracle = GraphUsageOracle::new(&graph);

        let sites = oracle.call_sites(
            &PackageCoordinate::new(Ecosystem::Cargo, "serde"),
            &["Value.get".to_string()],
        );

        let lines: Vec<_> = sites.iter().map(|site| site.line).collect();
        assert_eq!(lines, vec![Some(42), Some(99)], "got {sites:?}");
    }

    #[test]
    fn a_stub_reached_only_from_another_stub_reports_no_call_site() {
        let graph = graph_with(
            vec![
                stub_node("s1", "Sdk: cargo:serde#Value.get"),
                stub_node("s2", "Sdk: cargo:other#thing"),
            ],
            vec![edge_at("s2", "s1", "", Some("L42"))],
        );
        let oracle = GraphUsageOracle::new(&graph);

        let sites = oracle.call_sites(
            &PackageCoordinate::new(Ecosystem::Cargo, "serde"),
            &["Value.get".to_string()],
        );

        assert!(
            sites.is_empty(),
            "a stub-to-stub edge is not first-party evidence: {sites:?}"
        );
    }

    #[test]
    fn a_module_matches_a_stub_under_one_of_its_import_paths() {
        // Go advisories and lockfiles name the MODULE (`golang.org/x/net`),
        // while source imports name a PACKAGE inside it
        // (`golang.org/x/net/http2/h2c`). Comparing them exactly hid every Go
        // call site: observed live in gin, where `.Handler()` calls
        // `h2c.NewHandler` at gin.go:249 against a vulnerable x/net.
        let graph = graph_with(
            vec![
                stub_node("s1", "Sdk: go:golang.org/x/net/http2/h2c#NewHandler"),
                source_node("caller", false),
            ],
            vec![edge_at("caller", "s1", "gin.go", Some("L249"))],
        );
        let oracle = GraphUsageOracle::new(&graph);
        let package = PackageCoordinate::new(Ecosystem::Go, "golang.org/x/net");

        assert!(
            oracle.first_party_usage(&package),
            "a module's import path must count as usage of the module"
        );
        let sites = oracle.call_sites(&package, &[]);
        assert_eq!(sites.len(), 1, "got {sites:?}");
        assert_eq!(sites[0].file, "gin.go");
        assert_eq!(sites[0].line, Some(249));
    }

    #[test]
    fn a_module_does_not_match_a_similarly_named_sibling_module() {
        // `golang.org/x/net` must not swallow `golang.org/x/nettools`. Only a
        // path-segment boundary counts as containment.
        let graph = graph_with(
            vec![
                stub_node("s1", "Sdk: go:golang.org/x/nettools#Dial"),
                source_node("caller", false),
            ],
            vec![edge_at("caller", "s1", "main.go", Some("L1"))],
        );
        let oracle = GraphUsageOracle::new(&graph);

        assert!(
            !oracle.first_party_usage(&PackageCoordinate::new(Ecosystem::Go, "golang.org/x/net")),
            "a prefix without a path boundary is a different module"
        );
    }

    #[test]
    fn an_exact_package_match_still_wins_when_subpaths_also_exist() {
        let graph = graph_with(
            vec![
                stub_node("s1", "Sdk: go:golang.org/x/net#Exact"),
                stub_node("s2", "Sdk: go:golang.org/x/net/http2#Sub"),
                source_node("caller", false),
            ],
            vec![
                edge_at("caller", "s1", "a.go", Some("L1")),
                edge_at("caller", "s2", "b.go", Some("L2")),
            ],
        );
        let oracle = GraphUsageOracle::new(&graph);

        let sites = oracle.call_sites(
            &PackageCoordinate::new(Ecosystem::Go, "golang.org/x/net"),
            &[],
        );

        let files: Vec<_> = sites.iter().map(|site| site.file.as_str()).collect();
        assert_eq!(files, vec!["a.go", "b.go"], "got {sites:?}");
    }

    #[test]
    fn a_stub_reached_from_first_party_code_is_first_party_usage() {
        let graph = graph_with(
            vec![
                stub_node("s1", "Sdk: cargo:serde#Value.get"),
                source_node("caller", false),
            ],
            vec![edge("caller", "s1")],
        );
        let oracle = GraphUsageOracle::new(&graph);

        assert!(oracle.first_party_usage(&PackageCoordinate::new(Ecosystem::Cargo, "serde")));
        assert!(!oracle.dynamic_hazard(&PackageCoordinate::new(Ecosystem::Cargo, "serde")));
    }

    #[test]
    fn a_stub_reached_only_from_another_stub_is_not_first_party_usage() {
        let graph = graph_with(
            vec![
                stub_node("s1", "Sdk: cargo:serde#Value.get"),
                stub_node("s2", "Sdk: cargo:other#thing"),
            ],
            vec![edge("s2", "s1")],
        );
        let oracle = GraphUsageOracle::new(&graph);

        assert!(!oracle.first_party_usage(&PackageCoordinate::new(Ecosystem::Cargo, "serde")));
    }

    #[test]
    fn a_dynamically_referenced_caller_raises_the_hazard_flag() {
        let graph = graph_with(
            vec![
                stub_node("s1", "Sdk: cargo:serde#Value.get"),
                source_node("caller", true),
            ],
            vec![edge("caller", "s1")],
        );
        let oracle = GraphUsageOracle::new(&graph);

        assert!(oracle.dynamic_hazard(&PackageCoordinate::new(Ecosystem::Cargo, "serde")));
    }

    #[test]
    fn package_names_match_across_hyphen_and_underscore_spellings() {
        // A manifest says `serde-json`; Rust source says `serde_json`.
        let graph = graph_with(
            vec![
                stub_node("s1", "Sdk: cargo:serde_json#Value.get"),
                source_node("caller", false),
            ],
            vec![edge("caller", "s1")],
        );
        let oracle = GraphUsageOracle::new(&graph);

        assert!(oracle.first_party_usage(&PackageCoordinate::new(Ecosystem::Cargo, "serde-json")));
    }

    #[test]
    fn a_stub_from_another_ecosystem_is_not_matched() {
        let graph = graph_with(
            vec![
                stub_node("s1", "Sdk: npm:serde#Value.get"),
                source_node("caller", false),
            ],
            vec![edge("caller", "s1")],
        );
        let oracle = GraphUsageOracle::new(&graph);

        assert!(!oracle.first_party_usage(&PackageCoordinate::new(Ecosystem::Cargo, "serde")));
    }

    #[test]
    fn usage_of_one_package_does_not_leak_into_another_sharing_a_member_name() {
        // Both packages expose a member called `get`. Only one is reached.
        // Anything that keys reachability on the member name alone rather than
        // on the package would report both as used.
        let graph = graph_with(
            vec![
                stub_node("used", "Sdk: cargo:reached#get"),
                stub_node("unused", "Sdk: cargo:untouched#get"),
                source_node("caller", false),
            ],
            vec![edge("caller", "used")],
        );
        let oracle = GraphUsageOracle::new(&graph);

        assert!(oracle.first_party_usage(&PackageCoordinate::new(Ecosystem::Cargo, "reached")));
        assert!(
            !oracle.first_party_usage(&PackageCoordinate::new(Ecosystem::Cargo, "untouched")),
            "an unreached package must not inherit usage from a same-named member"
        );
    }

    #[test]
    fn reachable_functions_matches_an_advisory_path_by_its_final_segment() {
        let graph = graph_with(
            vec![
                stub_node("s1", "Sdk: cargo:serde#Value.get"),
                source_node("caller", false),
            ],
            vec![edge("caller", "s1")],
        );
        let oracle = GraphUsageOracle::new(&graph);
        let candidates = vec![
            "serde::value::Value::GET".to_string(),
            "serde::other::absent".to_string(),
        ];

        let reachable = oracle.reachable_functions(
            &PackageCoordinate::new(Ecosystem::Cargo, "serde"),
            &candidates,
        );

        assert_eq!(reachable, vec!["serde::value::Value::GET".to_string()]);
    }

    #[test]
    fn an_unreached_member_yields_no_reachable_functions() {
        let graph = graph_with(
            vec![stub_node("s1", "Sdk: cargo:serde#Value.get")],
            Vec::new(),
        );
        let oracle = GraphUsageOracle::new(&graph);

        assert!(oracle
            .reachable_functions(
                &PackageCoordinate::new(Ecosystem::Cargo, "serde"),
                &["Value.get".to_string()]
            )
            .is_empty());
    }

    #[test]
    fn a_package_with_no_stub_at_all_yields_nothing() {
        let graph = graph_with(vec![source_node("caller", false)], Vec::new());
        let oracle = GraphUsageOracle::new(&graph);

        assert!(!oracle.first_party_usage(&PackageCoordinate::new(Ecosystem::Cargo, "absent")));
        assert!(!oracle.dynamic_hazard(&PackageCoordinate::new(Ecosystem::Cargo, "absent")));
    }

    #[test]
    fn a_finding_reports_the_call_site_that_reaches_the_vulnerable_function() {
        let source = corpus(&[LEAF_ADVISORY_WITH_FUNCTIONS]);
        let graph = reaching_graph();
        let oracle = GraphUsageOracle::new(&graph);
        let index = ReachIndex::new(&graph);
        let packages = graph_of(LOCK);
        let mut req = request(&source, &oracle, None, &[], &packages);
        req.reach = Some(&index);

        let report = scan(&req).unwrap();

        let finding = &report.findings[0];
        assert_eq!(finding.call_sites.len(), 1, "got {:?}", finding.call_sites);
        assert_eq!(finding.call_sites[0].file, "src/api.rs");
        assert_eq!(finding.call_sites[0].line, Some(31));
        assert_eq!(finding.call_sites[0].symbol, "handle_items()");
    }

    #[test]
    fn a_finding_reports_the_route_that_reaches_the_call_site() {
        let source = corpus(&[LEAF_ADVISORY_WITH_FUNCTIONS]);
        let graph = reaching_graph();
        let oracle = GraphUsageOracle::new(&graph);
        let index = ReachIndex::new(&graph);
        let packages = graph_of(LOCK);
        let mut req = request(&source, &oracle, None, &[], &packages);
        req.reach = Some(&index);

        let report = scan(&req).unwrap();

        let finding = &report.findings[0];
        assert_eq!(
            finding.entry_points.len(),
            1,
            "got {:?}",
            finding.entry_points
        );
        assert_eq!(finding.entry_points[0].label, "/items");
        assert_eq!(
            finding.entry_points[0].path,
            vec!["/items", "handle_items()"]
        );
    }

    #[test]
    fn a_finding_reports_the_files_its_remediation_puts_in_scope() {
        let source = corpus(&[LEAF_ADVISORY_WITH_FUNCTIONS]);
        let graph = reaching_graph();
        let oracle = GraphUsageOracle::new(&graph);
        let index = ReachIndex::new(&graph);
        let packages = graph_of(LOCK);
        let mut req = request(&source, &oracle, None, &[], &packages);
        req.reach = Some(&index);

        let report = scan(&req).unwrap();

        let scope = &report.findings[0].scope;
        assert!(scope.graph_backed);
        assert_eq!(scope.review_files, vec!["src/api.rs"]);
        assert_eq!(scope.calling_symbols, 1);
        assert_eq!(scope.exposed_entry_points, 1);
    }

    #[test]
    fn a_scan_without_a_graph_marks_its_scope_as_unmeasured() {
        // The absence of call sites here is ignorance, not a clean bill of
        // health, and the report has to be able to tell the two apart.
        let source = corpus(&[LEAF_ADVISORY_WITH_FUNCTIONS]);
        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            None,
            &[],
            &graph_of(LOCK),
        ))
        .unwrap();

        let finding = &report.findings[0];
        assert!(finding.call_sites.is_empty());
        assert!(
            !finding.scope.graph_backed,
            "an unmeasured scope must not look measured"
        );
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
    fn an_advisory_two_sources_both_carry_is_suppressed_once() {
        // Composing the local corpus with the OSV API means the same advisory
        // arrives twice. Deduplication ran after the exception check, so a
        // suppressed finding was listed once per source.
        let source = CompositeSource::new(vec![corpus(&[LEAF_ADVISORY]), corpus(&[LEAF_ADVISORY])]);
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
reason = "accepted"
expires = "2099-01-01"
approved_by = "security-review"
"#
        ))
        .unwrap();
        let graph = graph_of(LOCK);
        let request = ScanRequest {
            repository_identity: "test-repo",
            packages: &graph,
            direct_dependencies: &[],
            source: &source,
            policy: Some(&policy),
            usage: &NoUsageEvidence,
            reach: None,
            impact: None,
            validation_commands: Vec::new(),
            today: "2026-08-06".into(),
            covered_ecosystems: [Ecosystem::Cargo].into_iter().collect(),
            feature_gated: Default::default(),
        };

        let report = scan(&request).unwrap();

        assert_eq!(report.suppressed.len(), 1, "one finding, one entry");
    }

    #[test]
    fn an_advisory_two_sources_both_carry_is_reported_once() {
        let source = CompositeSource::new(vec![corpus(&[LEAF_ADVISORY]), corpus(&[LEAF_ADVISORY])]);
        let graph = graph_of(LOCK);
        let request = ScanRequest {
            repository_identity: "test-repo",
            packages: &graph,
            direct_dependencies: &[],
            source: &source,
            policy: None,
            usage: &NoUsageEvidence,
            reach: None,
            impact: None,
            validation_commands: Vec::new(),
            today: "2026-08-06".into(),
            covered_ecosystems: [Ecosystem::Cargo].into_iter().collect(),
            feature_gated: Default::default(),
        };

        let report = scan(&request).unwrap();

        assert_eq!(report.findings.len(), 1);
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
    fn a_crate_reached_only_through_a_dev_dependency_is_de_ranked() {
        // `middle` is declared as a dev dependency, and `leaf` is reached only
        // through it. Before dependency-kind propagation only `middle` itself
        // was known to be development-only, so `leaf` was ranked as though it
        // shipped. This is the Rust case the gap named.
        let source = corpus(&[LEAF_ADVISORY]);
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Cargo, "middle"),
            "Cargo.toml",
            DependencyScope::Development,
        );
        dependency.resolved_version = Some("1.0.0".into());
        let direct = vec![dependency];

        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            None,
            &direct,
            &graph_of(LOCK),
        ))
        .unwrap();

        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].package.name, "leaf");
        assert!(
            !report.findings[0].verdict.runtime_reachable,
            "leaf is reachable only through a dev dependency"
        );
        assert!(report.findings[0]
            .verdict
            .evidence
            .iter()
            .any(|item| item.kind == crate::EvidenceKind::DevelopmentOnlyDependency));
    }

    #[test]
    fn a_crate_one_member_ships_and_another_only_tests_with_stays_runtime_reachable() {
        // `middle` is a dependency of one workspace member and a
        // dev-dependency of another, which is routine in a workspace: this
        // repository does it with serde, tokio, reqwest and zip. Taking the
        // union of every dev declaration marked it development-only and dragged
        // `leaf` down with it, demoting a critical finding on a crate that
        // ships.
        let source = corpus(&[LEAF_ADVISORY]);
        let mut ships = Dependency::new(
            PackageCoordinate::new(Ecosystem::Cargo, "middle"),
            "a/Cargo.toml",
            DependencyScope::Runtime,
        );
        ships.resolved_version = Some("1.0.0".into());
        let mut tests_with = Dependency::new(
            PackageCoordinate::new(Ecosystem::Cargo, "middle"),
            "b/Cargo.toml",
            DependencyScope::Development,
        );
        tests_with.resolved_version = Some("1.0.0".into());
        let direct = vec![ships, tests_with];

        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            None,
            &direct,
            &graph_of(LOCK),
        ))
        .unwrap();

        assert_eq!(report.findings.len(), 1);
        assert!(
            report.findings[0].verdict.runtime_reachable,
            "one member ships it, so it ships"
        );
        assert!(
            !report.findings[0]
                .verdict
                .evidence
                .iter()
                .any(|item| item.kind == crate::EvidenceKind::DevelopmentOnlyDependency),
            "a crate that ships is not a development-only dependency"
        );
    }

    #[test]
    fn a_finding_says_when_the_lockfile_recorded_no_dependency_kind() {
        // The honest counterpart to the test above. Nothing declared a scope
        // here, and Cargo.lock records none, so the scan is assuming runtime
        // reachability rather than reading it. The finding has to say so, or an
        // absence of evidence reads as evidence.
        let source = corpus(&[LEAF_ADVISORY]);

        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            None,
            &[],
            &graph_of(LOCK),
        ))
        .unwrap();

        assert!(report.findings[0].verdict.runtime_reachable);
        assert!(
            report.findings[0]
                .verdict
                .evidence
                .iter()
                .any(|item| item.kind == crate::EvidenceKind::DependencyScopeUnrecorded),
            "an assumed scope must be labelled as assumed"
        );
    }

    #[test]
    fn a_crate_reached_only_through_a_disabled_feature_is_de_ranked_not_dismissed() {
        // `middle` is an optional dependency no default feature enables, and
        // `leaf` is reached only through it. Nothing here compiles, but the
        // finding must survive: a feature this build leaves off is still a
        // feature someone else turns on.
        let source = corpus(&[LEAF_ADVISORY]);
        let graph = graph_of(LOCK);
        let mut request = request(&source, &NoUsageEvidence, None, &[], &graph);
        request.feature_gated = [PackageCoordinate::new(Ecosystem::Cargo, "middle")]
            .into_iter()
            .collect();

        let report = scan(&request).unwrap();

        assert_eq!(report.findings.len(), 1, "the finding is not dismissed");
        assert_ne!(
            report.findings[0].verdict.state,
            ApplicabilityState::NotApplicable
        );
        assert!(report.findings[0]
            .verdict
            .evidence
            .iter()
            .any(|item| item.kind == crate::EvidenceKind::FeatureGated));
    }

    #[test]
    fn a_crate_a_default_feature_compiles_carries_no_feature_gate_evidence() {
        let source = corpus(&[LEAF_ADVISORY]);

        let report = scan(&request(
            &source,
            &NoUsageEvidence,
            None,
            &[],
            &graph_of(LOCK),
        ))
        .unwrap();

        assert!(!report.findings[0]
            .verdict
            .evidence
            .iter()
            .any(|item| item.kind == crate::EvidenceKind::FeatureGated));
    }

    const MAVEN_ADVISORY: &str = r#"{
        "id": "GHSA-maven-0001",
        "summary": "log4j-core is vulnerable",
        "severity": [
            { "type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:C/C:H/I:H/A:H" }
        ],
        "affected": [
            {
                "package": { "ecosystem": "Maven", "name": "org.apache.logging.log4j:log4j-core" },
                "ranges": [
                    { "type": "ECOSYSTEM", "events": [{ "introduced": "0" }, { "fixed": "2.17.1" }] }
                ]
            }
        ]
    }"#;

    fn maven_dependency(version: Option<&str>) -> Dependency {
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Maven, "org.apache.logging.log4j:log4j-core"),
            "pom.xml",
            DependencyScope::Runtime,
        );
        dependency.resolved_version = version.map(str::to_string);
        dependency
    }

    #[test]
    fn a_declared_dependency_is_scanned_when_its_ecosystem_has_no_lockfile() {
        // Maven has no lockfile unless dependency locking is switched on, so
        // before this the whole ecosystem landed in the unaudited count. A pom
        // that pins a literal version still says exactly what is resolved.
        let source = corpus(&[MAVEN_ADVISORY]);
        let direct = vec![maven_dependency(Some("2.14.1"))];
        let graph = graph_of(LOCK);
        let mut request = request(&source, &NoUsageEvidence, None, &direct, &graph);
        request.covered_ecosystems = [Ecosystem::Cargo, Ecosystem::Maven].into_iter().collect();

        let report = scan(&request).unwrap();

        assert!(report
            .findings
            .iter()
            .any(|finding| finding.package.name == "org.apache.logging.log4j:log4j-core"));
    }

    #[test]
    fn a_directly_declared_ecosystem_is_reported_as_partially_covered() {
        // The accounting is the point. A pom gives direct declarations only, so
        // calling that "scanned" alongside a lockfile would overstate it.
        let source = corpus(&[MAVEN_ADVISORY]);
        let direct = vec![maven_dependency(Some("2.14.1"))];
        let graph = graph_of(LOCK);
        let mut request = request(&source, &NoUsageEvidence, None, &direct, &graph);
        request.covered_ecosystems = [Ecosystem::Cargo, Ecosystem::Maven].into_iter().collect();

        let report = scan(&request).unwrap();

        assert_eq!(
            report.coverage.get(&Ecosystem::Maven),
            Some(&EcosystemCoverage::DirectOnly)
        );
        assert_eq!(
            report.coverage.get(&Ecosystem::Cargo),
            Some(&EcosystemCoverage::Full)
        );
        assert_eq!(
            report.packages_partially_audited, 1,
            "the maven package is counted apart from the fully resolved ones"
        );
        assert_eq!(
            report.packages_scanned, 2,
            "packages scanned stays the lockfile-resolved count"
        );
    }

    #[test]
    fn a_declared_dependency_without_a_resolved_version_is_not_scanned() {
        // A pom whose version is an unresolved property says nothing about what
        // is installed, and a version match needs a version.
        let source = corpus(&[MAVEN_ADVISORY]);
        let direct = vec![maven_dependency(None)];
        let graph = graph_of(LOCK);
        let mut request = request(&source, &NoUsageEvidence, None, &direct, &graph);
        request.covered_ecosystems = [Ecosystem::Cargo, Ecosystem::Maven].into_iter().collect();

        let report = scan(&request).unwrap();

        assert!(report
            .findings
            .iter()
            .all(|finding| finding.package.ecosystem != Ecosystem::Maven));
        assert_eq!(report.packages_partially_audited, 0);
    }

    #[test]
    fn a_declaration_does_not_duplicate_a_package_its_lockfile_already_resolved() {
        // Cargo.toml declares what Cargo.lock resolves. Promoting declarations
        // for an ecosystem that already has a lockfile would scan it twice and
        // inflate every count in the report.
        let source = corpus(&[LEAF_ADVISORY]);
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Cargo, "leaf"),
            "Cargo.toml",
            DependencyScope::Runtime,
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

        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.packages_partially_audited, 0);
        assert_eq!(
            report.coverage.get(&Ecosystem::Cargo),
            Some(&EcosystemCoverage::Full)
        );
    }

    #[test]
    fn an_ecosystem_without_a_corpus_is_reported_as_unaudited_not_partial() {
        let source = corpus(&[MAVEN_ADVISORY]);
        let direct = vec![maven_dependency(Some("2.14.1"))];
        let graph = graph_of(LOCK);
        // Only cargo has a corpus.
        let request = request(&source, &NoUsageEvidence, None, &direct, &graph);

        let report = scan(&request).unwrap();

        assert_eq!(
            report.coverage.get(&Ecosystem::Maven),
            Some(&EcosystemCoverage::Unaudited)
        );
        assert!(report.uncovered_ecosystems.contains(&Ecosystem::Maven));
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
