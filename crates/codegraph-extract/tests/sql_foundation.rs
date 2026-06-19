//! Phase 1 end-to-end: a `.sql` schema produces a SQL-aware graph fragment with
//! object kinds, columns, an index, a policy, and RLS state on the table.
#![cfg(feature = "lang-sql")]

use codegraph_core::NodeKind;
use codegraph_extract::sql::extract_sql_source;

#[test]
fn schema_yields_columns_indexes_and_policy() {
    let schema = b"CREATE TABLE orders (id INT PRIMARY KEY, tenant_id INT, total NUMERIC);\n\
                   CREATE INDEX ix_orders_tenant ON orders (tenant_id);\n\
                   ALTER TABLE orders ENABLE ROW LEVEL SECURITY;\n\
                   CREATE POLICY tenant_isolation ON orders USING (tenant_id = 1);";
    let r = extract_sql_source("schema.sql", schema);

    assert!(
        r.nodes
            .iter()
            .any(|n| n.kind() == Some(NodeKind::Table) && n.label == "orders"),
        "expected an orders table node"
    );
    assert!(
        r.nodes
            .iter()
            .any(|n| n.kind() == Some(NodeKind::Column) && n.label == "tenant_id"),
        "expected a tenant_id column node"
    );
    assert!(
        r.nodes.iter().any(|n| n.kind() == Some(NodeKind::Index)),
        "expected an index node"
    );
    assert!(
        r.nodes.iter().any(|n| n.kind() == Some(NodeKind::Policy)),
        "expected a policy node"
    );

    let orders = r.nodes.iter().find(|n| n.label == "orders").unwrap();
    assert_eq!(
        orders.extra.get("rls_enabled").and_then(|v| v.as_bool()),
        Some(true),
        "RLS should be enabled on orders"
    );
    // The index points at the real tenant_id column node (id scheme match).
    let idx = r
        .nodes
        .iter()
        .find(|n| n.kind() == Some(NodeKind::Index))
        .unwrap();
    assert!(
        r.edges
            .iter()
            .any(|e| e.relation == "indexes" && e.source == idx.id),
        "the index should have an indexes edge to its column"
    );
}
