//! Dependency vulnerability commands.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use synaptic_api::{Ecosystem, PackageCoordinate};
use synaptic_vuln::{
    check_dependency, decision, discover_repository_files, feature_gated_in, is_sbom_source,
    repair_inputs, scan, sync_ecosystem, AdvisorySource, CompositeSource, CorpusCache,
    DecisionKind, EcosystemCoverage, Finding, FindingState, FindingStore, GraphUsageOracle,
    ImpactIndex, LocalDirSource, LockfileKind, NoUsageEvidence, PackageGraph, Priority, ReachIndex,
    ScanReport, ScanRequest, SystemCorpusFetcher, SystemOsvTransport, UsageOracle, VulnPolicy,
    DEFAULT_MAX_DOWNLOAD_BYTES, DEFAULT_POLICY_PATH, DEFAULT_STALE_AFTER_SECONDS,
};

use crate::cli::VulnAction;
use crate::commands::common::load_graph_data;

pub(crate) fn run_vuln(action: VulnAction) -> Result<()> {
    match action {
        VulnAction::Init { root } => run_init(&root),
        VulnAction::Scan {
            root,
            advisories,
            offline,
            online,
            graph,
            json,
            fail_on,
            record,
        } => run_scan(
            &root,
            advisories.as_deref(),
            offline,
            online,
            graph,
            json,
            fail_on.as_deref(),
            record,
        ),
        VulnAction::Findings { root, json, state } => run_findings(&root, json, state.as_deref()),
        VulnAction::Explain {
            finding,
            root,
            json,
        } => run_explain(&finding, &root, json),
        VulnAction::Brief {
            finding,
            root,
            graph,
            json,
        } => run_brief(&finding, &root, graph, json),
        VulnAction::Repair {
            finding,
            root,
            graph,
            dry_run,
            agent_command,
            candidate,
            repository_identity,
            network_guard,
            json,
        } => run_repair(
            &finding,
            &root,
            graph,
            dry_run,
            agent_command.as_deref(),
            candidate.as_deref(),
            repository_identity.as_deref(),
            network_guard,
            json,
            true,
        )
        .map(|_| ()),
        VulnAction::Verify { run, root, json } => run_verify(&run, &root, json),
        VulnAction::Publish {
            run,
            root,
            provider,
            provider_base_url,
            repository,
            target_branch,
            json,
        } => run_publish(
            &run,
            &root,
            json,
            crate::commands::api::PublishOptions {
                provider,
                provider_base_url,
                repository,
                target_branch,
            },
        ),
        VulnAction::Run {
            finding,
            root,
            graph,
            dry_run,
            agent_command,
            network_guard,
            defer_publish,
            provider,
            provider_base_url,
            repository,
            target_branch,
            json,
        } => run_composed(
            &finding,
            &root,
            graph,
            dry_run,
            agent_command.as_deref(),
            network_guard,
            defer_publish,
            crate::commands::api::PublishOptions {
                provider,
                provider_base_url,
                repository,
                target_branch,
            },
            json,
        ),
        VulnAction::ExportRun {
            run,
            root,
            output,
            json,
        } => run_export(&run, &root, &output, json),
        VulnAction::ImportRun {
            bundle,
            expected_digest,
            root,
            json,
        } => run_import(&bundle, &expected_digest, &root, json),
        VulnAction::Sync {
            ecosystem,
            max_bytes,
            cache,
        } => run_sync(&ecosystem, max_bytes, cache),
        VulnAction::Check {
            package,
            version,
            advisories,
            offline,
            root,
            json,
        } => run_check(
            &package,
            version.as_deref(),
            advisories.as_deref(),
            offline,
            &root,
            json,
        ),
        VulnAction::Accept {
            finding,
            root,
            reason,
            until,
            approved_by,
        } => run_accept(&finding, &root, &reason, &until, &approved_by),
    }
}

const POLICY_TEMPLATE: &str = r#"# Synaptic vulnerability policy.
#
# `deny` refuses a package outright. `pin` sets a minimum version. `exception`
# accepts a finding's risk until a date that must be supplied: an accepted risk
# is never permanent by default.
schema = 1

# [[deny]]
# package = "npm:request"
# reason = "unmaintained"
# replacement = "npm:undici"

# [[pin]]
# package = "cargo:example-crate"
# minimum = "0.10.66"
# reason = "organisation floor"

# [[exception]]
# finding = "vuln_finding_..."
# reason = "vulnerable path is not reachable in this build"
# expires = "2026-12-01"
# approved_by = "security-review"
"#;

fn run_init(root: &Path) -> Result<()> {
    let path = root.join(DEFAULT_POLICY_PATH);
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::write(&path, POLICY_TEMPLATE)
        .with_context(|| format!("cannot write {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

/// Resolve the advisory corpus, fetching it on first use unless told not to.
///
/// Order: an explicit `--advisories` directory wins; otherwise the shared
/// user-level cache is used, synced when it is absent or stale. `--offline`
/// never fetches, and says so rather than reporting a clean scan against
/// nothing.
fn resolve_source(
    explicit: Option<&Path>,
    offline: bool,
    ecosystem: Ecosystem,
) -> Result<LocalDirSource> {
    if let Some(directory) = explicit {
        return LocalDirSource::load(directory)
            .with_context(|| format!("cannot load advisories from {}", directory.display()));
    }

    let cache = CorpusCache::user_default()
        .context("cannot locate a home directory for the advisory cache; pass --advisories")?;
    let container = cache.ecosystem_dir(ecosystem);
    let now = unix_now();
    let stale = cache.needs_sync(ecosystem, now, DEFAULT_STALE_AFTER_SECONDS);

    if stale && !offline {
        eprintln!(
            "[synaptic] fetching the {ecosystem} advisory corpus into {}",
            container.display()
        );
        match sync_ecosystem(
            &cache,
            &SystemCorpusFetcher,
            ecosystem,
            DEFAULT_MAX_DOWNLOAD_BYTES,
        ) {
            Ok(metadata) => eprintln!(
                "[synaptic] {} advisories, upstream {}",
                metadata.advisory_count,
                metadata.last_modified.as_deref().unwrap_or("unknown")
            ),
            Err(error) if cache.resolve(ecosystem).is_some() => {
                eprintln!("[synaptic] corpus refresh failed ({error}); using the cached copy");
            }
            Err(error) => {
                return Err(anyhow::anyhow!(error)).context(
                    "no advisory corpus is available; run `synaptic vuln sync`, pass \
                     --advisories <dir>, or fix connectivity",
                )
            }
        }
    }

    // Read the generation the cache is currently serving, never the container:
    // retired generations may still be on disk, and loading the container would
    // read all of them at once.
    let Some(directory) = cache.resolve(ecosystem) else {
        bail!(
            "no advisory corpus at {}. Run `synaptic vuln sync` (or pass --advisories <dir>). \
             Refusing to report a scan against an empty corpus.",
            container.display()
        );
    };
    if stale && offline {
        eprintln!(
            "[synaptic] WARNING: the cached corpus is stale and --offline forbids refreshing it"
        );
    }
    LocalDirSource::load(&directory)
        .with_context(|| format!("cannot load advisories from {}", directory.display()))
}

/// Ask the OSV API about a set of packages.
///
/// Documents are cached under the shared advisory cache so a loop of checks
/// does not re-download what it already has; the batch query still runs every
/// time, so a newly published advisory is never missed.
fn live_source(coordinates: &[PackageCoordinate]) -> Result<LocalDirSource> {
    let transport = SystemOsvTransport::new()?;
    let cache = CorpusCache::user_default().map(|cache| cache.live_dir());
    Ok(synaptic_vuln::fetch_advisories(
        &transport,
        coordinates,
        cache.as_deref(),
    )?)
}

/// Resolve one corpus per ecosystem the repository actually locks.
///
/// An ecosystem whose corpus cannot be obtained is reported and skipped rather
/// than failing the whole scan, so a repository with one exotic ecosystem still
/// gets audited for the others. Skipping is announced loudly: unaudited is not
/// the same as clean.
///
/// `may_be_empty` is set when the caller has another source lined up. The bulk
/// export for a large ecosystem is refused outright for exceeding the download
/// limit, which is precisely the case `--online` exists to cover, so failing
/// here would make that flag unreachable on the repositories that need it. The
/// caller still refuses to scan against nothing.
fn resolve_sources(
    explicit: Option<&Path>,
    offline: bool,
    ecosystems: &BTreeSet<Ecosystem>,
    may_be_empty: bool,
) -> Result<(CompositeSource, BTreeSet<Ecosystem>)> {
    if let Some(directory) = explicit {
        let source = LocalDirSource::load(directory)
            .with_context(|| format!("cannot load advisories from {}", directory.display()))?;
        // An explicitly supplied corpus is taken to cover whatever is locked;
        // the operator chose it deliberately.
        return Ok((CompositeSource::new(vec![source]), ecosystems.clone()));
    }

    let mut composite = CompositeSource::default();
    let mut covered = BTreeSet::new();
    let mut skipped = Vec::new();
    for ecosystem in ecosystems {
        match resolve_source(None, offline, *ecosystem) {
            Ok(source) => {
                composite.push(source);
                covered.insert(*ecosystem);
            }
            Err(error) => skipped.push(format!("{ecosystem}: {error}")),
        }
    }
    for note in &skipped {
        eprintln!("[synaptic] WARNING: no advisory corpus for {note}");
    }
    if composite.is_empty() && !may_be_empty {
        bail!(
            "no advisory corpus could be obtained for any locked ecosystem ({}). \
             Run `synaptic vuln sync`, or pass --advisories <dir>.",
            skipped.join("; ")
        );
    }
    Ok((composite, covered))
}

fn run_sync(ecosystem: &str, max_bytes: Option<u64>, cache: Option<PathBuf>) -> Result<()> {
    let ecosystem: Ecosystem = ecosystem
        .parse()
        .map_err(|error: String| anyhow::anyhow!(error))?;
    let cache = match cache {
        Some(root) => CorpusCache::new(root),
        None => CorpusCache::user_default()
            .context("cannot locate a home directory for the advisory cache; pass --cache")?,
    };
    let metadata = sync_ecosystem(
        &cache,
        &SystemCorpusFetcher,
        ecosystem,
        max_bytes.unwrap_or(DEFAULT_MAX_DOWNLOAD_BYTES),
    )?;
    println!(
        "synced {} advisories for {ecosystem} into {}",
        metadata.advisory_count,
        cache
            .resolve(ecosystem)
            .unwrap_or_else(|| cache.ecosystem_dir(ecosystem))
            .display()
    );
    println!("source: {}", metadata.source_url);
    if let Some(modified) = &metadata.last_modified {
        println!("upstream last modified: {modified}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_scan(
    root: &Path,
    advisories: Option<&Path>,
    offline: bool,
    online: bool,
    graph: Option<PathBuf>,
    json: bool,
    fail_on: Option<&str>,
    record: bool,
) -> Result<()> {
    let threshold = fail_on.map(parse_priority).transpose()?;

    // Every lockfile in the repository, not just Cargo's: a polyglot repo is
    // only as audited as its least-covered ecosystem. The Cargo manifests come
    // out of the same traversal, because the feature resolver needs them and
    // walking the tree twice bought nothing.
    let discovered = discover_repository_files(root);
    let (packages, reads) = PackageGraph::from_lockfiles(root, &discovered.lockfiles);
    for read in &reads {
        match &read.error {
            Some(error) => eprintln!(
                "[synaptic] WARNING: {} could not be read ({error}); its packages are NOT scanned",
                read.path.display()
            ),
            None => eprintln!(
                "[synaptic] {} ({:?}): {} packages",
                read.path.display(),
                read.kind,
                read.packages
            ),
        }
    }

    let policy = VulnPolicy::load(root).context("cannot load the vulnerability policy")?;
    let direct = synaptic_api::scan_dependencies(root)
        .context("cannot inventory the repository's direct dependencies")?;
    let has_auditable_declaration = direct.iter().any(|dependency| {
        dependency.resolved_version.is_some()
            && (dependency.package.ecosystem == Ecosystem::Maven
                || is_sbom_source(&dependency.source_file))
    });
    if reads.is_empty() && !has_auditable_declaration {
        bail!(
            "[synaptic:vuln:no-auditable-dependencies] no supported lockfile or auditable pinned \
             dependency declaration was found under {}. Supported lockfiles: {}",
            root.display(),
            LockfileKind::all()
                .iter()
                .map(|kind| kind.file_name())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let mut ecosystems = reads
        .iter()
        .filter(|read| read.error.is_none())
        .map(|read| read.kind.ecosystem())
        .collect::<BTreeSet<_>>();
    // An ecosystem can be present without a lockfile: Maven has none, and
    // Gradle writes one only when dependency locking is enabled. A declaration
    // that pins a literal version is still auditable, so those ecosystems need
    // a corpus too, or they would be reported unaudited while their versions
    // were sitting right there.
    //
    // This mirrors the promotion rule in `scan` exactly. Widening it to every
    // declared ecosystem would resolve a corpus for each, and npm's is 226,000
    // documents -- minutes of loading to audit whatever a stray fixture
    // manifest happened to name.
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
    ecosystems.extend(&declared_only);

    let (mut source, mut covered_ecosystems) =
        resolve_sources(advisories, offline, &ecosystems, online)?;

    // An explicit --online adds the OSV API alongside whatever corpora were
    // resolved. It is opt-in because the bulk export is the better default:
    // one request, offline afterwards, and it discloses nothing about this
    // repository's dependencies.
    if online && !synaptic_vuln::offline_forced() {
        let coordinates: Vec<PackageCoordinate> = packages
            .packages()
            .map(|package| package.key.coordinate.clone())
            .chain(
                direct
                    .iter()
                    .filter(|dependency| dependency.resolved_version.is_some())
                    .map(|dependency| dependency.package.clone()),
            )
            .collect();
        match live_source(&coordinates) {
            Ok(live) => {
                // An ecosystem the API can answer for is no longer unaudited.
                covered_ecosystems.extend(
                    ecosystems.iter().copied().filter(|ecosystem| {
                        synaptic_vuln::osv_ecosystem_name(*ecosystem).is_some()
                    }),
                );
                eprintln!("[synaptic] queried the OSV API: {}", live.describe().origin);
                source.push(live);
            }
            // A failed query must not quietly become a clean scan, but it must
            // not discard the corpora that did load either.
            Err(error) => eprintln!(
                "[synaptic] WARNING: the OSV API could not be reached ({error}); \
                 the scan continues against the local corpus only"
            ),
        }
    }

    // Nothing answered. Reporting that as a scan would print a clean bill of
    // health nobody checked.
    if source.is_empty() {
        bail!(
            "no advisory corpus could be obtained for any locked ecosystem and the OSV API \
             did not answer. Run `synaptic vuln sync`, or pass --advisories <dir>. \
             Refusing to report a scan against an empty corpus."
        );
    }

    // The graph supplies the raising signals, and is only ever consulted about
    // a package some advisory names. When the corpus names none of them there
    // is nothing to raise, so reading a 22 MB graph and indexing it would be
    // pure cost -- and a repository with no findings is the common case.
    //
    // The declared dependencies are asked about too, because `scan` promotes
    // the Maven and SBOM ones to scannable packages and they are not in the
    // lockfile graph this iterates.
    let corpus_names_something = packages.scan_targets().any(|resolved| {
        !AdvisorySource::advisories_for(&source, &resolved.key.coordinate).is_empty()
    }) || direct.iter().any(|dependency| {
        dependency.resolved_version.is_some()
            && !AdvisorySource::advisories_for(&source, &dependency.package).is_empty()
    });

    // The graph supplies the raising signals. Without it every finding still
    // gets version and dependency-path analysis, it just stays at
    // review-required more often.
    let graph_data = match graph {
        // An explicitly requested graph is always loaded, so a path that does
        // not exist is still reported rather than silently ignored.
        Some(path) => Some(load_graph_data(&path, None)?),
        None => {
            let conventional = root.join("synaptic-out/graph.json");
            (corpus_names_something && conventional.exists())
                .then(|| load_graph_data(&conventional, None))
                .transpose()?
        }
    };
    let graph_oracle = graph_data.as_ref().map(GraphUsageOracle::new);
    let usage: &dyn UsageOracle = match &graph_oracle {
        Some(oracle) => oracle,
        None => &NoUsageEvidence,
    };
    // Built from the same graph, and only when there is one, so an absent
    // graph leaves findings without entry-point evidence rather than
    // reporting that nothing reaches them.
    let reach_index = graph_data.as_ref().map(ReachIndex::new);
    // Last, because building a KnowledgeGraph consumes the GraphData the two
    // indexes above borrow.
    let impact_index = graph_data
        .map(|data| ImpactIndex::new(synaptic_graph::KnowledgeGraph::from_graph_data(data)));

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
    })?;

    if record {
        let store = FindingStore::new(root);
        let digest = policy.as_ref().cloned().unwrap_or_default().digest();
        let base = base_sha(root);
        for finding in &report.findings {
            let existed = store.get(&finding.id)?.is_some();
            store.upsert(
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
                    "synaptic vuln scan",
                    format!("{} at {}", finding.advisory_id, finding.resolved_version),
                ),
            )?;
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }

    if let Some(threshold) = threshold {
        let breaching = report
            .findings
            .iter()
            .filter(|finding| finding.priority <= threshold)
            .count();
        if breaching > 0 {
            bail!("{breaching} finding(s) at or above {threshold:?}");
        }
    }
    Ok(())
}

fn print_report(report: &ScanReport) {
    println!(
        "corpus: {} ({} advisories, {} unreadable, newest {})",
        report.corpus.origin,
        report.corpus.advisory_count,
        report.corpus.unreadable_documents,
        report
            .corpus
            .newest_modified
            .as_deref()
            .unwrap_or("unknown")
    );
    if report.corpus.advisory_count == 0 {
        println!("WARNING: the advisory corpus is empty, so an absence of findings means nothing");
    }
    if report.corpus.unreadable_documents > 0 {
        println!(
            "WARNING: {} advisory document(s) could not be parsed; coverage is incomplete",
            report.corpus.unreadable_documents
        );
    }
    println!("packages scanned: {}", report.packages_scanned);
    if report.packages_partially_audited > 0 {
        let ecosystems = report
            .coverage
            .iter()
            .filter(|(_, coverage)| **coverage == EcosystemCoverage::DirectOnly)
            .map(|(ecosystem, _)| ecosystem.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "packages partially audited: {} ({ecosystems}: direct declarations only, transitive \
             dependencies were not seen)",
            report.packages_partially_audited
        );
    }
    if report.packages_unaudited > 0 {
        let ecosystems = report
            .uncovered_ecosystems
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "WARNING: {} package(s) NOT audited (no corpus for {ecosystems}); they are not known to be clean",
            report.packages_unaudited
        );
    }
    println!(
        "findings: {} ({} applicable, {} suppressed by exception)",
        report.findings.len(),
        report.applicable().count(),
        report.suppressed.len()
    );
    for finding in &report.findings {
        println!();
        print_finding(finding);
    }
    for suppressed in &report.suppressed {
        println!(
            "\nsuppressed {} ({}) until {} by {}: {}",
            suppressed.finding_id,
            suppressed.advisory_id,
            suppressed.expires,
            suppressed.approved_by,
            suppressed.reason
        );
    }
}

fn print_finding(finding: &Finding) {
    println!(
        "[{:?}] {} {}@{}",
        finding.priority, finding.advisory_id, finding.package, finding.resolved_version
    );
    if let Some(summary) = &finding.summary {
        println!("  {summary}");
    }
    println!(
        "  id={} state={:?} severity={:?}{}",
        finding.id,
        finding.verdict.state,
        finding.severity.band,
        finding
            .severity
            .base_score
            .map(|score| format!(" ({score})"))
            .unwrap_or_default()
    );
    if !finding.dependency_path.is_empty() {
        let path = finding
            .dependency_path
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" -> ");
        println!("  path: {path}");
    }
    match &finding.remediation.recommended_version {
        Some(version) => println!(
            "  fix: upgrade to {version} (risk {:?}, availability {:?})",
            finding.remediation.compatibility_risk, finding.remediation.availability
        ),
        None => println!("  fix: {:?}", finding.remediation.kind),
    }
}

fn run_findings(root: &Path, json: bool, state: Option<&str>) -> Result<()> {
    let wanted = state.map(parse_state).transpose()?;
    let records = FindingStore::new(root).list()?;
    let records = records
        .into_iter()
        .filter(|record| wanted.is_none_or(|wanted| record.state == wanted))
        .collect::<Vec<_>>();

    if json {
        println!("{}", serde_json::to_string_pretty(&records)?);
        return Ok(());
    }
    if records.is_empty() {
        println!("no findings recorded; run `synaptic vuln scan --record` first");
        return Ok(());
    }
    for record in &records {
        println!(
            "{} [{:?}] {:?} {} {}@{} ({} decisions)",
            record.id,
            record.finding.priority,
            record.state,
            record.finding.advisory_id,
            record.finding.package,
            record.finding.resolved_version,
            record.decisions.len()
        );
    }
    Ok(())
}

fn run_explain(finding: &str, root: &Path, json: bool) -> Result<()> {
    let record = FindingStore::new(root)
        .get(finding)?
        .with_context(|| format!("finding {finding} is not in the ledger"))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&record)?);
        return Ok(());
    }

    print_finding(&record.finding);
    println!("\nevidence:");
    for item in &record.finding.verdict.evidence {
        println!("  [{:?}] {:?}: {}", item.direction, item.kind, item.detail);
    }
    let exposure = exposure_lines(&record.finding);
    if !exposure.is_empty() {
        println!();
        for line in exposure {
            println!("{line}");
        }
    }
    if !record.finding.remediation.required_changes.is_empty() {
        println!("\nrequired changes:");
        for change in &record.finding.remediation.required_changes {
            println!("  - {change}");
        }
    }
    if !record.finding.remediation.validation_commands.is_empty() {
        println!("\nvalidate with:");
        for command in &record.finding.remediation.validation_commands {
            println!("  - {command}");
        }
    }
    for note in &record.finding.remediation.notes {
        println!("\nnote: {note}");
    }
    if !record.finding.references.is_empty() {
        println!("\nreferences:");
        for reference in &record.finding.references {
            println!("  - {reference}");
        }
    }
    println!("\nhistory:");
    for entry in &record.decisions {
        println!(
            "  {} {:?} by {}: {}",
            entry.at, entry.kind, entry.actor, entry.detail
        );
    }
    Ok(())
}

/// Convert one finding into the brief the repair loop consumes.
///
/// The brief is only built for an `Applicable` finding, because generating a
/// patch for something never shown to be reachable would spend an agent's
/// budget on a guess. Anything short of that reports what is missing instead.
fn run_brief(finding_id: &str, root: &Path, graph: Option<PathBuf>, json: bool) -> Result<()> {
    let (record, context) = prepare_repair_context(finding_id, root, graph, None)?;
    let brief = context
        .brief
        .as_ref()
        .expect("an applicable vulnerability repair always has a brief");

    if json {
        println!("{}", serde_json::to_string_pretty(&brief)?);
        return Ok(());
    }

    println!("brief {} for {}", brief.id, record.finding.advisory_id);
    println!("  package:   {}", record.finding.package);
    println!(
        "  upgrade:   {} -> {}",
        record.finding.resolved_version,
        context.event.release.as_deref().unwrap_or("?")
    );
    println!("  base sha:  {}", brief.base_sha);
    println!("  bindings:  {}", brief.usage_bindings.len());
    if !brief.allowed_files.is_empty() {
        println!("  patch may touch:");
        for file in &brief.allowed_files {
            println!("    - {file}");
        }
    }
    if !brief.required_tests.is_empty() {
        println!("  required tests:");
        for test in &brief.required_tests {
            println!("    - {test}");
        }
    }
    if !brief.verification.is_empty() {
        println!("  verification:");
        for gate in &brief.verification {
            println!("    - {gate:?}");
        }
    }
    println!("\nrun `synaptic vuln brief {finding_id} --json` for the full generation input");
    Ok(())
}

fn vulnerability_repair_config() -> synaptic_api::ApiMaintenanceConfig {
    synaptic_api::ApiMaintenanceConfig {
        schema: synaptic_api::ApiMaintenanceConfig::SCHEMA,
        mode: synaptic_api::MaintenanceMode::DraftPr,
        base_branch: "main".into(),
        max_files: 12,
        max_changed_lines: 800,
        max_attempts: 3,
        max_risk_score: 80,
        allowed_paths: Vec::new(),
        allow_workflow_changes: false,
        allow_generated_changes: false,
        require_resolved_version: true,
        require_graph_invariants: true,
        require_tests: true,
        commands: synaptic_api::CommandPolicy::default(),
        publish: synaptic_api::PublishPolicy::default(),
        coverage: synaptic_api::CoveragePolicy::default(),
        vendors: Vec::new(),
    }
}

fn repository_relative(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let value = relative.to_string_lossy().replace('\\', "/");
    (!value.is_empty() && !value.starts_with("../")).then_some(value)
}

fn add_dependency_files(
    root: &Path,
    finding: &Finding,
    assessment: &mut synaptic_api::RelevanceAssessment,
) -> Vec<String> {
    let mut files = synaptic_api::scan_dependencies(root)
        .unwrap_or_default()
        .into_iter()
        .filter(|dependency| dependency.package == finding.package)
        .map(|dependency| dependency.source_file)
        .collect::<Vec<_>>();
    let discovered = discover_repository_files(root);
    for path in discovered.lockfiles {
        let matches_ecosystem = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(LockfileKind::for_file_name)
            .is_some_and(|kind| kind.ecosystem() == finding.package.ecosystem);
        if matches_ecosystem {
            if let Some(relative) = repository_relative(root, &path) {
                files.push(relative);
            }
        }
    }
    if finding.package.ecosystem == Ecosystem::Cargo {
        files.extend(
            discovered
                .cargo_manifests
                .iter()
                .filter_map(|path| repository_relative(root, path)),
        );
    }
    let selected = files.clone();
    for file in selected {
        files.extend(companion_dependency_files(root, &file));
    }
    files.sort();
    files.dedup();

    // Dependency manifests and their companion locks are mandatory repair
    // inputs. Put them first so the bounded brief cannot spend its file budget
    // on graph-adjacent source files before including consistency-critical files.
    let existing = std::mem::take(&mut assessment.bindings);
    let mut prioritized = Vec::with_capacity(existing.len() + files.len());
    for file in &files {
        if let Some(binding) = existing.iter().find(|binding| binding.source_file == *file) {
            prioritized.push(binding.clone());
        } else {
            prioritized.push(synaptic_api::ApiUsageBinding {
                vendor: assessment.vendor.clone(),
                operation_node_id: format!("{}#dependency", assessment.vendor),
                caller_node_id: format!("dependency-file:{file}"),
                source_file: file.clone(),
                source_location: None,
                sdk_package: Some(finding.package.name.clone()),
                sdk_member: None,
                sdk_version: Some(finding.resolved_version.clone()),
                api_version: None,
                basis: synaptic_api::BindingBasis::Unknown,
                confidence: 1.0,
            });
        }
    }
    prioritized.extend(
        existing
            .into_iter()
            .filter(|binding| !files.contains(&binding.source_file)),
    );
    assessment.bindings = prioritized;
    files
}

fn companion_dependency_files(root: &Path, file: &str) -> Vec<String> {
    let path = Path::new(file);
    let directory = path.parent().unwrap_or_else(|| Path::new(""));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let companions: &[&str] = match name {
        "package.json" | "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock" => &[
            "package.json",
            "package-lock.json",
            "pnpm-lock.yaml",
            "yarn.lock",
        ],
        "pyproject.toml" | "poetry.lock" | "uv.lock" => {
            &["pyproject.toml", "poetry.lock", "uv.lock"]
        }
        "Cargo.toml" | "Cargo.lock" => &["Cargo.toml", "Cargo.lock"],
        "go.mod" | "go.sum" => &["go.mod", "go.sum"],
        "composer.json" | "composer.lock" => &["composer.json", "composer.lock"],
        "Gemfile" | "Gemfile.lock" => &["Gemfile", "Gemfile.lock"],
        "Package.swift" | "Package.resolved" => &["Package.swift", "Package.resolved"],
        "pubspec.yaml" | "pubspec.lock" => &["pubspec.yaml", "pubspec.lock"],
        "mix.exs" | "mix.lock" => &["mix.exs", "mix.lock"],
        "Podfile" | "Podfile.lock" => &["Podfile", "Podfile.lock"],
        _ => &[],
    };
    companions
        .iter()
        .map(|companion| directory.join(companion))
        .filter(|candidate| root.join(candidate).is_file())
        .map(|candidate| candidate.to_string_lossy().replace('\\', "/"))
        .collect()
}

fn prepare_repair_context(
    finding_id: &str,
    root: &Path,
    graph: Option<PathBuf>,
    explicit_repository_identity: Option<&str>,
) -> Result<(
    synaptic_vuln::FindingRecord,
    crate::commands::api::ImpactContext,
)> {
    let record = FindingStore::new(root)
        .get(finding_id)?
        .with_context(|| format!("finding {finding_id} is not in the ledger"))?;
    if record.version != synaptic_vuln::FindingRecord::VERSION
        || record.id != finding_id
        || record.finding.id != finding_id
    {
        bail!("finding {finding_id} has inconsistent ledger identity");
    }
    let Some(mut inputs) = repair_inputs(&record.finding, record.created_at) else {
        bail!(
            "{finding_id} has no fixed version to upgrade to ({:?}); remediation requires \
             removing, replacing, or mitigating the dependency",
            record.finding.remediation.kind
        );
    };
    if inputs.assessment.state != synaptic_api::ApplicabilityState::Applicable {
        bail!(
            "{finding_id} is {:?}, not applicable; the repair loop only patches findings shown \
             to be reachable. Run `synaptic vuln explain {finding_id}` for the evidence.",
            inputs.assessment.state
        );
    }
    let dependency_files = add_dependency_files(root, &record.finding, &mut inputs.assessment);
    let graph_path = graph.unwrap_or_else(|| root.join("synaptic-out/graph.json"));
    let data = load_graph_data(&graph_path, None)
        .with_context(|| format!("loading graph {}", graph_path.display()))?;
    let current_base = base_sha(root);
    if current_base.is_empty() {
        bail!("vulnerability repair requires a repository with a resolvable HEAD commit");
    }
    if record.base_sha != current_base {
        bail!(
            "finding {finding_id} was recorded at {}, but repository HEAD is {}; rerun \
             `synaptic vuln scan --record` before repairing",
            record.base_sha,
            current_base
        );
    }
    let current_policy_digest = VulnPolicy::load(root)?.unwrap_or_default().digest();
    if record.policy_digest != current_policy_digest {
        bail!(
            "finding {finding_id} was recorded under a different vulnerability policy; rerun \
             `synaptic vuln scan --record` before repairing"
        );
    }
    if data
        .built_at_commit
        .as_deref()
        .is_some_and(|commit| commit != current_base)
    {
        bail!(
            "vulnerability graph was built at a different commit; rebuild with `synaptic extract .`"
        );
    }
    let base = current_base;
    let knowledge = synaptic_graph::KnowledgeGraph::from_graph_data(data);
    let identity = explicit_repository_identity
        .map(str::to_string)
        .unwrap_or_else(|| repository_identity(root));
    if record.repository_identity != identity {
        bail!(
            "finding {finding_id} belongs to repository {}, not {identity}",
            record.repository_identity
        );
    }
    let config = vulnerability_repair_config();
    if dependency_files.len() > config.max_files {
        bail!(
            "repair requires {} dependency manifest/lock files, exceeding the {}-file safety budget",
            dependency_files.len(),
            config.max_files
        );
    }
    let mut brief = synaptic_api::build_repair_brief(synaptic_api::RepairBriefRequest {
        repository_root: root,
        repository_identity: &identity,
        base_sha: &base,
        event: &inputs.event,
        assessment: &inputs.assessment,
        graph: &knowledge,
        memory: &[],
        budget: &synaptic_api::BriefBudget {
            max_files: config.max_files,
            ..synaptic_api::BriefBudget::default()
        },
    })?;
    let missing_dependency_files = dependency_files
        .iter()
        .filter(|file| !brief.allowed_files.contains(file))
        .cloned()
        .collect::<Vec<_>>();
    if !missing_dependency_files.is_empty() {
        bail!(
            "repair brief omitted required dependency files: {}",
            missing_dependency_files.join(", ")
        );
    }
    brief.verification.insert(
        0,
        synaptic_api::VerificationRequirement {
            gate: "vulnerability_resolution".into(),
            description:
                "Every audited resolution of the package is outside the advisory's affected range"
                    .into(),
            required: true,
        },
    );
    Ok((
        record.clone(),
        crate::commands::api::ImpactContext {
            config,
            event: inputs.event,
            assessment: inputs.assessment,
            graph: knowledge,
            brief: Some(brief),
            policy_digest: record.policy_digest.clone(),
        },
    ))
}

#[allow(clippy::too_many_arguments)]
fn run_repair(
    finding_id: &str,
    root: &Path,
    graph: Option<PathBuf>,
    dry_run: bool,
    agent_command: Option<&str>,
    candidate: Option<&Path>,
    explicit_repository_identity: Option<&str>,
    network_guard: Vec<String>,
    json: bool,
    emit: bool,
) -> Result<Option<String>> {
    let (record, context) =
        prepare_repair_context(finding_id, root, graph, explicit_repository_identity)?;
    let store = FindingStore::new(root);
    if !dry_run
        && !matches!(
            record.state,
            FindingState::Remediating | FindingState::Verified | FindingState::PullRequestOpen
        )
    {
        store.transition(
            finding_id,
            FindingState::Remediating,
            decision(
                DecisionKind::RemediationPlanned,
                "synaptic vuln repair",
                format!(
                    "bounded upgrade to {}",
                    record
                        .finding
                        .remediation
                        .recommended_version
                        .as_deref()
                        .unwrap_or("the fixed version")
                ),
            ),
        )?;
    }
    let result = crate::commands::api::repair_prepared(
        context,
        crate::commands::api::RepairWorkflow::Vulnerability,
        root,
        dry_run,
        agent_command,
        candidate,
        explicit_repository_identity,
        network_guard,
        json,
        emit,
    );
    let run_id = match result {
        Ok(run_id) => run_id,
        Err(error) => {
            if !dry_run {
                let current = store.get(finding_id)?;
                if current.is_some_and(|finding| finding.state == FindingState::Remediating) {
                    store.transition(
                        finding_id,
                        FindingState::Open,
                        decision(
                            DecisionKind::RemediationFailed,
                            "synaptic vuln repair",
                            error.to_string(),
                        ),
                    )?;
                }
            }
            return Err(error);
        }
    };
    if dry_run {
        return Ok(run_id);
    }
    let Some(run_id) = run_id else {
        return Ok(None);
    };
    let run = synaptic_api::ApiRunStore::vulnerability(root).load(&run_id)?;
    let current = store
        .get(finding_id)?
        .with_context(|| format!("finding {finding_id} disappeared during repair"))?;
    if run.state == synaptic_api::RunState::Verified
        && current.state != FindingState::Verified
        && current.state != FindingState::PullRequestOpen
    {
        store.transition(
            finding_id,
            FindingState::Verified,
            decision(
                DecisionKind::RemediationVerified,
                "synaptic vuln repair",
                format!("repair run {run_id} passed every required gate"),
            ),
        )?;
    } else if matches!(
        run.state,
        synaptic_api::RunState::RepairFailed
            | synaptic_api::RunState::VerificationFailed
            | synaptic_api::RunState::Inconclusive
    ) && current.state == FindingState::Remediating
    {
        store.transition(
            finding_id,
            FindingState::Open,
            decision(
                DecisionKind::RemediationFailed,
                "synaptic vuln repair",
                format!("repair run {run_id} ended in {:?}", run.state),
            ),
        )?;
    }
    Ok(Some(run_id))
}

fn run_verify(run_id: &str, root: &Path, json: bool) -> Result<()> {
    let (verification, patch_digest) = crate::commands::api::validate_repair_run(
        run_id,
        root,
        crate::commands::api::RepairWorkflow::Vulnerability,
    )?;
    let run = synaptic_api::ApiRunStore::vulnerability(root).load(run_id)?;
    validate_finding_run(root, &run)?;
    crate::commands::api::emit_json_or_text(
        json,
        &serde_json::json!({
            "version": 1,
            "run": run_id,
            "finding": run.event_id,
            "verification": verification,
            "patch_digest": patch_digest
        }),
        &format!("Vulnerability run {run_id} is conclusively verified"),
    )
}

fn run_publish(
    run_id: &str,
    root: &Path,
    json: bool,
    options: crate::commands::api::PublishOptions,
) -> Result<()> {
    let _ = crate::commands::api::validate_repair_run(
        run_id,
        root,
        crate::commands::api::RepairWorkflow::Vulnerability,
    )?;
    let directory = crate::commands::api::repair_run_directory(
        root,
        run_id,
        crate::commands::api::RepairWorkflow::Vulnerability,
    )?;
    let manifest: crate::commands::api::RepairManifest =
        crate::commands::api::read_json(directory.join("run.json"))?;
    let brief: synaptic_api::RepairBrief =
        crate::commands::api::read_json(directory.join("repair-brief.json"))?;
    let verification: synaptic_api::VerificationReport =
        crate::commands::api::read_json(directory.join("verification.json"))?;
    let context = crate::commands::api::publish_context(&options, "main")?;
    let ledger = synaptic_api::ApiRunStore::vulnerability(root);
    let mut run = ledger.load(run_id)?;
    validate_finding_run(root, &run)?;
    if !matches!(
        run.state,
        synaptic_api::RunState::Verified | synaptic_api::RunState::PrOpen
    ) {
        bail!(
            "vulnerability run {run_id} is {:?}, not publishable",
            run.state
        );
    }
    if run.event_id != brief.event.id || manifest.event_id != run.event_id {
        bail!("vulnerability publication artifacts disagree on the finding id");
    }
    let mut session = synaptic_sandbox::RepairSession::create_vulnerability(
        root,
        &manifest.branch,
        &brief.event.id,
    )?;
    session.preserve_branch_on_cleanup();
    let result = synaptic_api::publish_verified_vulnerability_change_request(
        &synaptic_api::DraftPublishRequest {
            worktree: session.path().to_path_buf(),
            branch: manifest.branch.clone(),
            brief,
            verification,
            labels: Vec::new(),
            reviewers: Vec::new(),
        },
        &context,
        &synaptic_api::SystemPublishCommandRunner,
    )?;
    let _branch = session.retain_verified_branch()?;
    crate::commands::api::write_pretty(directory.join("change-request.json"), &result)?;
    if run.state == synaptic_api::RunState::Verified {
        ledger.transition(
            &mut run,
            synaptic_api::RunState::PrOpen,
            None,
            Some(result.url.clone()),
        )?;
    }
    let findings = FindingStore::new(root);
    let finding = findings
        .get(&run.event_id)?
        .with_context(|| format!("finding {} is not in the ledger", run.event_id))?;
    if finding.state != FindingState::PullRequestOpen {
        if finding.state != FindingState::Verified {
            bail!(
                "finding {} is {:?}, not verified for publication",
                finding.id,
                finding.state
            );
        }
        findings.transition(
            &finding.id,
            FindingState::PullRequestOpen,
            decision(
                DecisionKind::PullRequestOpened,
                "synaptic vuln publish",
                result.url.clone(),
            ),
        )?;
    }
    crate::commands::api::emit_json_or_text(
        json,
        &serde_json::json!({"version":1,"run":run,"finding":finding.id,"publish":result}),
        &format!(
            "Draft change request for vulnerability run {run_id}: {}",
            result.url
        ),
    )
}

fn run_export(run_id: &str, root: &Path, output: &Path, json: bool) -> Result<()> {
    let (verification, _) = crate::commands::api::validate_repair_run(
        run_id,
        root,
        crate::commands::api::RepairWorkflow::Vulnerability,
    )?;
    let directory = crate::commands::api::repair_run_directory(
        root,
        run_id,
        crate::commands::api::RepairWorkflow::Vulnerability,
    )?;
    let run = synaptic_api::ApiRunStore::vulnerability(root).load(run_id)?;
    let finding = FindingStore::new(root)
        .get(&run.event_id)?
        .with_context(|| format!("finding {} is not in the ledger", run.event_id))?;
    validate_finding_run(root, &run)?;
    if finding.state != FindingState::Verified {
        bail!(
            "finding {} is {:?}, not exportable",
            finding.id,
            finding.state
        );
    }
    let event = crate::commands::api::read_json(directory.join("event.json"))?;
    let brief = crate::commands::api::read_json(directory.join("repair-brief.json"))?;
    let outcome = crate::commands::api::read_json(directory.join("repair-outcome.json"))?;
    let patch = std::fs::read_to_string(directory.join("proposed.patch"))?;
    let handoff = synaptic_vuln::VerifiedVulnerabilityRunHandoff::new(
        run,
        finding,
        event,
        brief,
        outcome,
        verification,
        patch,
    )?;
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    crate::commands::api::write_pretty(output.to_path_buf(), &handoff)?;
    crate::commands::api::emit_json_or_text(
        json,
        &serde_json::json!({
            "version":1,
            "run":run_id,
            "output":output,
            "bundle_digest":handoff.bundle_digest,
            "patch_digest":handoff.patch_digest
        }),
        &format!(
            "Exported verified vulnerability run {run_id} to {}",
            output.display()
        ),
    )
}

fn validate_finding_run(
    root: &Path,
    run: &synaptic_api::ApiRunRecord,
) -> Result<synaptic_vuln::FindingRecord> {
    let finding = FindingStore::new(root)
        .get(&run.event_id)?
        .with_context(|| format!("vulnerability run {} has no matching finding", run.id))?;
    if finding.id != finding.finding.id
        || finding.id != run.event_id
        || finding.base_sha != run.base_sha
        || finding.policy_digest != run.policy_digest
    {
        bail!(
            "vulnerability run {} and finding {} identities disagree",
            run.id,
            run.event_id
        );
    }
    if !matches!(
        finding.state,
        FindingState::Verified | FindingState::PullRequestOpen
    ) {
        bail!(
            "finding {} is {:?}, not verified",
            finding.id,
            finding.state
        );
    }
    Ok(finding)
}

fn run_import(bundle: &Path, expected_digest: &str, root: &Path, json: bool) -> Result<()> {
    const MAX_HANDOFF_BYTES: u64 = 64 * 1024 * 1024;
    let metadata = std::fs::metadata(bundle)
        .with_context(|| format!("inspect vulnerability handoff {}", bundle.display()))?;
    if metadata.len() > MAX_HANDOFF_BYTES {
        bail!("vulnerability handoff exceeds the 64 MiB limit");
    }
    let handoff: synaptic_vuln::VerifiedVulnerabilityRunHandoff =
        crate::commands::api::read_json(bundle.to_path_buf())?;
    handoff.verify()?;
    if handoff.bundle_digest != expected_digest {
        bail!("vulnerability handoff digest does not match --expected-digest");
    }
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize publication checkout {}", root.display()))?;
    let head = base_sha(&root);
    if head != handoff.run.base_sha {
        bail!(
            "publication checkout HEAD {head} does not match verified base {}",
            handoff.run.base_sha
        );
    }
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&root)
        .output()?;
    if !status.status.success() || !status.stdout.is_empty() {
        bail!("publication checkout must be clean before importing a vulnerability run");
    }
    let policy_digest = VulnPolicy::load(&root)?.unwrap_or_default().digest();
    if policy_digest != handoff.run.policy_digest {
        bail!("publication checkout vulnerability policy differs from the verified run");
    }
    let config = vulnerability_repair_config();
    let policy = synaptic_api::PatchPolicy {
        allowed_files: handoff.brief.allowed_files.clone(),
        max_files: config.max_files,
        max_changed_lines: config.max_changed_lines,
        allow_workflows: false,
        allow_generated: false,
        ..synaptic_api::PatchPolicy::default()
    };
    let inspection = synaptic_api::validate_patch(&root, &handoff.patch, &policy)?;
    let session = synaptic_sandbox::RepairSession::create_vulnerability(
        &root,
        &handoff.run.base_sha,
        &handoff.finding.id,
    )?;
    if session.branch() != handoff.branch {
        bail!("vulnerability handoff branch does not match the repair session");
    }
    session.apply_patch(handoff.patch.as_bytes())?;
    let title = format!(
        "Upgrade {} for {}",
        handoff.event.vendor, handoff.finding.finding.advisory_id
    );
    let commit = session.commit_verified_vulnerability(
        &title,
        &handoff.finding.id,
        &inspection.changed_files,
    )?;
    let branch = session.retain_verified_branch()?;
    FindingStore::new(&root).import_verified(&handoff.finding)?;
    let directory = crate::commands::api::repair_run_directory(
        &root,
        &handoff.run.id,
        crate::commands::api::RepairWorkflow::Vulnerability,
    )?;
    std::fs::create_dir_all(&directory)?;
    crate::commands::api::write_pretty(directory.join("event.json"), &handoff.event)?;
    crate::commands::api::write_pretty(directory.join("repair-brief.json"), &handoff.brief)?;
    crate::commands::api::write_pretty(directory.join("repair-outcome.json"), &handoff.outcome)?;
    crate::commands::api::write_pretty(directory.join("verification.json"), &handoff.verification)?;
    std::fs::write(directory.join("proposed.patch"), &handoff.patch)?;
    crate::commands::api::write_pretty(
        directory.join("run.json"),
        &crate::commands::api::RepairManifest {
            version: 1,
            run_id: handoff.run.id.clone(),
            event_id: handoff.finding.id.clone(),
            branch,
            commit,
        },
    )?;
    synaptic_api::ApiRunStore::vulnerability(&root).import_verified(&handoff.run)?;
    crate::commands::api::emit_json_or_text(
        json,
        &serde_json::json!({
            "version":1,
            "run":handoff.run.id,
            "finding":handoff.finding.id,
            "branch":handoff.branch,
            "bundle_digest":handoff.bundle_digest,
            "patch_digest":handoff.patch_digest
        }),
        &format!("Imported verified vulnerability run {}", handoff.run.id),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_composed(
    finding_id: &str,
    root: &Path,
    graph: Option<PathBuf>,
    dry_run: bool,
    agent_command: Option<&str>,
    network_guard: Vec<String>,
    defer_publish: bool,
    options: crate::commands::api::PublishOptions,
    json: bool,
) -> Result<()> {
    let run_id = run_repair(
        finding_id,
        root,
        graph,
        dry_run,
        agent_command,
        None,
        options.repository.as_deref(),
        network_guard,
        false,
        false,
    )?
    .ok_or_else(|| anyhow::anyhow!("vulnerability repair produced no run"))?;
    let mut published = false;
    if !dry_run && !defer_publish {
        let run = synaptic_api::ApiRunStore::vulnerability(root).load(&run_id)?;
        if run.state == synaptic_api::RunState::Verified {
            run_publish(&run_id, root, false, options)?;
            published = true;
        }
    }
    let run = synaptic_api::ApiRunStore::vulnerability(root).load(&run_id)?;
    crate::commands::api::emit_json_or_text(
        json,
        &serde_json::json!({
            "version":1,
            "finding":finding_id,
            "run":run_id,
            "state":run.state,
            "base_sha":run.base_sha,
            "policy_digest":run.policy_digest,
            "dry_run":dry_run,
            "publication_deferred":defer_publish,
            "published":published
        }),
        &format!("Vulnerability maintenance run {run_id}: {:?}", run.state),
    )
}

fn run_check(
    package: &str,
    version: Option<&str>,
    advisories: Option<&Path>,
    offline: bool,
    root: &Path,
    json: bool,
) -> Result<()> {
    let coordinate: PackageCoordinate = package
        .parse()
        .map_err(|error| anyhow::anyhow!("{error}"))
        .context("package must be <ecosystem>:<name>, for example cargo:serde")?;
    // Checking one package is a question about that package, so it goes
    // straight to OSV unless told not to. `--advisories` means the operator
    // chose a corpus deliberately, and that choice wins.
    let offline = offline || synaptic_vuln::offline_forced();
    let live = (!offline && advisories.is_none())
        .then(|| live_source(std::slice::from_ref(&coordinate)))
        .transpose();
    let source = match live {
        Ok(Some(source)) => source,
        Ok(None) => resolve_source(advisories, offline, coordinate.ecosystem)?,
        // Degrade to the local corpus rather than failing, and say which
        // answered: a degraded answer must never read as a complete one.
        Err(error) => {
            eprintln!(
                "[synaptic] WARNING: the OSV API could not be reached ({error}); \
                 falling back to the local corpus"
            );
            resolve_source(advisories, true, coordinate.ecosystem)?
        }
    };
    let policy = VulnPolicy::load(root).context("cannot load the vulnerability policy")?;

    let safety = check_dependency(&coordinate, version, &source, policy.as_ref());
    let corpus = source.describe();

    if json {
        // The corpus travels with the answer. Without it there is no way to
        // tell a live OSV result from one read out of a corpus that has not
        // been refreshed in a fortnight, and those are different claims.
        let mut document = serde_json::to_value(&safety)?;
        if let Some(object) = document.as_object_mut() {
            object.insert("corpus".into(), serde_json::to_value(&corpus)?);
        }
        println!("{}", serde_json::to_string_pretty(&document)?);
        return Ok(());
    }
    println!("{:?} {}", safety.verdict, safety.package);
    println!("  source: {}", corpus.origin);
    if let Some(constraint) = &safety.approved_constraint {
        println!(
            "  use {constraint} (availability {:?})",
            safety
                .constraint_availability
                .expect("a constraint always carries availability")
        );
    }
    for alternative in &safety.alternatives {
        println!("  alternative: {alternative}");
    }
    for reason in &safety.reasons {
        println!("  {reason}");
    }
    if safety.advisories.is_empty() && safety.reasons.is_empty() {
        println!("  no advisory in {} names this package", corpus.origin);
    }
    Ok(())
}

fn run_accept(
    finding: &str,
    root: &Path,
    reason: &str,
    until: &str,
    approved_by: &str,
) -> Result<()> {
    let store = FindingStore::new(root);
    store
        .get(finding)?
        .with_context(|| format!("finding {finding} is not in the ledger"))?;

    let path = root.join(DEFAULT_POLICY_PATH);
    let existing = if path.exists() {
        std::fs::read_to_string(&path)?
    } else {
        "schema = 1\n".to_string()
    };
    let block = format!(
        "\n[[exception]]\nfinding = {finding:?}\nreason = {reason:?}\nexpires = {until:?}\napproved_by = {approved_by:?}\n"
    );
    let candidate = format!("{existing}{block}");
    // Validate before writing so a malformed date cannot land in the policy.
    VulnPolicy::parse(&candidate).context("the resulting policy would be invalid")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, candidate)?;

    store.transition(
        finding,
        FindingState::Accepted,
        decision(
            DecisionKind::ExceptionGranted,
            approved_by,
            format!("{reason} (expires {until})"),
        ),
    )?;
    println!(
        "accepted {finding} until {until}; recorded in {}",
        path.display()
    );
    Ok(())
}

fn parse_priority(value: &str) -> Result<Priority> {
    match value.trim().to_ascii_lowercase().as_str() {
        "p0" => Ok(Priority::P0),
        "p1" => Ok(Priority::P1),
        "p2" => Ok(Priority::P2),
        "p3" => Ok(Priority::P3),
        other => bail!("unknown priority {other:?}; use p0, p1, p2 or p3"),
    }
}

fn parse_state(value: &str) -> Result<FindingState> {
    match value.trim().to_ascii_lowercase().as_str() {
        "open" => Ok(FindingState::Open),
        "accepted" => Ok(FindingState::Accepted),
        "remediating" => Ok(FindingState::Remediating),
        "verified" => Ok(FindingState::Verified),
        "pull_request_open" | "pr" => Ok(FindingState::PullRequestOpen),
        "resolved" => Ok(FindingState::Resolved),
        other => bail!("unknown finding state {other:?}"),
    }
}

/// A repository identity that is the same on every machine.
///
/// Finding ids are content-addressed over this, and a policy exception names a
/// finding id. Keying that on an absolute path would make every developer and
/// every CI runner compute a different id for the same vulnerability, so a
/// shared exception would silently fail to match. The git remote is used when
/// there is one; the canonical path remains the fallback for repositories
/// without a remote, where sharing is not possible anyway.
fn repository_identity(root: &Path) -> String {
    git_remote_url(root)
        .as_deref()
        .and_then(normalize_remote_identity)
        .unwrap_or_else(|| {
            root.canonicalize()
                .unwrap_or_else(|_| root.to_path_buf())
                .to_string_lossy()
                .into_owned()
        })
}

fn git_remote_url(root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!url.is_empty()).then_some(url)
}

/// Reduce a git remote URL to `host/namespace/repository`.
///
/// Handles the three forms git accepts: `https://host/ns/repo(.git)`,
/// `ssh://git@host/ns/repo(.git)`, and the scp-like `git@host:ns/repo(.git)`.
fn normalize_remote_identity(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    // Strip any scheme, then any `user@` credential prefix.
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let without_user = without_scheme
        .rsplit_once('@')
        .map_or(without_scheme, |(_, rest)| rest);
    // The scp-like form separates host from path with ':' rather than '/'.
    let normalized = without_user.replacen(':', "/", 1);

    let trimmed = normalized
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or_else(|| normalized.trim_end_matches('/'));
    let segments = trimmed
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    // host + namespace + repository at minimum; fewer means nothing portable.
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
    if root.join("Cargo.toml").exists() {
        vec!["cargo test --workspace --all-features --locked".to_string()]
    } else {
        Vec::new()
    }
}

/// Today's date as `YYYY-MM-DD` in UTC.
///
/// Computed directly from the epoch rather than pulling in a date library for
/// one call. The algorithm is Howard Hinnant's `civil_from_days`.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
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
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (year + i64::from(month <= 2), month, day)
}

/// Render the reachability evidence a finding carries.
///
/// Returns an empty vector only when there is genuinely nothing to say. The
/// difference between "no graph was read" and "the graph showed no calls" is
/// always stated, because those two carry opposite weight for a reviewer.
fn exposure_lines(finding: &Finding) -> Vec<String> {
    let mut lines = Vec::new();

    if !finding.scope.graph_backed {
        lines.push(
            "exposure: no graph was read, so call sites and entry points were not measured"
                .to_string(),
        );
        return lines;
    }

    if finding.call_sites.is_empty() {
        lines.push(
            "exposure: no first-party call sites found in the graph (static reachability is \
             incomplete; this does not prove the package is unused)"
                .to_string(),
        );
    } else {
        lines.push(format!(
            "call sites ({} in {} file(s)):",
            finding.call_sites.len(),
            finding.scope.review_files.len()
        ));
        for site in &finding.call_sites {
            let location = match site.line {
                Some(line) => format!("{}:{line}", site.file),
                None => site.file.clone(),
            };
            lines.push(format!("  {location}  {} -> {}", site.symbol, site.member));
        }
    }

    if finding.entry_points.is_empty() {
        if !finding.call_sites.is_empty() {
            lines.push(
                "entry points: none traced to a route, queue, or command (the walk is bounded \
                 and cannot see dynamic dispatch)"
                    .to_string(),
            );
        }
    } else {
        lines.push(format!(
            "reachable from {} entry point(s):",
            finding.entry_points.len()
        ));
        for entry in &finding.entry_points {
            lines.push(format!(
                "  [{}] {}",
                entry.kind.as_str(),
                entry.path.join(" -> ")
            ));
        }
    }

    if !finding.scope.review_files.is_empty() {
        lines.push("review after upgrading:".to_string());
        for file in &finding.scope.review_files {
            lines.push(format!("  - {file}"));
        }
    }

    if let Some(impact) = &finding.scope.impact {
        lines.push(format!(
            "upgrade blast radius: {} symbol(s) depend on the calling code",
            impact.dependent_symbols
        ));
        if !impact.public_api_touched.is_empty() {
            lines.push("  public API in the calling set:".to_string());
            for symbol in &impact.public_api_touched {
                lines.push(format!("    - {symbol}"));
            }
        }
        if impact.at_risk_tests.is_empty() {
            lines.push("  no covering tests found; verify this upgrade by hand".to_string());
        } else {
            lines.push(format!("  tests to run ({}):", impact.at_risk_tests.len()));
            for test in &impact.at_risk_tests {
                lines.push(format!("    - {test}"));
            }
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exposed_finding() -> Finding {
        let json = serde_json::json!({
            "version": 1,
            "id": "vuln_finding_test",
            "advisory_id": "RUSTSEC-2026-0001",
            "package": "cargo:leaf",
            "resolved_version": "0.9.18",
            "is_direct_dependency": true,
            "verdict": { "state": "applicable", "evidence": [], "runtime_reachable": true },
            "severity": { "band": "high", "source": "cvss_v3_vector" },
            "priority": "p1",
            "remediation": {
                "kind": "upgrade",
                "recommended_version": "0.9.20",
                "availability": "unverified",
                "compatibility_risk": "patch",
                "required_changes": [],
                "validation_commands": [],
                "notes": []
            },
            "call_sites": [
                { "symbol": "handle_items()", "symbol_id": "handle_items",
                  "file": "src/api.rs", "line": 31, "member": "parse" }
            ],
            "entry_points": [
                { "kind": "http_route", "label": "/items", "id": "route_items",
                  "path": ["/items", "handle_items()"] }
            ],
            "scope": {
                "graph_backed": true,
                "review_files": ["src/api.rs"],
                "calling_symbols": 1,
                "exposed_entry_points": 1
            }
        });
        serde_json::from_value(json).expect("fixture parses")
    }

    fn unmeasured_finding() -> Finding {
        let mut finding = exposed_finding();
        finding.call_sites.clear();
        finding.entry_points.clear();
        finding.scope = Default::default();
        finding
    }

    #[test]
    fn the_exposure_names_each_call_site_with_its_file_and_line() {
        let lines = exposure_lines(&exposed_finding()).join("\n");

        assert!(
            lines.contains("src/api.rs:31"),
            "call site location missing from:\n{lines}"
        );
        assert!(
            lines.contains("handle_items()"),
            "enclosing symbol missing from:\n{lines}"
        );
    }

    #[test]
    fn the_exposure_names_the_reaching_entry_point_and_its_path() {
        let lines = exposure_lines(&exposed_finding()).join("\n");

        assert!(
            lines.contains("/items"),
            "entry point missing from:\n{lines}"
        );
        assert!(
            lines.contains("/items -> handle_items()"),
            "reaching path missing from:\n{lines}"
        );
    }

    #[test]
    fn the_exposure_reports_the_upgrade_blast_radius_and_tests_to_run() {
        let mut finding = exposed_finding();
        finding.scope.impact = Some(
            serde_json::from_value(serde_json::json!({
                "dependent_symbols": 12,
                "public_api_touched": ["generateReport()"],
                "at_risk_tests": ["report_generator_test"]
            }))
            .expect("impact fixture parses"),
        );

        let lines = exposure_lines(&finding).join("\n");

        assert!(lines.contains("12"), "blast radius missing from:\n{lines}");
        assert!(
            lines.contains("report_generator_test"),
            "at-risk tests missing from:\n{lines}"
        );
        assert!(
            lines.contains("generateReport()"),
            "public API missing from:\n{lines}"
        );
    }

    #[test]
    fn an_exposure_without_a_forecast_makes_no_blast_radius_claim() {
        let lines = exposure_lines(&exposed_finding()).join("\n");

        assert!(
            !lines.contains("depend on"),
            "no forecast was run; this must not imply one:\n{lines}"
        );
    }

    #[test]
    fn an_unmeasured_finding_says_no_graph_was_read() {
        let lines = exposure_lines(&unmeasured_finding()).join("\n");

        assert!(
            lines.contains("no graph"),
            "an unmeasured scope must say so, got:\n{lines}"
        );
    }

    #[test]
    fn a_measured_finding_with_no_call_sites_is_distinguished_from_an_unmeasured_one() {
        let mut finding = exposed_finding();
        finding.call_sites.clear();
        finding.entry_points.clear();
        finding.scope.review_files.clear();
        finding.scope.calling_symbols = 0;
        finding.scope.exposed_entry_points = 0;

        let lines = exposure_lines(&finding).join("\n");

        assert!(
            !lines.contains("no graph"),
            "a graph was read; this must not claim otherwise:\n{lines}"
        );
        assert!(
            lines.contains("no first-party call sites"),
            "a measured absence must be stated, got:\n{lines}"
        );
    }

    #[test]
    fn converts_a_known_epoch_day_to_its_calendar_date() {
        // 2026-08-06 is 20671 days after the epoch.
        assert_eq!(civil_from_days(20_671), (2026, 8, 6));
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2024-02-29 exercises the leap-year branch.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn today_is_formatted_as_an_iso_calendar_date() {
        let value = today();

        assert_eq!(value.len(), 10);
        assert_eq!(value.matches('-').count(), 2);
        assert!(value.chars().all(|ch| ch.is_ascii_digit() || ch == '-'));
    }

    #[test]
    fn normalizes_an_https_remote_to_a_portable_identity() {
        assert_eq!(
            normalize_remote_identity("https://github.com/ColinVaughn/Synaptic.git").as_deref(),
            Some("github.com/ColinVaughn/Synaptic")
        );
    }

    #[test]
    fn normalizes_an_scp_style_ssh_remote() {
        assert_eq!(
            normalize_remote_identity("git@github.com:ColinVaughn/Synaptic.git").as_deref(),
            Some("github.com/ColinVaughn/Synaptic")
        );
    }

    #[test]
    fn normalizes_an_ssh_url_remote() {
        assert_eq!(
            normalize_remote_identity("ssh://git@github.com/org/repo.git").as_deref(),
            Some("github.com/org/repo")
        );
    }

    #[test]
    fn keeps_nested_namespaces_such_as_gitlab_subgroups() {
        assert_eq!(
            normalize_remote_identity("https://gitlab.com/group/subgroup/repo").as_deref(),
            Some("gitlab.com/group/subgroup/repo")
        );
    }

    #[test]
    fn tolerates_a_trailing_slash() {
        assert_eq!(
            normalize_remote_identity("https://github.com/org/repo/").as_deref(),
            Some("github.com/org/repo")
        );
    }

    #[test]
    fn a_remote_without_a_namespace_is_not_a_portable_identity() {
        assert_eq!(normalize_remote_identity("https://github.com/"), None);
        assert_eq!(normalize_remote_identity(""), None);
        assert_eq!(normalize_remote_identity("   "), None);
    }

    #[test]
    fn a_portable_identity_never_contains_a_local_filesystem_path() {
        // The whole point: a finding id keyed to an absolute path differs on
        // every machine, so a shared policy exception would silently not match.
        let identity = normalize_remote_identity("git@github.com:ColinVaughn/Synaptic.git")
            .expect("valid remote");

        assert_eq!(identity, "github.com/ColinVaughn/Synaptic");
        // `/` is the identity's own namespace separator, so testing it against
        // `MAIN_SEPARATOR` asserted something different on each platform: it
        // held on Windows and could never hold on Unix. What actually has to be
        // true is that nothing machine-local survived normalization.
        assert!(!identity.contains('\\'), "no Windows path separator");
        assert!(!identity.contains(':'), "no scheme or scp-style separator");
        assert!(!identity.starts_with('/'), "not an absolute path");
        assert!(
            identity
                .split('/')
                .next()
                .is_some_and(|host| host.contains('.')),
            "the first segment is a host, not a drive or directory"
        );
    }

    #[test]
    fn an_identity_from_a_windows_checkout_path_is_recognisably_local() {
        // The fallback when a repository has no remote. It is deliberately a
        // path, and the test above is what guards the portable case; this one
        // pins the distinction so the two never get conflated again.
        let local = PathBuf::from(r"C:\Users\dev\repo");

        let identity = local.to_string_lossy().into_owned();

        assert!(
            normalize_remote_identity(&identity).is_none(),
            "a bare checkout path is not a portable remote identity"
        );
    }

    #[test]
    fn priority_thresholds_parse_case_insensitively() {
        assert_eq!(parse_priority("P0").unwrap(), Priority::P0);
        assert_eq!(parse_priority("p3").unwrap(), Priority::P3);
        assert!(parse_priority("urgent").is_err());
    }

    #[test]
    fn finding_states_parse_from_their_serialized_names() {
        assert_eq!(parse_state("open").unwrap(), FindingState::Open);
        assert_eq!(
            parse_state("pull_request_open").unwrap(),
            FindingState::PullRequestOpen
        );
        assert!(parse_state("nonsense").is_err());
    }

    #[test]
    fn nested_go_module_includes_its_companion_sum() {
        let repository = tempfile::tempdir().unwrap();
        let module = repository.path().join("cmd/docker/publisher");
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(module.join("go.mod"), "module example.test/publisher\n").unwrap();
        std::fs::write(
            module.join("go.sum"),
            "example.test/dependency v1.0.0 h1:test\n",
        )
        .unwrap();

        assert_eq!(
            companion_dependency_files(repository.path(), "cmd/docker/publisher/go.mod"),
            vec![
                "cmd/docker/publisher/go.mod".to_string(),
                "cmd/docker/publisher/go.sum".to_string(),
            ]
        );
    }
}
