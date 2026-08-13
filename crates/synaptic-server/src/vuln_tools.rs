//! Agent-facing vulnerability tools.
//!
//! These expose the dependency-safety guardrail and the findings ledger to
//! assistants so generated code does not reach for a known-vulnerable version.
//!
//! The ledger tools read local state only. `vuln_check_dependency` asks OSV
//! about the one package it was given, unless an operator configured a corpus
//! or set `SYNAPTIC_OFFLINE=1`, in which case it reads that instead.
//! `vuln_scan` stays local by default and reaches OSV only when its explicit
//! `online` argument is true.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use synaptic_core::{EdgeSiteAccumulator, GraphData};
use synaptic_graph::KnowledgeGraph;
use synaptic_vuln::{
    AdvisorySource, CompositeSource, CorpusCache, DecisionKind, Ecosystem, FindingStore,
    GraphUsageOracle, ImpactIndex, LocalDirSource, NoUsageEvidence, PackageCoordinate,
    PackageGraph, ReachIndex, ScanRequest, SystemOsvTransport, UsageOracle, VulnPolicy,
    check_dependency, decision, discover_repository_files, feature_gated_in, is_sbom_source,
    repair_inputs, scan,
};

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
            );
        }
        Err(error) => {
            return (
                format!("Finding {finding} could not be read: {error}"),
                json!({ "error": "unreadable_finding" }),
            );
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

/// Scan the repository through the MCP server's trusted graph path.
///
/// This deliberately makes network use opt-in. A whole-repository scan reveals
/// the dependency set to OSV, whereas the default local-corpus path does not.
/// Recording is separate for the same reason: it is an auditable filesystem
/// mutation, never an incidental effect of inspecting a repository.
pub(crate) fn scan_tool(
    root: &Path,
    graph_path: Option<&Path>,
    repo: Option<&str>,
    record: bool,
    online: bool,
) -> Result<(String, Value), String> {
    let discovered = discover_repository_files(root);
    let (packages, reads) = PackageGraph::from_lockfiles(root, &discovered.lockfiles);
    if reads.is_empty() {
        return Err(format!(
            "No supported lockfile was found under {}. vuln_scan cannot audit unpinned dependencies. Generate or commit a supported lockfile and retry; until then, use vuln_check_dependency for each dependency you intend to add or upgrade.",
            root.display()
        ));
    }
    let direct = synaptic_api::scan_dependencies(root)
        .map_err(|error| format!("Cannot inventory direct dependencies: {error}"))?;

    let mut ecosystems = reads
        .iter()
        .filter(|read| read.error.is_none())
        .map(|read| read.kind.ecosystem())
        .collect::<BTreeSet<_>>();
    // Maven declarations and SBOMs are scan targets despite their lack of a
    // conventional lockfile. This mirrors the CLI promotion rule exactly.
    let declared_only = direct
        .iter()
        .filter(|dependency| dependency.resolved_version.is_some())
        .filter(|dependency| {
            dependency.package.ecosystem == Ecosystem::Maven
                || is_sbom_source(&dependency.source_file)
        })
        .map(|dependency| dependency.package.ecosystem)
        .filter(|ecosystem| !ecosystems.contains(ecosystem))
        .collect::<BTreeSet<_>>();
    ecosystems.extend(declared_only);

    let mut source = CompositeSource::default();
    let mut covered_ecosystems = BTreeSet::new();
    if let Some(directory) = advisory_dir(root) {
        let local = LocalDirSource::load(&directory).map_err(|error| {
            format!(
                "Cannot load advisory corpus at {}: {error}",
                directory.display()
            )
        })?;
        source.push(local);
        // An explicitly configured corpus is trusted to cover the repository's
        // ecosystems, just as CLI `--advisories` is.
        covered_ecosystems = ecosystems.clone();
    } else if let Some(cache) = CorpusCache::user_default() {
        for ecosystem in &ecosystems {
            let Some(directory) = cache.resolve(*ecosystem) else {
                continue;
            };
            if let Ok(local) = LocalDirSource::load(&directory) {
                source.push(local);
                covered_ecosystems.insert(*ecosystem);
            }
        }
    }

    let mut online_error = None;
    if online && synaptic_vuln::offline_forced() {
        online_error = Some("SYNAPTIC_OFFLINE=1 disabled the requested OSV lookup".into());
    } else if online {
        let coordinates = scan_coordinates(&packages, &direct);
        match SystemOsvTransport::new()
            .map_err(|error| error.to_string())
            .and_then(|transport| {
                synaptic_vuln::fetch_advisories(
                    &transport,
                    &coordinates,
                    CorpusCache::user_default()
                        .map(|cache| cache.live_dir())
                        .as_deref(),
                )
                .map_err(|error| error.to_string())
            }) {
            Ok(live) => {
                source.push(live);
                covered_ecosystems.extend(
                    ecosystems.iter().copied().filter(|ecosystem| {
                        synaptic_vuln::osv_ecosystem_name(*ecosystem).is_some()
                    }),
                );
            }
            Err(error) => online_error = Some(error),
        }
    }
    if source.is_empty() {
        return Err(
            "No advisory corpus is available. Configure SYNAPTIC_VULN_ADVISORIES, add .synaptic/vuln/advisories, or run `synaptic vuln sync`. An empty corpus is not a clean scan."
                .into(),
        );
    }

    let corpus_names_something = packages.scan_targets().any(|resolved| {
        !AdvisorySource::advisories_for(&source, &resolved.key.coordinate).is_empty()
    }) || direct.iter().any(|dependency| {
        dependency.resolved_version.is_some()
            && !AdvisorySource::advisories_for(&source, &dependency.package).is_empty()
    });
    let graph_data = if corpus_names_something {
        let path = graph_path.ok_or_else(|| {
            "vuln_scan needs the graph it was served with; start the MCP server from <root>/synaptic-out/graph.json".to_string()
        })?;
        load_graph_data(path, repo)?
    } else {
        None
    };
    let graph_oracle = graph_data.as_ref().map(GraphUsageOracle::new);
    let usage: &dyn UsageOracle = graph_oracle
        .as_ref()
        .map(|oracle| oracle as &dyn UsageOracle)
        .unwrap_or(&NoUsageEvidence);
    let reach_index = graph_data.as_ref().map(ReachIndex::new);
    let impact_index =
        graph_data.map(|data| ImpactIndex::new(KnowledgeGraph::from_graph_data(data)));
    let policy = VulnPolicy::load(root)
        .map_err(|error| format!("Cannot load vulnerability policy: {error}"))?;
    let identity = repository_identity(root);
    let report = scan(&ScanRequest {
        repository_identity: &identity,
        packages: &packages,
        direct_dependencies: &direct,
        source: &source,
        policy: policy.as_ref(),
        usage,
        reach: reach_index.as_ref(),
        impact: impact_index.as_ref(),
        validation_commands: validation_commands(root),
        today: today(),
        covered_ecosystems,
        feature_gated: feature_gated_in(&discovered.cargo_manifests),
    })
    .map_err(|error| format!("Vulnerability scan failed: {error}"))?;

    if record {
        let store = FindingStore::new(root);
        let digest = policy.as_ref().map(VulnPolicy::digest).unwrap_or_default();
        let base = base_sha(root);
        for finding in &report.findings {
            let existed = store
                .get(&finding.id)
                .map_err(|error| format!("Cannot read finding ledger: {error}"))?
                .is_some();
            store
                .upsert(
                    finding,
                    &identity,
                    &base,
                    &digest,
                    decision(
                        if existed {
                            DecisionKind::Redetected
                        } else {
                            DecisionKind::Detected
                        },
                        "synaptic MCP vuln_scan",
                        format!("{} at {}", finding.advisory_id, finding.resolved_version),
                    ),
                )
                .map_err(|error| format!("Cannot record finding: {error}"))?;
        }
    }

    let mut text = format!(
        "Vulnerability scan: {} package(s) scanned; {} finding(s), {} applicable.",
        report.packages_scanned,
        report.findings.len(),
        report.applicable().count(),
    );
    text.push_str(&format!(
        " Advisory source: {}. An empty result reflects this corpus, not proof the repository is clean.",
        report.corpus.origin
    ));
    if record {
        text.push_str(" Findings were recorded in the audit ledger.");
    } else {
        text.push_str(
            " This result was not recorded; pass record=true before requesting vuln_brief.",
        );
    }
    if report.packages_unaudited > 0 {
        text.push_str(&format!(
            " WARNING: {} package(s) were not audited because their ecosystem has no corpus.",
            report.packages_unaudited
        ));
    }
    if let Some(error) = &online_error {
        text.push_str(&format!(
            " WARNING: online OSV lookup failed ({error}); local corpora were used."
        ));
    }
    Ok((
        text,
        json!({
            "report": report,
            "recorded": record,
            "online": online,
            "online_error": online_error,
        }),
    ))
}

/// Convert a recorded, proven vulnerability into the bounded repair brief an
/// agent needs to generate a patch without inventing scope or verification.
pub(crate) fn brief_tool(
    root: &Path,
    graph_path: Option<&Path>,
    repo: Option<&str>,
    finding_id: &str,
) -> Result<(String, Value), String> {
    let record = FindingStore::new(root)
        .get(finding_id)
        .map_err(|error| format!("Cannot read finding ledger: {error}"))?
        .ok_or_else(|| {
            format!(
                "Finding {finding_id} is not in the ledger. Run vuln_scan with record=true first."
            )
        })?;
    let inputs = repair_inputs(&record.finding, record.created_at).ok_or_else(|| {
        format!(
            "{finding_id} has no fixed upgrade target ({:?}); use a removal, replacement, or mitigation workflow instead.",
            record.finding.remediation.kind
        )
    })?;
    if inputs.assessment.state != synaptic_api::ApplicabilityState::Applicable {
        return Err(format!(
            "{finding_id} is {:?}, not Applicable. The repair brief refuses to patch an unproven finding.",
            inputs.assessment.state
        ));
    }
    let path = graph_path.ok_or_else(|| {
        "vuln_brief needs the graph it was served with; start the MCP server from <root>/synaptic-out/graph.json".to_string()
    })?;
    let data = load_graph_data(path, repo)?.ok_or_else(|| {
        "The current advisory corpus names no packages, so no graph-backed finding can be handed off.".to_string()
    })?;
    let repository_base = base_sha(root);
    let base_sha = if repository_base.is_empty() {
        data.built_at_commit
            .clone()
            .unwrap_or_else(|| "working-tree".into())
    } else {
        repository_base
    };
    let graph = KnowledgeGraph::from_graph_data(data);
    let identity = repository_identity(root);
    let brief = synaptic_api::build_repair_brief(synaptic_api::RepairBriefRequest {
        repository_root: root,
        repository_identity: &identity,
        base_sha: &base_sha,
        event: &inputs.event,
        assessment: &inputs.assessment,
        graph: &graph,
        memory: &[],
        budget: &synaptic_api::BriefBudget::default(),
    })
    .map_err(|error| format!("Cannot build repair brief: {error}"))?;
    let text = format!(
        "Repair brief {} for {}: {} -> {}; {} binding(s), {} permitted file(s), {} required test(s).",
        brief.id,
        record.finding.advisory_id,
        record.finding.resolved_version,
        inputs.event.release.as_deref().unwrap_or("unknown"),
        brief.usage_bindings.len(),
        brief.allowed_files.len(),
        brief.required_tests.len(),
    );
    Ok((
        text,
        serde_json::to_value(brief).expect("repair brief is serializable"),
    ))
}

fn scan_coordinates(
    packages: &PackageGraph,
    direct: &[synaptic_api::Dependency],
) -> Vec<PackageCoordinate> {
    packages
        .packages()
        .map(|package| package.key.coordinate.clone())
        .chain(
            direct
                .iter()
                .filter(|dependency| dependency.resolved_version.is_some())
                .map(|dependency| dependency.package.clone()),
        )
        .collect()
}

/// Read the immutable graph snapshot the MCP server was started with.
fn load_graph_data(path: &Path, repo: Option<&str>) -> Result<Option<GraphData>, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("Cannot read graph {}: {error}", path.display()))?;
    let graph: GraphData = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Cannot parse graph {}: {error}", path.display()))?;
    Ok(Some(match repo {
        Some(tag) => scope_graph_to_repo(graph, tag),
        None => graph,
    }))
}

/// Restrict a federated graph to one member and rewrite its repo-prefixed paths
/// back to paths relative to that member's physical checkout.
///
/// Shared external SDK stubs are retained when a selected first-party node
/// reaches them, even when external dedup assigned the stub another member's
/// `repo` tag. Cross-repo first-party nodes are excluded: their files live under
/// another jail and cannot be part of this member's patch brief.
fn scope_graph_to_repo(mut graph: GraphData, tag: &str) -> GraphData {
    let selected = graph
        .nodes
        .iter()
        .filter(|node| node.repo.as_deref() == Some(tag) && !node.is_external_stub())
        .map(|node| node.id.0.clone())
        .collect::<BTreeSet<_>>();
    let external = graph
        .nodes
        .iter()
        .filter(|node| node.is_external_stub())
        .map(|node| node.id.0.clone())
        .collect::<BTreeSet<_>>();

    graph.links.retain(|edge| {
        let source_selected = selected.contains(&edge.source.0);
        let target_selected = selected.contains(&edge.target.0);
        (source_selected && (target_selected || external.contains(&edge.target.0)))
            || (target_selected && (source_selected || external.contains(&edge.source.0)))
    });
    let mut kept = selected;
    for edge in &graph.links {
        kept.insert(edge.source.0.clone());
        kept.insert(edge.target.0.clone());
    }
    graph.nodes.retain(|node| kept.contains(&node.id.0));
    graph
        .hyperedges
        .retain(|hyperedge| hyperedge.nodes.iter().all(|node| kept.contains(&node.0)));

    for node in &mut graph.nodes {
        strip_repo_prefix(&mut node.source_file, tag);
    }
    for edge in &mut graph.links {
        let mut aggregated_sites = edge
            .extra
            .contains_key("sites")
            .then(|| EdgeSiteAccumulator::new(edge));
        strip_repo_prefix(&mut edge.source_file, tag);
        if let Some(sites) = &mut aggregated_sites {
            sites.rewrite(|site| strip_repo_prefix(&mut site.source_file, tag));
        }
        if let Some(sites) = aggregated_sites {
            sites.apply_to(edge);
        }
    }
    graph
}

fn strip_repo_prefix(path: &mut String, tag: &str) {
    if let Some(relative) = path.strip_prefix(&format!("{tag}/")) {
        *path = relative.to_string();
    }
}

fn repository_identity(root: &Path) -> String {
    let remote = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
    remote
        .as_deref()
        .and_then(normalize_remote_identity)
        .unwrap_or_else(|| {
            root.canonicalize()
                .unwrap_or_else(|_| root.to_path_buf())
                .to_string_lossy()
                .into_owned()
        })
}

fn normalize_remote_identity(url: &str) -> Option<String> {
    let without_scheme = url
        .trim()
        .split_once("://")
        .map_or(url.trim(), |(_, rest)| rest);
    let without_user = without_scheme
        .rsplit_once('@')
        .map_or(without_scheme, |(_, rest)| rest);
    let normalized = without_user.replacen(':', "/", 1);
    let without_trailing_slash = normalized.trim_end_matches('/');
    let trimmed = without_trailing_slash
        .strip_suffix(".git")
        .unwrap_or(without_trailing_slash);
    let segments = trimmed
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    (segments.len() >= 3).then(|| segments.join("/"))
}

fn base_sha(root: &Path) -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
}

fn validation_commands(root: &Path) -> Vec<String> {
    root.join("Cargo.toml")
        .exists()
        .then(|| "cargo test --workspace --all-features --locked".to_string())
        .into_iter()
        .collect()
}

fn today() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default();
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
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
    fn a_scan_without_a_lockfile_gives_an_agent_a_safe_next_step() {
        let dir = tempfile::tempdir().unwrap();

        let error = scan_tool(dir.path(), None, None, false, false).unwrap_err();

        assert!(
            error.contains("cannot audit unpinned dependencies"),
            "{error}"
        );
        assert!(
            error.contains("vuln_check_dependency"),
            "the fallback must be discoverable to an agent: {error}"
        );
    }

    #[test]
    fn an_unknown_finding_is_reported_as_unknown() {
        let dir = tempfile::tempdir().unwrap();

        let (_, structured) = explain_tool(dir.path(), "vuln_finding_missing");

        assert_eq!(structured["error"], "unknown_finding");
    }
}
