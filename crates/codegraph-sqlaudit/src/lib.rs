//! SQL performance + security auditor over the SQL-aware code graph.
#![forbid(unsafe_code)]

pub mod advise;
pub mod explain;
pub mod findings;
pub mod graphview;
pub mod render;
pub mod rules;

#[cfg(feature = "live-explain")]
pub mod explain_live;

use std::path::PathBuf;

pub use advise::advise;
pub use explain::{NoPlan, PlanProvider, PlanSignal};
pub use findings::{AuditReport, Category, Finding, Severity};

#[cfg(feature = "live-explain")]
pub use explain_live::LiveExplain;

#[derive(Debug, Clone, Default)]
pub struct AuditOptions {
    /// Repo root for source-reading rules (N+1). None disables them.
    pub root: Option<PathBuf>,
    /// Only keep findings at least this severe.
    pub min_severity: Option<Severity>,
}

/// Run every rule over the graph and return a sorted, tallied report.
pub fn audit(kg: &codegraph_graph::KnowledgeGraph, opts: &AuditOptions) -> AuditReport {
    let ctx = rules::AuditCtx {
        kg,
        root: opts.root.as_deref(),
    };
    let mut findings = Vec::new();
    for rule in rules::all_rules() {
        findings.extend(rule.check(&ctx));
    }
    if let Some(min) = opts.min_severity {
        findings.retain(|f| f.severity.rank() <= min.rank());
    }
    AuditReport::from_findings(findings, Vec::new())
}

/// Like [`audit`] but also runs `provider` over each code->SQL query snippet,
/// adding a plan-backed finding for a real sequential scan.
pub fn audit_with_plan(
    kg: &codegraph_graph::KnowledgeGraph,
    opts: &AuditOptions,
    provider: &dyn PlanProvider,
) -> AuditReport {
    let report = audit(kg, opts);
    let ctx = rules::AuditCtx {
        kg,
        root: opts.root.as_deref(),
    };
    let mut extra: Vec<Finding> = Vec::new();
    for (snip, loc, ids) in rules::query_snippets(&ctx) {
        // Only EXPLAIN read queries; skip writes to avoid side effects.
        if !snip.trim_start().to_ascii_uppercase().starts_with("SELECT") {
            continue;
        }
        if let Some(sig) = provider.explain(&snip) {
            if sig.seq_scan {
                extra.push(Finding {
                    rule_id: "PERF-PLAN-001".into(),
                    severity: Severity::High,
                    category: Category::Performance,
                    title: "Query plan shows a sequential scan".into(),
                    detail: format!(
                        "EXPLAIN reports a sequential (full-table) scan{}. Confirm an index covers the filter/join columns.",
                        sig.est_rows.map(|r| format!(" over ~{r} rows")).unwrap_or_default()
                    ),
                    location: loc,
                    node_ids: ids,
                    snippet: Some(snip),
                    remediation: "Add an index on the filtered/joined columns, or rewrite the predicate to be sargable.".into(),
                    confidence: 0.9,
                    evidence: Some(sig.raw.chars().take(200).collect()),
                });
            }
        }
    }
    if let Some(min) = opts.min_severity {
        extra.retain(|f| f.severity.rank() <= min.rank());
    }
    let mut all = report.findings;
    all.extend(extra);
    AuditReport::from_findings(all, report.unparsed)
}
