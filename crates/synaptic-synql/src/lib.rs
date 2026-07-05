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

/// Multi-graph (federated) SYNQL execution: parse once, evaluate against each
/// shard graph with `LIMIT` deferred, merge as the union graph would (rows
/// sort+dedup, matching the evaluator's ordering; aggregate counts sum per
/// group key), then apply the limit once in [`finish`](Self::finish).
/// Fold-style, so only one graph need be resident at a time.
///
/// Boundary: a relationship pattern only matches within one graph. An edge
/// that spans shards (the federation bridge) is not visible here, matching
/// per-repo isolation semantics.
pub struct FederatedRun {
    q: ast::Query,
    limit: Option<usize>,
    /// File-outline mode keeps each graph's line-sorted row order (appended in
    /// add order) instead of re-sorting lexicographically in `finish`.
    outline: bool,
    columns: Vec<String>,
    rows: Vec<Vec<NodeId>>,
    groups: std::collections::BTreeMap<Vec<String>, u64>,
    saw_agg: bool,
}

impl FederatedRun {
    /// Parse + validate `query`, deferring its `LIMIT` to the merged result.
    pub fn query(query: &str) -> Result<Self, SynqlError> {
        let mut q = parser::parse(query).map_err(SynqlError::Parse)?;
        eval::validate_query(&q).map_err(SynqlError::Parse)?;
        let limit = q.limit.take();
        Ok(FederatedRun {
            q,
            limit,
            outline: false,
            columns: Vec::new(),
            rows: Vec::new(),
            groups: std::collections::BTreeMap::new(),
            saw_agg: false,
        })
    }

    /// The federated form of [`file_outline`]: rows stay in each graph's
    /// line-sorted order.
    pub fn file_outline(file: &str) -> Result<Self, SynqlError> {
        let mut fr = Self::query(&format!(
            "MATCH (n) WHERE n.file =~ \"{}\" RETURN n",
            regex::escape(file)
        ))?;
        fr.outline = true;
        Ok(fr)
    }

    /// Evaluate against one shard graph and fold its result in.
    pub fn add(&mut self, kg: &KnowledgeGraph) {
        let r = eval::run_query(kg, &self.q);
        self.columns = r.columns;
        match r.aggregates {
            Some(agg) => {
                self.saw_agg = true;
                for row in agg {
                    // Split the formatted row back into (group key, count) by the
                    // RETURN shape; a projection with no count() sums zeros, which
                    // still dedups the keys exactly like the union's grouping.
                    let mut count: u64 = 0;
                    let mut key: Vec<String> = Vec::new();
                    for (item, cell) in self.q.ret.iter().zip(row) {
                        if matches!(item, ast::RetItem::Count(_)) {
                            count = cell.parse().unwrap_or(0);
                        } else {
                            key.push(cell);
                        }
                    }
                    *self.groups.entry(key).or_insert(0) += count;
                }
            }
            None => {
                let mut rows = r.rows;
                if self.outline {
                    rows.sort_by_key(|row| {
                        row.first()
                            .and_then(|id| kg.node(id))
                            .and_then(|n| n.span())
                            .map(|s| s.start_line)
                            .unwrap_or(u32::MAX)
                    });
                }
                self.rows.extend(rows);
            }
        }
    }

    /// Merge into the final result and apply the deferred `LIMIT`.
    pub fn finish(self) -> QueryResult {
        if self.saw_agg {
            let ret = self.q.ret;
            let mut agg: Vec<Vec<String>> = self
                .groups
                .into_iter()
                .map(|(key, count)| {
                    let mut ki = key.into_iter();
                    ret.iter()
                        .map(|item| match item {
                            ast::RetItem::Count(_) => count.to_string(),
                            _ => ki.next().unwrap_or_default(),
                        })
                        .collect()
                })
                .collect();
            if let Some(lim) = self.limit {
                agg.truncate(lim);
            }
            return QueryResult {
                columns: self.columns,
                rows: Vec::new(),
                aggregates: Some(agg),
            };
        }
        let mut rows = self.rows;
        if !self.outline {
            rows.sort();
            rows.dedup();
        }
        if let Some(lim) = self.limit {
            rows.truncate(lim);
        }
        QueryResult {
            columns: self.columns,
            rows,
            aggregates: None,
        }
    }
}

#[cfg(test)]
mod federated_tests {
    use super::*;
    use serde_json::Map;
    use synaptic_core::{FileType, GraphData, Node, NodeId};
    use synaptic_graph::KnowledgeGraph;

    fn node(id: &str, community: u32) -> Node {
        Node {
            id: NodeId(id.into()),
            label: format!("{id}()"),
            file_type: FileType::Code,
            source_file: format!("{id}.py"),
            source_location: Some("L1".into()),
            community: Some(community),
            repo: None,
            extra: Map::new(),
        }
    }

    fn kg(nodes: Vec<Node>) -> KnowledgeGraph {
        KnowledgeGraph::from_graph_data(GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes,
            links: vec![],
            hyperedges: vec![],
            built_at_commit: None,
        })
    }

    #[test]
    fn federated_rows_with_limit_match_the_union() {
        let g1 = kg(vec![node("a", 1), node("b", 1)]);
        let g2 = kg(vec![node("c", 2)]);
        let gu = kg(vec![node("a", 1), node("b", 1), node("c", 2)]);
        let q = "MATCH (n) RETURN n LIMIT 2";
        let mut fr = FederatedRun::query(q).unwrap();
        fr.add(&g1);
        fr.add(&g2);
        let merged = fr.finish();
        let union = run(&gu, q).unwrap();
        assert!(!union.rows.is_empty(), "fixture must match something");
        assert_eq!(merged, union, "LIMIT must apply after the merge");
    }

    #[test]
    fn federated_aggregate_counts_sum_across_graphs() {
        // Community 1 spans both graphs: only a summed merge gets count 3.
        let g1 = kg(vec![node("a", 1), node("b", 1)]);
        let g2 = kg(vec![node("c", 1), node("d", 2)]);
        let gu = kg(vec![node("a", 1), node("b", 1), node("c", 1), node("d", 2)]);
        let q = "MATCH (n) RETURN n.community, count(n)";
        let mut fr = FederatedRun::query(q).unwrap();
        fr.add(&g1);
        fr.add(&g2);
        let merged = fr.finish();
        let union = run(&gu, q).unwrap();
        assert_eq!(merged, union, "per-group counts must sum across graphs");
        let agg = merged.aggregates.expect("aggregate output");
        assert!(
            agg.iter()
                .any(|row| row == &vec!["1".to_string(), "3".to_string()]),
            "community 1 counts 2+1 across the graphs: {agg:?}"
        );
    }
}
