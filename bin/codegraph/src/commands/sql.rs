//! `sql audit`: run the SQL auditor over the graph and write findings.json + audit.md.
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

use codegraph_sqlaudit::{advise, audit, render::render_markdown, AuditOptions, Severity};

use crate::cli::SqlAction;
use crate::commands::common::{default_graph_path, load_scoped_graph};

pub(crate) fn run_sql(action: SqlAction) -> Result<()> {
    match action {
        SqlAction::Audit {
            graph,
            root,
            severity,
            repo,
            out,
            explain,
            db_url,
            json,
        } => {
            let kg = load_scoped_graph(&default_graph_path(graph), repo.as_deref())?;
            let min_severity = match severity.as_deref() {
                Some(s) => Some(Severity::parse(s).ok_or_else(|| {
                    anyhow!("bad --severity '{s}' (use critical|high|medium|low|info)")
                })?),
                None => None,
            };
            let report = build_audit_report(
                &kg,
                &AuditOptions {
                    root: Some(root),
                    min_severity,
                },
                explain,
                db_url.as_deref(),
            );

            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
                return Ok(());
            }
            let out_dir = out.unwrap_or_else(|| PathBuf::from("codegraph-out/sql"));
            std::fs::create_dir_all(&out_dir)
                .with_context(|| format!("creating {}", out_dir.display()))?;
            let jpath = out_dir.join("findings.json");
            std::fs::write(&jpath, serde_json::to_string_pretty(&report)?)?;
            let mpath = out_dir.join("audit.md");
            std::fs::write(&mpath, render_markdown(&report))?;
            println!("SQL audit: {}", report.summary);
            println!("  findings: {}", jpath.display());
            println!("  report:   {}", mpath.display());
            Ok(())
        }
        SqlAction::Advise {
            query,
            dialect,
            graph,
            repo,
            json,
        } => {
            let kg = load_scoped_graph(&default_graph_path(graph), repo.as_deref())?;
            let report = advise(&kg, &query, dialect.as_deref());
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", report.summary);
                for f in &report.findings {
                    println!("  [{}] {} ({})", f.severity.as_str(), f.title, f.rule_id);
                    println!("      fix: {}", f.remediation);
                }
            }
            Ok(())
        }
    }
}

/// Audit, optionally corroborating perf findings with a live EXPLAIN provider.
/// The live path exists only when built with `--features live-explain`.
fn build_audit_report(
    kg: &codegraph_graph::KnowledgeGraph,
    opts: &AuditOptions,
    explain: bool,
    db_url: Option<&str>,
) -> codegraph_sqlaudit::AuditReport {
    #[cfg(feature = "live-explain")]
    {
        if explain {
            if let Some(url) = db_url {
                if let Some(p) = codegraph_sqlaudit::LiveExplain::new(url) {
                    return codegraph_sqlaudit::audit_with_plan(kg, opts, &p);
                }
                eprintln!(
                    "[codegraph] could not init live EXPLAIN; reporting static findings only"
                );
            } else {
                eprintln!("[codegraph] --explain needs --db-url; reporting static findings only");
            }
        }
    }
    #[cfg(not(feature = "live-explain"))]
    {
        if explain {
            eprintln!("[codegraph] --explain requires building with --features live-explain; reporting static findings only");
        }
    }
    let _ = (explain, db_url);
    audit(kg, opts)
}
