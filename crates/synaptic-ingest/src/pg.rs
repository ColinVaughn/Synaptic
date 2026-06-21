//! Live PostgreSQL schema introspection.
//!
//! Synaptic has no SQL extractor, so we **emit nodes/edges directly** from the
//! introspected `information_schema` rows (design §10.1): one node per
//! table/view/function under a schema root, foreign keys as `references` edges.
//!
//! Design for testability: the schema rows are gathered into a plain
//! [`PgSchema`] DTO, and [`build_postgres_graph`] turns that into [`Ingested`]
//! with no I/O — fully unit-testable. Only the live connector
//! ([`SystemPostgres`], behind the `pg` cargo feature) touches a database, and
//! that path is untestable offline (as accepted for this source).

use serde_json::{json, Map, Value};
use synaptic_core::{sanitize_label, sanitize_metadata, Confidence, Edge, FileType, Node, NodeId};

use crate::Ingested;

/// Error from live introspection. Connection errors are pre-sanitized to a
/// single line so a DSN/credentials embedded in the driver message can't leak.
#[derive(Debug, thiserror::Error)]
pub enum PgError {
    #[error("could not connect to PostgreSQL: {0}")]
    Connection(String),
    #[error("postgres query failed: {0}")]
    Query(String),
}

/// A base table (we only graph `BASE TABLE`s; views come via [`PgView`]).
#[derive(Debug, Clone, PartialEq)]
pub struct PgTable {
    pub schema: String,
    pub name: String,
    /// `information_schema.tables.table_type` — only `BASE TABLE` is graphed.
    pub table_type: String,
}

/// A view.
#[derive(Debug, Clone, PartialEq)]
pub struct PgView {
    pub schema: String,
    pub name: String,
}

/// A function or procedure (`routine_type` `FUNCTION`/`PROCEDURE`).
#[derive(Debug, Clone, PartialEq)]
pub struct PgRoutine {
    pub schema: String,
    pub name: String,
    pub routine_type: String,
}

/// A foreign-key constraint (composite-aware via the column lists).
#[derive(Debug, Clone, PartialEq)]
pub struct PgForeignKey {
    pub constraint: String,
    pub table_schema: String,
    pub table_name: String,
    pub ref_schema: String,
    pub ref_name: String,
    pub columns: Vec<String>,
    pub ref_columns: Vec<String>,
}

/// The full introspected schema — the pure input to [`build_postgres_graph`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PgSchema {
    pub host: String,
    pub dbname: String,
    pub tables: Vec<PgTable>,
    pub views: Vec<PgView>,
    pub routines: Vec<PgRoutine>,
    pub foreign_keys: Vec<PgForeignKey>,
}

/// Something that can produce a [`PgSchema`] (a live DB, or a mock in tests).
pub trait SchemaSource {
    fn fetch(&self) -> Result<PgSchema, PgError>;
}

/// Fetch a schema from `source` and turn it into graph nodes/edges.
pub fn introspect_postgres(source: &dyn SchemaSource) -> Result<Ingested, PgError> {
    Ok(build_postgres_graph(&source.fetch()?))
}

fn qualified(schema: &str, name: &str) -> String {
    format!("{schema}.{name}")
}

fn table_id(schema: &str, name: &str) -> String {
    format!("pg_table:{schema}.{name}")
}
fn view_id(schema: &str, name: &str) -> String {
    format!("pg_view:{schema}.{name}")
}
fn function_id(schema: &str, name: &str) -> String {
    format!("pg_function:{schema}.{name}")
}

fn make_pg_node(id: &str, label: &str, source_file: &str, kind: &str) -> Node {
    let mut extra = Map::new();
    extra.insert("metadata".into(), json!({ "pg_kind": kind }));
    Node {
        id: NodeId(id.to_string()),
        label: sanitize_label(label),
        file_type: FileType::Code,
        source_file: source_file.to_string(),
        source_location: None,
        community: None,
        repo: None,
        extra,
    }
}

fn make_pg_edge(
    source: &str,
    target: &str,
    relation: &str,
    source_file: &str,
    meta: Map<String, Value>,
) -> Edge {
    let mut extra = Map::new();
    if !meta.is_empty() {
        extra.insert("metadata".into(), Value::Object(sanitize_metadata(&meta)));
    }
    Edge {
        source: NodeId(source.to_string()),
        target: NodeId(target.to_string()),
        relation: relation.to_string(),
        confidence: Confidence::Extracted,
        confidence_score: Some(1.0),
        source_file: source_file.to_string(),
        source_location: None,
        weight: 1.0,
        context: Some("postgres".to_string()),
        cross_repo: false,
        extra,
    }
}

/// Turn an introspected [`PgSchema`] into graph nodes/edges (no I/O).
///
/// A `pg_schema` root node `contains` one node per base table, view, and
/// function/procedure; each foreign key becomes a `references` edge from the
/// child table to the referenced table, carrying the constraint + column lists
/// in metadata. The shared `source_file` is the sanitized virtual DSN path
/// `postgresql://{host}/{dbname}` (no credentials).
pub fn build_postgres_graph(schema: &PgSchema) -> Ingested {
    let mut out = Ingested::default();
    let source_file = format!("postgresql://{}/{}", schema.host, schema.dbname);
    let root_id = format!("pg_schema:{}/{}", schema.host, schema.dbname);
    out.nodes.push(make_pg_node(
        &root_id,
        &format!("{}@{}", schema.dbname, schema.host),
        &source_file,
        "database",
    ));

    for t in &schema.tables {
        if t.table_type != "BASE TABLE" {
            continue; // views surface via the dedicated views query
        }
        let id = table_id(&t.schema, &t.name);
        out.nodes.push(make_pg_node(
            &id,
            &qualified(&t.schema, &t.name),
            &source_file,
            "table",
        ));
        out.edges.push(make_pg_edge(
            &root_id,
            &id,
            "contains",
            &source_file,
            Map::new(),
        ));
    }
    for v in &schema.views {
        let id = view_id(&v.schema, &v.name);
        out.nodes.push(make_pg_node(
            &id,
            &qualified(&v.schema, &v.name),
            &source_file,
            "view",
        ));
        out.edges.push(make_pg_edge(
            &root_id,
            &id,
            "contains",
            &source_file,
            Map::new(),
        ));
    }
    for r in &schema.routines {
        if r.routine_type != "FUNCTION" && r.routine_type != "PROCEDURE" {
            continue;
        }
        let id = function_id(&r.schema, &r.name);
        out.nodes.push(make_pg_node(
            &id,
            &format!("{}()", qualified(&r.schema, &r.name)),
            &source_file,
            "function",
        ));
        out.edges.push(make_pg_edge(
            &root_id,
            &id,
            "contains",
            &source_file,
            Map::new(),
        ));
    }
    for fk in &schema.foreign_keys {
        let child = table_id(&fk.table_schema, &fk.table_name);
        let parent = table_id(&fk.ref_schema, &fk.ref_name);
        let mut meta = Map::new();
        meta.insert("constraint".into(), json!(fk.constraint));
        meta.insert("columns".into(), json!(fk.columns));
        meta.insert("ref_columns".into(), json!(fk.ref_columns));
        out.edges.push(make_pg_edge(
            &child,
            &parent,
            "references",
            &source_file,
            meta,
        ));
    }
    out
}

// Live connector (feature-gated; untestable offline).

/// Keep only the first line of a driver error so a DSN/credentials the driver
/// may embed in later lines can't leak into our error output.
#[cfg(feature = "pg")]
fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

/// A live PostgreSQL database addressed by a DSN (empty DSN = `PG*` env vars).
#[cfg(feature = "pg")]
pub struct SystemPostgres {
    dsn: String,
}

#[cfg(feature = "pg")]
impl SystemPostgres {
    pub fn new(dsn: impl Into<String>) -> Self {
        Self { dsn: dsn.into() }
    }
}

#[cfg(feature = "pg")]
impl SchemaSource for SystemPostgres {
    fn fetch(&self) -> Result<PgSchema, PgError> {
        use postgres::config::Host;
        use postgres::{Client, Config, NoTls};
        use std::str::FromStr;

        let config = Config::from_str(&self.dsn)
            .map_err(|e| PgError::Connection(first_line(&e.to_string())))?;
        let host = config
            .get_hosts()
            .iter()
            .find_map(|h| {
                // `Host::Unix` only exists under cfg(unix), so the wildcard is
                // unreachable on Windows but required on Unix.
                #[allow(unreachable_patterns)]
                match h {
                    Host::Tcp(s) => Some(s.clone()),
                    _ => None,
                }
            })
            .unwrap_or_else(|| "localhost".to_string());
        let dbname = config.get_dbname().unwrap_or("db").to_string();

        let mut client = config
            .connect(NoTls)
            .map_err(|e| PgError::Connection(first_line(&e.to_string())))?;
        // Read-only, deferrable; best effort (applies to the next transaction).
        let _ = client
            .batch_execute("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE READ ONLY DEFERRABLE");

        let q = |client: &mut Client, sql: &str| -> Result<Vec<postgres::Row>, PgError> {
            client
                .query(sql, &[])
                .map_err(|e| PgError::Query(e.to_string()))
        };

        let mut schema = PgSchema {
            host,
            dbname,
            ..Default::default()
        };

        for row in q(
            &mut client,
            "SELECT table_schema, table_name, table_type \
             FROM information_schema.tables \
             WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
             ORDER BY table_schema, table_name",
        )? {
            schema.tables.push(PgTable {
                schema: row.get(0),
                name: row.get(1),
                table_type: row.get(2),
            });
        }
        for row in q(
            &mut client,
            "SELECT table_schema, table_name \
             FROM information_schema.views \
             WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
             ORDER BY table_schema, table_name",
        )? {
            schema.views.push(PgView {
                schema: row.get(0),
                name: row.get(1),
            });
        }
        for row in q(
            &mut client,
            "SELECT routine_schema, routine_name, routine_type \
             FROM information_schema.routines \
             WHERE routine_schema NOT IN ('pg_catalog', 'information_schema') \
             ORDER BY routine_schema, routine_name",
        )? {
            schema.routines.push(PgRoutine {
                schema: row.get(0),
                name: row.get(1),
                routine_type: row.get(2),
            });
        }
        for row in q(
            &mut client,
            "SELECT tc.constraint_name, kcu1.table_schema, kcu1.table_name, \
                    ARRAY_AGG(kcu1.column_name ORDER BY kcu1.ordinal_position) AS columns, \
                    kcu2.table_schema AS foreign_table_schema, \
                    kcu2.table_name AS foreign_table_name, \
                    ARRAY_AGG(kcu2.column_name ORDER BY kcu2.ordinal_position) AS foreign_columns \
             FROM information_schema.table_constraints AS tc \
             JOIN information_schema.referential_constraints AS rc \
               ON tc.constraint_name = rc.constraint_name \
               AND tc.table_schema = rc.constraint_schema \
             JOIN information_schema.key_column_usage AS kcu1 \
               ON tc.constraint_name = kcu1.constraint_name \
               AND tc.table_schema = kcu1.table_schema \
             JOIN information_schema.key_column_usage AS kcu2 \
               ON rc.unique_constraint_name = kcu2.constraint_name \
               AND rc.unique_constraint_schema = kcu2.table_schema \
               AND kcu1.position_in_unique_constraint = kcu2.ordinal_position \
             WHERE tc.constraint_type = 'FOREIGN KEY' \
               AND tc.table_schema NOT IN ('pg_catalog', 'information_schema') \
             GROUP BY tc.constraint_name, kcu1.table_schema, kcu1.table_name, \
                      kcu2.table_schema, kcu2.table_name \
             ORDER BY kcu1.table_schema, kcu1.table_name",
        )? {
            schema.foreign_keys.push(PgForeignKey {
                constraint: row.get(0),
                table_schema: row.get(1),
                table_name: row.get(2),
                columns: row.get(3),
                ref_schema: row.get(4),
                ref_name: row.get(5),
                ref_columns: row.get(6),
            });
        }
        Ok(schema)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PgSchema {
        PgSchema {
            host: "localhost".into(),
            dbname: "shop".into(),
            tables: vec![
                PgTable {
                    schema: "public".into(),
                    name: "orders".into(),
                    table_type: "BASE TABLE".into(),
                },
                PgTable {
                    schema: "public".into(),
                    name: "customers".into(),
                    table_type: "BASE TABLE".into(),
                },
                // A VIEW row in information_schema.tables must be ignored here.
                PgTable {
                    schema: "public".into(),
                    name: "v_recent".into(),
                    table_type: "VIEW".into(),
                },
            ],
            views: vec![PgView {
                schema: "public".into(),
                name: "v_recent".into(),
            }],
            routines: vec![
                PgRoutine {
                    schema: "public".into(),
                    name: "total".into(),
                    routine_type: "FUNCTION".into(),
                },
                PgRoutine {
                    schema: "public".into(),
                    name: "noisy".into(),
                    routine_type: "OTHER".into(),
                },
            ],
            foreign_keys: vec![PgForeignKey {
                constraint: "orders_customer_fk".into(),
                table_schema: "public".into(),
                table_name: "orders".into(),
                ref_schema: "public".into(),
                ref_name: "customers".into(),
                columns: vec!["customer_id".into()],
                ref_columns: vec!["id".into()],
            }],
        }
    }

    #[test]
    fn emits_entities_under_a_schema_root() {
        let out = build_postgres_graph(&sample());
        let ids: Vec<&str> = out.nodes.iter().map(|n| n.id.0.as_str()).collect();
        assert!(ids.contains(&"pg_schema:localhost/shop"));
        assert!(ids.contains(&"pg_table:public.orders"));
        assert!(ids.contains(&"pg_table:public.customers"));
        assert!(ids.contains(&"pg_view:public.v_recent"));
        assert!(ids.contains(&"pg_function:public.total"));
        // The VIEW-typed table row is NOT a second table node; the OTHER routine is skipped.
        assert!(!ids.contains(&"pg_table:public.v_recent"));
        assert!(!ids.contains(&"pg_function:public.noisy"));
        // Each entity is `contains`-linked from the root.
        let contains = out
            .edges
            .iter()
            .filter(|e| e.relation == "contains" && e.source.0 == "pg_schema:localhost/shop")
            .count();
        assert_eq!(contains, 4, "orders + customers + v_recent + total");
    }

    #[test]
    fn foreign_key_becomes_a_references_edge_with_metadata() {
        let out = build_postgres_graph(&sample());
        let fk = out
            .edges
            .iter()
            .find(|e| e.relation == "references")
            .expect("a references edge");
        assert_eq!(fk.source.0, "pg_table:public.orders");
        assert_eq!(fk.target.0, "pg_table:public.customers");
        assert_eq!(fk.context.as_deref(), Some("postgres"));
        assert_eq!(
            fk.extra["metadata"]["constraint"],
            json!("orders_customer_fk")
        );
        assert_eq!(fk.extra["metadata"]["columns"], json!(["customer_id"]));
        assert_eq!(fk.extra["metadata"]["ref_columns"], json!(["id"]));
    }

    #[test]
    fn source_file_is_a_credential_free_virtual_dsn() {
        let out = build_postgres_graph(&sample());
        assert!(out
            .nodes
            .iter()
            .all(|n| n.source_file == "postgresql://localhost/shop"));
    }

    #[test]
    fn introspect_uses_the_injected_source() {
        struct Mock;
        impl SchemaSource for Mock {
            fn fetch(&self) -> Result<PgSchema, PgError> {
                Ok(sample())
            }
        }
        let out = introspect_postgres(&Mock).unwrap();
        assert!(out.nodes.iter().any(|n| n.id.0 == "pg_table:public.orders"));

        struct Boom;
        impl SchemaSource for Boom {
            fn fetch(&self) -> Result<PgSchema, PgError> {
                Err(PgError::Connection("host=secret password=hunter2".into()))
            }
        }
        assert!(matches!(
            introspect_postgres(&Boom),
            Err(PgError::Connection(_))
        ));
    }
}
