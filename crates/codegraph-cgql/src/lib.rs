//! CGQL: a structural query language + architectural pattern library over the
//! CodeGraph.
//!
//! `MATCH (c:class) WHERE c.loc > 500 AND c.fan_out > 20 RETURN c` selects nodes
//! (and relationship patterns between them) by structural properties: kind,
//! visibility, lines-of-code, fan-in/out, degree, community, name, file, language.
//! The named-pattern library ([`patterns`]) layers common architectural patterns
//! on the same engine.

pub mod ast;
pub mod eval;
pub mod lexer;
pub mod parser;
pub mod patterns;
pub mod view;

use codegraph_core::NodeId;
use codegraph_graph::KnowledgeGraph;

pub use view::NodeView;

/// A parse or evaluation error.
#[derive(Debug, thiserror::Error)]
pub enum CgqlError {
    #[error("{0}")]
    Parse(String),
}

/// The result of a query. For a plain query, `rows` holds one row of bound node
/// ids per match (sorted, de-duplicated) and `aggregates` is `None`. For an
/// aggregation/projection query (`count(...)` or a `var.field` in RETURN),
/// `aggregates` holds the scalar cells and `rows` is empty. `columns` are the
/// RETURN headers in either case.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<NodeId>>,
    pub aggregates: Option<Vec<Vec<String>>>,
}

/// Parse and run a CGQL query against `kg`.
pub fn run(kg: &KnowledgeGraph, query: &str) -> Result<QueryResult, CgqlError> {
    let q = parser::parse(query).map_err(CgqlError::Parse)?;
    eval::validate_query(&q).map_err(CgqlError::Parse)?;
    Ok(eval::run_query(kg, &q))
}

/// Parse and validate a query, returning a human-readable plan (no evaluation).
pub fn explain(query: &str) -> Result<String, CgqlError> {
    let q = parser::parse(query).map_err(CgqlError::Parse)?;
    eval::validate_query(&q).map_err(CgqlError::Parse)?;
    Ok(eval::explain_plan(&q))
}
