//! Advise on a single candidate query before it is written: run the query-text
//! checks, then cross-reference the graph for the tables it touches (RLS gaps,
//! unindexed filter columns, unknown tables).
use synaptic_core::NodeKind;
use synaptic_graph::KnowledgeGraph;

use crate::findings::{AuditReport, Category, Finding, Severity};
use crate::graphview::{columns_of, indexed_columns, nodes_of_kind, table_flag};
use crate::rules::performance::evaluate_query_text;
use crate::rules::security::TENANT_HINTS;

/// Critique `query` against the graph. `dialect` is advisory (parser dialect).
pub fn advise(kg: &KnowledgeGraph, query: &str, _dialect: Option<&str>) -> AuditReport {
    let mut findings = evaluate_query_text(query, None, Vec::new());
    let mut unparsed = Vec::new();

    let tables = referenced_tables(query);
    if tables.is_empty() {
        unparsed.push(query.chars().take(200).collect());
    }
    for tname in &tables {
        let tnode = nodes_of_kind(kg, NodeKind::Table)
            .into_iter()
            .find(|t| t.label.eq_ignore_ascii_case(tname));
        let Some(t) = tnode else {
            findings.push(Finding {
                rule_id: "ADV-TABLE-404".into(),
                severity: Severity::Info,
                category: Category::Correctness,
                title: format!("Referenced table `{tname}` is not in the graph"),
                detail: "The query targets a table the code graph does not define; double-check the name or extract its schema.".into(),
                location: None,
                node_ids: vec![],
                snippet: None,
                remediation: format!("Confirm `{tname}` exists and is included in extraction."),
                confidence: 0.5,
                evidence: None,
            });
            continue;
        };
        // RLS gap on a tenant-scoped table.
        let has_tenant = columns_of(kg, &t.id)
            .iter()
            .any(|c| TENANT_HINTS.contains(&c.label.to_lowercase().as_str()));
        if has_tenant && !table_flag(t, "rls_enabled") {
            findings.push(Finding {
                rule_id: "SEC-RLS-001".into(),
                severity: Severity::High,
                category: Category::Security,
                title: format!("Querying `{}`, which has a tenant column but no RLS", t.label),
                detail: "This table is multi-tenant but not RLS-protected; this query must filter by tenant explicitly or it can leak across tenants.".into(),
                location: None,
                node_ids: vec![t.id.0.clone()],
                snippet: None,
                remediation: "Enable + FORCE row-level security on the table, and ensure the query filters by tenant.".into(),
                confidence: 0.6,
                evidence: Some("tenant-like column, rls_enabled false".into()),
            });
        }
        // Unindexed filter columns referenced in the query.
        let indexed = indexed_columns(kg, &t.id);
        let table_cols: Vec<String> = columns_of(kg, &t.id)
            .iter()
            .map(|c| c.label.to_lowercase())
            .collect();
        for col in where_columns(query) {
            if table_cols.contains(&col) && !indexed.contains(&col) {
                findings.push(Finding {
                    rule_id: "PERF-IDX-001".into(),
                    severity: Severity::Medium,
                    category: Category::Performance,
                    title: format!("Filtering `{}.{}` which has no index", t.label, col),
                    detail:
                        "The query filters on a column with no index, so it will scan the table."
                            .into(),
                    location: None,
                    node_ids: vec![t.id.0.clone()],
                    snippet: None,
                    remediation: format!("CREATE INDEX ON {} ({});", t.label, col),
                    confidence: 0.5,
                    evidence: Some("WHERE column not in any index".into()),
                });
            }
        }
    }
    AuditReport::from_findings(findings, unparsed)
}

/// Table names referenced by the candidate. SELECT/CTE go through the sqlparser
/// AST (FROM + JOIN); a regex over FROM/JOIN/INTO/UPDATE/TABLE backstops write
/// statements and parse failures without depending on dialect-specific shapes.
fn referenced_tables(sql: &str) -> Vec<String> {
    use sqlparser::ast::{SetExpr, Statement, TableFactor};
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;
    use std::sync::LazyLock;

    let clean = |raw: &str| {
        raw.trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']')
            .to_string()
    };
    let last_ident = |q: &str| clean(q.rsplit('.').next().unwrap_or(q));

    let mut out: Vec<String> = Vec::new();
    if let Ok(stmts) = Parser::parse_sql(&GenericDialect {}, sql) {
        for stmt in stmts {
            if let Statement::Query(q) = stmt {
                if let SetExpr::Select(select) = *q.body {
                    for twj in select.from {
                        if let TableFactor::Table { name, .. } = twj.relation {
                            if let Some(p) = name.0.last() {
                                out.push(last_ident(&p.to_string()));
                            }
                        }
                        for j in twj.joins {
                            if let TableFactor::Table { name, .. } = j.relation {
                                if let Some(p) = name.0.last() {
                                    out.push(last_ident(&p.to_string()));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    static TBL_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?is)\b(?:from|join|into|update|table)\s+[`"\[]?([\w.]+)"#)
            .expect("table regex")
    });
    for caps in TBL_RE.captures_iter(sql) {
        let name = last_ident(&caps[1]);
        if !name.is_empty() && !out.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
            out.push(name);
        }
    }
    out
}

/// Lowercased column identifiers used in the WHERE clause (best-effort regex).
fn where_columns(sql: &str) -> Vec<String> {
    use std::sync::LazyLock;
    static WHERE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?is)\bwhere\b(.*?)(?:\bgroup\b|\border\b|\blimit\b|;|$)")
            .expect("where regex")
    });
    static COL_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        // optional `table.` qualifier, then the column, then an operator.
        regex::Regex::new(
            r"(?i)(?:[a-z_][a-z0-9_]*\.)?([a-z_][a-z0-9_]*)\s*(=|<|>|<=|>=|like|in|between)",
        )
        .expect("col regex")
    });
    let Some(c) = WHERE_RE.captures(sql) else {
        return Vec::new();
    };
    COL_RE
        .captures_iter(&c[1])
        .map(|m| m[1].to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kg() -> KnowledgeGraph {
        KnowledgeGraph::from_graph_data(
            serde_json::from_value(serde_json::json!({
                "nodes": [
                    {"id":"sql:orders","label":"orders","file_type":"code","source_file":"s.sql","kind":"table","rls_enabled":false},
                    {"id":"sql:orders:col:tenant_id","label":"tenant_id","file_type":"code","source_file":"s.sql","kind":"column"}
                ],
                "links": [{"source":"sql:orders","target":"sql:orders:col:tenant_id","relation":"has_column","confidence":"EXTRACTED","source_file":"s.sql"}]
            }))
            .unwrap(),
        )
    }

    #[test]
    fn advise_flags_select_star_and_rls_and_unindexed_filter() {
        let r = advise(&kg(), "SELECT * FROM orders WHERE tenant_id = 1", None);
        let ids: Vec<&str> = r.findings.iter().map(|f| f.rule_id.as_str()).collect();
        assert!(ids.contains(&"PERF-SEL-001"), "{ids:?}");
        assert!(ids.contains(&"SEC-RLS-001"), "{ids:?}");
        assert!(ids.contains(&"PERF-IDX-001"), "{ids:?}");
    }

    #[test]
    fn advise_notes_unknown_table() {
        let r = advise(&kg(), "SELECT id FROM nonexistent", None);
        assert!(r.findings.iter().any(|f| f.rule_id == "ADV-TABLE-404"));
    }

    #[test]
    fn advise_injection_remediation_matches_audit_smart_text() {
        // advise_sql shares the query-text engine (`evaluate_query_text`) with
        // audit_sql, so its SEC-INJ-001 remediation carries the same smart
        // identifier-vs-value wording -- never a generic-only "bound parameters"
        // line when the interpolation sits in identifier position. Guards the two
        // tools against silently diverging.
        let g = kg();

        // Identifier-position interpolation cannot be a bound parameter.
        let ident = advise(&g, "SELECT * FROM \"${table}\"", None);
        let fi = ident
            .findings
            .iter()
            .find(|f| f.rule_id == "SEC-INJ-001")
            .expect("identifier-position injection is flagged");
        assert!(
            fi.remediation.contains("identifier-quoting"),
            "advise must steer an interpolated identifier to allowlist+quote: {}",
            fi.remediation
        );

        // Value-position interpolation: the plain bound-parameter advice is right.
        let val = advise(&g, "SELECT * FROM orders WHERE id = ${id}", None);
        let fv = val
            .findings
            .iter()
            .find(|f| f.rule_id == "SEC-INJ-001")
            .expect("value-position injection is flagged");
        assert!(
            fv.remediation.contains("bound parameters")
                && !fv.remediation.contains("identifier-quoting"),
            "advise keeps plain bound-parameter advice for a value: {}",
            fv.remediation
        );
    }
}
