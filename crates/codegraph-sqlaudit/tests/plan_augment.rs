//! Plan-augmentation: a seq-scan provider adds a PERF-PLAN-001 finding.
use codegraph_graph::KnowledgeGraph;
use codegraph_sqlaudit::{audit_with_plan, AuditOptions, PlanProvider, PlanSignal};

struct AlwaysSeqScan;
impl PlanProvider for AlwaysSeqScan {
    fn explain(&self, _sql: &str) -> Option<PlanSignal> {
        Some(PlanSignal {
            seq_scan: true,
            est_cost: Some(999.0),
            est_rows: Some(50_000),
            raw: "Seq Scan on orders".into(),
        })
    }
}

#[test]
fn plan_seq_scan_adds_a_finding() {
    let gd = serde_json::from_value(serde_json::json!({
        "nodes": [
            {"id":"app.f","label":"f()","file_type":"code","source_file":"a.py","kind":"function"},
            {"id":"sql:orders","label":"orders","file_type":"code","source_file":"s.sql","kind":"table"}
        ],
        "links": [{"source":"app.f","target":"sql:orders","relation":"queries","confidence":"INFERRED","source_file":"a.py","source_location":"L2","sql":"SELECT id FROM orders WHERE tenant_id = 1"}]
    }))
    .unwrap();
    let kg = KnowledgeGraph::from_graph_data(gd);
    let r = audit_with_plan(&kg, &AuditOptions::default(), &AlwaysSeqScan);
    assert!(
        r.findings.iter().any(|f| f.rule_id == "PERF-PLAN-001"),
        "{:?}",
        r.findings.iter().map(|f| &f.rule_id).collect::<Vec<_>>()
    );
}
