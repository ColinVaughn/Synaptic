//! Design/correctness rules: missing primary keys and similar schema smells.
use codegraph_core::NodeKind;

use crate::findings::{Category, Finding, Severity};
use crate::graphview::{columns_of, nodes_of_kind};
use crate::rules::{AuditCtx, Rule};

pub fn register(rules: &mut Vec<Box<dyn Rule>>) {
    rules.push(Box::new(TableWithoutPrimaryKey));
    rules.push(Box::new(InsertWithoutColumnList));
    rules.push(Box::new(ImpliedForeignKey));
}

pub struct TableWithoutPrimaryKey;
pub struct InsertWithoutColumnList;
pub struct ImpliedForeignKey;

impl Rule for ImpliedForeignKey {
    fn id(&self) -> &'static str {
        "DES-FK-001"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let mut out = Vec::new();
        for t in nodes_of_kind(ctx.kg, NodeKind::Table) {
            for c in columns_of(ctx.kg, &t.id) {
                let name = c.label.to_lowercase();
                let Some(base) = name.strip_suffix("_id") else {
                    continue;
                };
                if base.is_empty() {
                    continue;
                }
                // A real foreign key (captured as fk_target) exempts the column.
                if c.extra.get("fk_target").is_some() {
                    continue;
                }
                // The table's own primary key is identity, not a relationship.
                if c.extra.get("pk").and_then(|v| v.as_bool()).unwrap_or(false) {
                    continue;
                }
                // A T-SQL IDENTITY surrogate key (often `<abbrev>_id`) is the
                // table's own generated id, not a foreign key.
                if c.extra
                    .get("identity")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    continue;
                }
                // Only key-typed columns look like a local reference; a text
                // `*_id` is usually an external identifier (Stripe/Zoho/etc.),
                // not a missing foreign key.
                let dt = c
                    .extra
                    .get("data_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                let key_typed = dt.contains("int") || dt.contains("uuid") || dt.contains("serial");
                if !key_typed {
                    continue;
                }
                out.push(Finding {
                    rule_id: self.id().into(),
                    severity: Severity::Low,
                    category: Category::Correctness,
                    title: format!(
                        "Column `{}.{}` implies a relationship but has no foreign key",
                        t.label, c.label
                    ),
                    detail: "A key-typed column named like a reference (`*_id`) with no FOREIGN KEY lets orphaned/invalid ids slip in; the database cannot enforce the relationship or cascade.".into(),
                    location: c.source_location.as_ref().map(|l| format!("{}:{}", c.source_file, l)),
                    node_ids: vec![t.id.0.clone(), c.id.0.clone()],
                    snippet: None,
                    remediation: format!(
                        "Add a foreign key, e.g. ALTER TABLE {} ADD FOREIGN KEY ({}) REFERENCES {}(id);",
                        t.label, c.label, base
                    ),
                    confidence: 0.4,
                    evidence: Some(format!("`{name}` is key-typed ({dt}) with no fk_target")),
                });
            }
        }
        out
    }
}

impl Rule for InsertWithoutColumnList {
    fn id(&self) -> &'static str {
        "DES-INS-001"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        // The detection lives in the shared query-text engine so the advise path
        // critiques candidate INSERTs too.
        crate::rules::performance::delegate(ctx, self.id())
    }
}

impl Rule for TableWithoutPrimaryKey {
    fn id(&self) -> &'static str {
        "DES-PK-001"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let mut out = Vec::new();
        for t in nodes_of_kind(ctx.kg, NodeKind::Table) {
            let cols = columns_of(ctx.kg, &t.id);
            // Only judge tables whose columns we actually captured.
            if cols.is_empty() {
                continue;
            }
            let has_pk = cols
                .iter()
                .any(|c| c.extra.get("pk").and_then(|v| v.as_bool()).unwrap_or(false));
            if !has_pk {
                out.push(Finding {
                    rule_id: self.id().into(),
                    severity: Severity::Medium,
                    category: Category::Correctness,
                    title: format!("Table `{}` has no primary key", t.label),
                    detail: "A table without a primary key has no reliable row identity; it complicates replication, dedup, and UPDATE/DELETE targeting.".into(),
                    location: t.source_location.as_ref().map(|l| format!("{}:{}", t.source_file, l)),
                    node_ids: vec![t.id.0.clone()],
                    snippet: None,
                    remediation: format!("Add a primary key to {} (e.g. an identity/serial id column).", t.label),
                    confidence: 0.7,
                    evidence: Some("no column marked pk".into()),
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::AuditCtx;
    use serde_json::json;

    #[test]
    fn flags_table_without_primary_key() {
        let kg = codegraph_graph::KnowledgeGraph::from_graph_data(
            serde_json::from_value(json!({
                "nodes": [
                    {"id":"sql:logs","label":"logs","file_type":"code","source_file":"s.sql","kind":"table"},
                    {"id":"sql:logs:col:msg","label":"msg","file_type":"code","source_file":"s.sql","kind":"column","pk":false}
                ],
                "links": [{"source":"sql:logs","target":"sql:logs:col:msg","relation":"has_column","confidence":"EXTRACTED","source_file":"s.sql"}]
            }))
            .unwrap(),
        );
        let cx = AuditCtx {
            kg: &kg,
            root: None,
        };
        let f = TableWithoutPrimaryKey.check(&cx);
        assert!(f.iter().any(|x| x.rule_id == "DES-PK-001"));
    }

    fn graph_with_query(sql: &str) -> codegraph_graph::KnowledgeGraph {
        codegraph_graph::KnowledgeGraph::from_graph_data(
            serde_json::from_value(json!({
                "nodes": [
                    {"id":"app.f","label":"f()","file_type":"code","source_file":"a.py","kind":"function"},
                    {"id":"sql:orders","label":"orders","file_type":"code","source_file":"s.sql","kind":"table"}
                ],
                "links": [{"source":"app.f","target":"sql:orders","relation":"writes_to","confidence":"INFERRED","source_file":"a.py","source_location":"L2","sql":sql}]
            }))
            .unwrap(),
        )
    }

    #[test]
    fn flags_insert_without_column_list() {
        let kg = graph_with_query("INSERT INTO orders VALUES (1, 2, 3)");
        let cx = AuditCtx {
            kg: &kg,
            root: None,
        };
        let f = InsertWithoutColumnList.check(&cx);
        assert!(f.iter().any(|x| x.rule_id == "DES-INS-001"), "{f:?}");
    }

    #[test]
    fn insert_with_explicit_column_list_is_not_flagged() {
        let kg = graph_with_query("INSERT INTO orders (id, total) VALUES (1, 99)");
        let cx = AuditCtx {
            kg: &kg,
            root: None,
        };
        let f = InsertWithoutColumnList.check(&cx);
        assert!(
            f.iter().all(|x| x.rule_id != "DES-INS-001"),
            "explicit column list should not flag: {f:?}"
        );
    }

    /// One-table graph: a single `<col>` column on `orders` with the given extra.
    fn fk_graph(col: &str, extra: serde_json::Value) -> codegraph_graph::KnowledgeGraph {
        let mut node = serde_json::json!({
            "id": format!("sql:orders:col:{col}"), "label": col,
            "file_type":"code","source_file":"s.sql","kind":"column"
        });
        node.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        codegraph_graph::KnowledgeGraph::from_graph_data(
            serde_json::from_value(json!({
                "nodes": [
                    {"id":"sql:orders","label":"orders","file_type":"code","source_file":"s.sql","kind":"table"},
                    node
                ],
                "links": [{"source":"sql:orders","target":format!("sql:orders:col:{col}"),"relation":"has_column","confidence":"EXTRACTED","source_file":"s.sql"}]
            }))
            .unwrap(),
        )
    }
    fn des_fk(kg: &codegraph_graph::KnowledgeGraph) -> Vec<Finding> {
        ImpliedForeignKey.check(&AuditCtx { kg, root: None })
    }

    #[test]
    fn flags_keyed_id_column_with_no_fk() {
        // uuid customer_id, not pk, no fk_target -> a real missing FK.
        let kg = fk_graph("customer_id", json!({"data_type":"uuid","pk":false}));
        assert!(des_fk(&kg).iter().any(|x| x.rule_id == "DES-FK-001"));
    }

    #[test]
    fn fk_target_exempts_column() {
        // conversation_id with a recorded fk_target (the prefixed-table case that
        // false-positived on A11yCore) must NOT flag.
        let kg = fk_graph(
            "conversation_id",
            json!({"data_type":"uuid","pk":false,"fk_target":"chat_conversations"}),
        );
        assert!(
            des_fk(&kg).iter().all(|x| x.rule_id != "DES-FK-001"),
            "fk_target must exempt the column"
        );
    }

    #[test]
    fn external_string_id_not_flagged() {
        // stripe_customer_id is text (an external API id), not a local FK.
        let kg = fk_graph("stripe_customer_id", json!({"data_type":"text","pk":false}));
        assert!(
            des_fk(&kg).iter().all(|x| x.rule_id != "DES-FK-001"),
            "text *_id is not an implied local FK"
        );
    }

    #[test]
    fn primary_key_id_column_not_flagged() {
        // key_id that is the table's own primary key is identity, not a relation.
        let kg = fk_graph("key_id", json!({"data_type":"uuid","pk":true}));
        assert!(des_fk(&kg).iter().all(|x| x.rule_id != "DES-FK-001"));
    }

    #[test]
    fn identity_surrogate_id_not_flagged() {
        // A T-SQL IDENTITY surrogate key named <abbrev>_id (bal_id) is the
        // table's own generated id, not a foreign key.
        let kg = fk_graph(
            "bal_id",
            json!({"data_type":"int","pk":false,"identity":true}),
        );
        assert!(des_fk(&kg).iter().all(|x| x.rule_id != "DES-FK-001"));
    }
}
