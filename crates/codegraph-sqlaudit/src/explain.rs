//! Query-plan seam. The default `NoPlan` provider returns nothing (the static
//! audit is unchanged). A live provider (feature `live-explain`) runs EXPLAIN
//! and parses the output into a PlanSignal. The parser is pure and tested here.
use std::sync::LazyLock;

use regex::Regex;

/// What a query plan tells us, normalized across engines.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanSignal {
    /// The plan contains a sequential/full table scan.
    pub seq_scan: bool,
    /// Estimated top-level cost, if the engine reported one.
    pub est_cost: Option<f64>,
    /// Estimated row count, if reported.
    pub est_rows: Option<u64>,
    /// The raw EXPLAIN text (truncated by the caller for findings).
    pub raw: String,
}

/// Supplies a plan for a SQL string. Implementations may hit a live database.
pub trait PlanProvider {
    fn explain(&self, sql: &str) -> Option<PlanSignal>;
}

/// The default: no plan available (static-only audit).
pub struct NoPlan;
impl PlanProvider for NoPlan {
    fn explain(&self, _sql: &str) -> Option<PlanSignal> {
        None
    }
}

/// Parse a PostgreSQL `EXPLAIN` text output into a PlanSignal. Pure; offline.
/// Looks for "Seq Scan" and the first "cost=..rows=" estimate.
pub fn parse_pg_explain(text: &str) -> PlanSignal {
    static COST_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"cost=[0-9.]+\.\.([0-9.]+)\s+rows=([0-9]+)").expect("cost regex")
    });
    let seq_scan = text.contains("Seq Scan");
    let mut est_cost = None;
    let mut est_rows = None;
    if let Some(c) = COST_RE.captures(text) {
        est_cost = c.get(1).and_then(|m| m.as_str().parse::<f64>().ok());
        est_rows = c.get(2).and_then(|m| m.as_str().parse::<u64>().ok());
    }
    PlanSignal {
        seq_scan,
        est_cost,
        est_rows,
        raw: text.chars().take(2000).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noplan_returns_none() {
        assert!(NoPlan.explain("SELECT 1").is_none());
    }

    #[test]
    fn parses_pg_seq_scan_and_cost() {
        let explain =
            "Seq Scan on orders  (cost=0.00..431.00 rows=25000 width=44)\n  Filter: (tenant_id = 1)";
        let p = parse_pg_explain(explain);
        assert!(p.seq_scan);
        assert_eq!(p.est_cost, Some(431.00));
        assert_eq!(p.est_rows, Some(25000));
    }

    #[test]
    fn parses_index_scan_as_no_seq_scan() {
        let explain =
            "Index Scan using ix_orders_tenant on orders  (cost=0.29..8.31 rows=1 width=44)";
        let p = parse_pg_explain(explain);
        assert!(!p.seq_scan);
        assert_eq!(p.est_rows, Some(1));
    }
}
