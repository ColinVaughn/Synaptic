//! End-to-end: a tiny SQL-aware graph yields the expected findings.
use codegraph_graph::KnowledgeGraph;
use codegraph_sqlaudit::{audit, AuditOptions};

#[test]
fn audits_a_small_graph() {
    let gd = serde_json::from_value(serde_json::json!({
        "nodes": [
            {"id":"sql:orders","label":"orders","file_type":"code","source_file":"s.sql","kind":"table"},
            {"id":"sql:orders:col:tenant_id","label":"tenant_id","file_type":"code","source_file":"s.sql","kind":"column"}
        ],
        "links": [
            {"source":"sql:orders","target":"sql:orders:col:tenant_id","relation":"has_column","confidence":"EXTRACTED","source_file":"s.sql"}
        ]
    }))
    .unwrap();
    let kg = KnowledgeGraph::from_graph_data(gd);
    let report = audit(&kg, &AuditOptions::default());
    assert!(report.findings.iter().any(|f| f.rule_id == "SEC-RLS-001"));
    // tenant_id column is FK-shaped and unindexed -> PERF-IDX-001.
    assert!(report.findings.iter().any(|f| f.rule_id == "PERF-IDX-001"));
}
