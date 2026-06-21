//! Markdown rendering of an audit report.
use crate::AuditReport;

/// Render an audit report as Markdown grouped by finding, severity-sorted.
pub fn render_markdown(report: &AuditReport) -> String {
    let mut out = format!("# SQL Audit\n\n{}\n\n", report.summary);
    for f in &report.findings {
        out.push_str(&format!(
            "## [{}] {} ({})\n\n{}\n\n- confidence: {:.2}\n- where: {}\n- fix: {}\n\n",
            f.severity.as_str(),
            f.title,
            f.rule_id,
            f.detail,
            f.confidence,
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

#[cfg(test)]
mod tests {
    use crate::findings::{AuditReport, Category, Finding, Severity};

    fn finding(conf: f32) -> Finding {
        Finding {
            rule_id: "PERF-IDX-001".into(),
            severity: Severity::Medium,
            category: Category::Performance,
            title: "Likely-foreign-key column not indexed".into(),
            detail: "d".into(),
            location: None,
            node_ids: vec![],
            snippet: None,
            remediation: "CREATE INDEX ...".into(),
            confidence: conf,
            evidence: None,
        }
    }

    #[test]
    fn markdown_shows_confidence() {
        // Confidence must be visible so a 0.50 name-heuristic reads as a guess
        // rather than carrying the same weight as a high-confidence finding.
        let r = AuditReport::from_findings(vec![finding(0.5)], vec![]);
        let md = super::render_markdown(&r);
        assert!(md.contains("0.5"), "confidence not rendered: {md}");
    }
}
