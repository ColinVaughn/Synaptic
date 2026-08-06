//! Dependency vulnerability commands.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use synaptic_api::{Ecosystem, PackageCoordinate};
use synaptic_vuln::{
    check_dependency, decision, scan, sync_ecosystem, AdvisorySource, CompositeSource, CorpusCache,
    DecisionKind, Finding, FindingState, FindingStore, GraphUsageOracle, LocalDirSource,
    LockfileKind, NoUsageEvidence, PackageGraph, Priority, ScanReport, ScanRequest,
    SystemCorpusFetcher, UsageOracle, VulnPolicy, DEFAULT_MAX_DOWNLOAD_BYTES, DEFAULT_POLICY_PATH,
    DEFAULT_STALE_AFTER_SECONDS,
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
            graph,
            json,
            fail_on,
            record,
        } => run_scan(
            &root,
            advisories.as_deref(),
            offline,
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
    let directory = cache.ecosystem_dir(ecosystem);
    let now = unix_now();
    let stale = cache.needs_sync(ecosystem, now, DEFAULT_STALE_AFTER_SECONDS);

    if stale && !offline {
        eprintln!(
            "[synaptic] fetching the {ecosystem} advisory corpus into {}",
            directory.display()
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
            Err(error) if directory.is_dir() => {
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

    if !directory.is_dir() {
        bail!(
            "no advisory corpus at {}. Run `synaptic vuln sync` (or pass --advisories <dir>). \
             Refusing to report a scan against an empty corpus.",
            directory.display()
        );
    }
    if stale && offline {
        eprintln!(
            "[synaptic] WARNING: the cached corpus is stale and --offline forbids refreshing it"
        );
    }
    LocalDirSource::load(&directory)
        .with_context(|| format!("cannot load advisories from {}", directory.display()))
}

/// Resolve one corpus per ecosystem the repository actually locks.
///
/// An ecosystem whose corpus cannot be obtained is reported and skipped rather
/// than failing the whole scan, so a repository with one exotic ecosystem still
/// gets audited for the others. Skipping is announced loudly: unaudited is not
/// the same as clean.
fn resolve_sources(
    explicit: Option<&Path>,
    offline: bool,
    ecosystems: &BTreeSet<Ecosystem>,
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
    if composite.is_empty() {
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
        cache.ecosystem_dir(ecosystem).display()
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
    graph: Option<PathBuf>,
    json: bool,
    fail_on: Option<&str>,
    record: bool,
) -> Result<()> {
    let threshold = fail_on.map(parse_priority).transpose()?;

    // Every lockfile in the repository, not just Cargo's: a polyglot repo is
    // only as audited as its least-covered ecosystem.
    let (packages, reads) = PackageGraph::from_repository(root);
    if reads.is_empty() {
        bail!(
            "no lockfile found under {}. Supported: {}",
            root.display(),
            LockfileKind::all()
                .iter()
                .map(|kind| kind.file_name())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
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

    let ecosystems = reads
        .iter()
        .filter(|read| read.error.is_none())
        .map(|read| read.kind.ecosystem())
        .collect::<BTreeSet<_>>();
    let (source, covered_ecosystems) = resolve_sources(advisories, offline, &ecosystems)?;
    let policy = VulnPolicy::load(root).context("cannot load the vulnerability policy")?;
    let direct = synaptic_api::scan_dependencies(root)
        .context("cannot inventory the repository's direct dependencies")?;

    // The graph supplies the raising signals. Without it every finding still
    // gets version and dependency-path analysis, it just stays at
    // review-required more often.
    let graph_data = match graph {
        Some(path) => Some(load_graph_data(&path, None)?),
        None => {
            let conventional = root.join("synaptic-out/graph.json");
            conventional
                .exists()
                .then(|| load_graph_data(&conventional, None))
                .transpose()?
        }
    };
    let graph_oracle = graph_data.as_ref().map(GraphUsageOracle::new);
    let usage: &dyn UsageOracle = match &graph_oracle {
        Some(oracle) => oracle,
        None => &NoUsageEvidence,
    };

    let identity = repository_identity(root);
    let report = scan(&ScanRequest {
        repository_identity: &identity,
        packages: &packages,
        direct_dependencies: &direct,
        source: &source,
        policy: policy.as_ref(),
        usage,
        validation_commands: validation_commands(root),
        today: today(),
        covered_ecosystems,
    })?;

    if record {
        let store = FindingStore::new(root);
        let digest = policy.as_ref().map(VulnPolicy::digest).unwrap_or_default();
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
    // Check the corpus for the package's own ecosystem, not the repository's.
    let source = resolve_source(advisories, offline, coordinate.ecosystem)?;
    let policy = VulnPolicy::load(root).context("cannot load the vulnerability policy")?;

    let safety = check_dependency(&coordinate, version, &source, policy.as_ref());

    if json {
        println!("{}", serde_json::to_string_pretty(&safety)?);
        return Ok(());
    }
    println!("{:?} {}", safety.verdict, safety.package);
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
        println!(
            "  no advisory in {} names this package",
            source.describe().origin
        );
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

#[cfg(test)]
mod tests {
    use super::*;

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

        assert!(!identity.contains(std::path::MAIN_SEPARATOR));
        assert!(!identity.contains(':'));
        assert!(!identity.starts_with('/'));
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
}
