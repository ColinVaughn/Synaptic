//! The finding/severity model produced by the auditor. Serde wire format is
//! snake_case and versioned (like codegraph-predict's ChangeForecast).
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const AUDIT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Info,
}

impl Severity {
    /// Lower rank = more severe. Used to filter by a minimum severity.
    pub fn rank(self) -> u8 {
        match self {
            Severity::Critical => 0,
            Severity::High => 1,
            Severity::Medium => 2,
            Severity::Low => 3,
            Severity::Info => 4,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
            Severity::Info => "info",
        }
    }
    /// Parse a CLI threshold string.
    pub fn parse(s: &str) -> Option<Severity> {
        Some(match s.to_ascii_lowercase().as_str() {
            "critical" => Severity::Critical,
            "high" => Severity::High,
            "medium" => Severity::Medium,
            "low" => Severity::Low,
            "info" => Severity::Info,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Security,
    Performance,
    Correctness,
    Maintainability,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub rule_id: String,
    pub severity: Severity,
    pub category: Category,
    pub title: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub location: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub node_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub snippet: Option<String>,
    pub remediation: String,
    pub confidence: f32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub evidence: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditReport {
    pub version: u32,
    pub findings: Vec<Finding>,
    pub counts_by_severity: BTreeMap<String, usize>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub unparsed: Vec<String>,
    pub summary: String,
}

impl AuditReport {
    /// Build a report from raw findings: sort by severity then rule id, tally
    /// counts, and write a one-line summary. Deterministic output.
    pub fn from_findings(mut findings: Vec<Finding>, unparsed: Vec<String>) -> Self {
        findings.sort_by(|a, b| {
            a.severity
                .rank()
                .cmp(&b.severity.rank())
                .then_with(|| a.rule_id.cmp(&b.rule_id))
                .then_with(|| a.location.cmp(&b.location))
        });
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for f in &findings {
            *counts.entry(f.severity.as_str().to_string()).or_insert(0) += 1;
        }
        let summary = format!(
            "{} finding(s): {} critical, {} high, {} medium, {} low",
            findings.len(),
            counts.get("critical").copied().unwrap_or(0),
            counts.get("high").copied().unwrap_or(0),
            counts.get("medium").copied().unwrap_or(0),
            counts.get("low").copied().unwrap_or(0),
        );
        AuditReport {
            version: AUDIT_VERSION,
            findings,
            counts_by_severity: counts,
            unparsed,
            summary,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_sorts_by_severity_and_tallies() {
        let mk = |id: &str, sev: Severity| Finding {
            rule_id: id.into(),
            severity: sev,
            category: Category::Security,
            title: "t".into(),
            detail: "d".into(),
            location: None,
            node_ids: vec![],
            snippet: None,
            remediation: "fix".into(),
            confidence: 1.0,
            evidence: None,
        };
        let r = AuditReport::from_findings(
            vec![mk("B", Severity::Low), mk("A", Severity::Critical)],
            vec![],
        );
        assert_eq!(r.findings[0].rule_id, "A"); // critical sorts first
        assert_eq!(r.counts_by_severity.get("critical"), Some(&1));
        assert_eq!(r.version, AUDIT_VERSION);
    }

    #[test]
    fn severity_parse_and_rank() {
        assert_eq!(Severity::parse("HIGH"), Some(Severity::High));
        assert!(Severity::Critical.rank() < Severity::Low.rank());
        assert_eq!(Severity::parse("nope"), None);
    }
}
