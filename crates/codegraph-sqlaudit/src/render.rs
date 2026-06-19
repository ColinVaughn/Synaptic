//! Markdown rendering of an audit report.
use crate::AuditReport;

/// Render an audit report as Markdown grouped by finding, severity-sorted.
pub fn render_markdown(report: &AuditReport) -> String {
    let mut out = format!("# SQL Audit\n\n{}\n\n", report.summary);
    for f in &report.findings {
        out.push_str(&format!(
            "## [{}] {} ({})\n\n{}\n\n- where: {}\n- fix: {}\n\n",
            f.severity.as_str(),
            f.title,
            f.rule_id,
            f.detail,
            f.location.as_deref().unwrap_or("-"),
            f.remediation,
        ));
    }
    if !report.unparsed.is_empty() {
        out.push_str(&format!(
            "\n_{} statement(s) could not be parsed._\n",
            report.unparsed.len()
        ));
    }
    out
}
