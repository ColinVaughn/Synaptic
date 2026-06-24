//! SYNQL: a structural query language + architectural pattern library over the
//! Synaptic.
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

use synaptic_core::NodeId;
use synaptic_graph::KnowledgeGraph;

pub use view::NodeView;

/// A parse or evaluation error.
#[derive(Debug, thiserror::Error)]
pub enum SynqlError {
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

/// Parse and run a SYNQL query against `kg`.
pub fn run(kg: &KnowledgeGraph, query: &str) -> Result<QueryResult, SynqlError> {
    let q = parser::parse(query).map_err(SynqlError::Parse)?;
    eval::validate_query(&q).map_err(SynqlError::Parse)?;
    Ok(eval::run_query(kg, &q))
}

/// List every symbol defined in `file` (a path substring), ordered by line -- a
/// file outline. A convenience wrapper over `MATCH (n) WHERE n.file =~ "<file>"
/// RETURN n` with the file string regex-escaped so a path matches literally, and
/// the rows sorted by the symbol's start line. Shared by the `structural_search`
/// MCP tool's `file` param and the `synaptic search --file` CLI flag.
pub fn file_outline(kg: &KnowledgeGraph, file: &str) -> Result<QueryResult, SynqlError> {
    let q = format!(
        "MATCH (n) WHERE n.file =~ \"{}\" RETURN n",
        regex::escape(file)
    );
    let mut r = run(kg, &q)?;
    r.rows.sort_by_key(|row| {
        row.first()
            .and_then(|id| kg.node(id))
            .and_then(|n| n.span())
            .map(|s| s.start_line)
            .unwrap_or(u32::MAX)
    });
    Ok(r)
}

/// Parse and validate a query, returning a human-readable plan (no evaluation).
pub fn explain(query: &str) -> Result<String, SynqlError> {
    let q = parser::parse(query).map_err(SynqlError::Parse)?;
    eval::validate_query(&q).map_err(SynqlError::Parse)?;
    Ok(eval::explain_plan(&q))
}
