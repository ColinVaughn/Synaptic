//! `audit readiness`: static port-readiness findings over graph + source root.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use synaptic_readiness::{audit, render::render_markdown, AuditOptions, Profile, Severity};

use crate::cli::AuditAction;
use crate::commands::common::{default_graph_path, load_scoped_graph};

pub(crate) fn run_audit(action: AuditAction) -> Result<()> {
    match action {
        AuditAction::Readiness {
            graph,
            root,
            repo,
            profile,
            severity,
            limit,
            verbose,
            json,
            out,
        } => run_readiness(ReadinessArgs {
            graph,
            root,
            repo,
            profile,
            severity,
            limit,
            verbose,
            json,
            out,
        }),
    }
}

struct ReadinessArgs {
    graph: Option<PathBuf>,
    root: PathBuf,
    repo: Option<String>,
    profile: String,
    severity: Option<String>,
    limit: usize,
    verbose: bool,
    json: bool,
    out: Option<PathBuf>,
}

fn run_readiness(a: ReadinessArgs) -> Result<()> {
    let graph_path = default_graph_path(a.graph);
    let kg = load_scoped_graph(&graph_path, a.repo.as_deref())?;
    let profile = Profile::parse(&a.profile)
        .ok_or_else(|| anyhow!("unknown readiness profile '{}'", a.profile))?;
    let min_severity = match a.severity.as_deref() {
        Some(s) => Some(Severity::parse(s).ok_or_else(|| anyhow!("unknown severity '{s}'"))?),
        None => None,
    };
    let report = audit(
        &kg,
        &AuditOptions {
            root: Some(a.root.clone()),
            profile,
            min_severity,
            repo: a.repo.clone(),
        },
    );
    let report = if a.verbose {
        report
    } else {
        capped_report(report, a.limit)
    };

    let out_dir = a
        .out
        .unwrap_or_else(|| PathBuf::from("synaptic-out").join("readiness"));
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let json_path = out_dir.join("readiness.json");
    let md_path = out_dir.join("readiness.md");
    std::fs::write(&json_path, serde_json::to_vec_pretty(&report)?)
        .with_context(|| format!("writing {}", json_path.display()))?;
    std::fs::write(&md_path, render_markdown(&report))
        .with_context(|| format!("writing {}", md_path.display()))?;

    if a.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Readiness audit: {}", report.summary);
        println!("  json: {}", json_path.display());
        println!("  markdown: {}", md_path.display());
    }
    Ok(())
}

fn capped_report(
    mut report: synaptic_readiness::ReadinessReport,
    limit: usize,
) -> synaptic_readiness::ReadinessReport {
    if report.findings.len() > limit {
        report.findings.truncate(limit);
        report =
            synaptic_readiness::ReadinessReport::from_findings(report.findings, report.skipped);
    }
    report
}
