//! Live EXPLAIN provider (compiled only under the `live-explain` feature).
//! Connects with sqlx and runs `EXPLAIN <query>`, parsing the text output.
//! Read-only: it runs EXPLAIN (never EXPLAIN ANALYZE), so it does not execute
//! the audited query.
use crate::explain::{parse_pg_explain, PlanProvider, PlanSignal};

/// Connects to `db_url` (postgres://, mysql://, sqlite://) and EXPLAINs queries.
/// One pool is opened at construction and reused for every query.
pub struct LiveExplain {
    pool: sqlx::AnyPool,
    rt: tokio::runtime::Runtime,
}

impl LiveExplain {
    /// Build a provider for a connection URL, opening one pooled connection.
    /// `None` if the runtime or the connection cannot be established.
    pub fn new(db_url: &str) -> Option<Self> {
        sqlx::any::install_default_drivers();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        let pool = rt.block_on(async { sqlx::AnyPool::connect(db_url).await.ok() })?;
        Some(LiveExplain { pool, rt })
    }
}

impl PlanProvider for LiveExplain {
    fn explain(&self, sql: &str) -> Option<PlanSignal> {
        use sqlx::Row;
        // Defense-in-depth: only EXPLAIN read queries, never anything that could
        // execute a write. (The caller already filters to SELECT, but the
        // provider enforces it too.)
        let head = sql.trim_start().to_ascii_uppercase();
        if !head.starts_with("SELECT") && !head.starts_with("WITH") {
            return None;
        }
        let explain_sql = format!("EXPLAIN {sql}");
        let pool = self.pool.clone();
        let text: Option<String> = self.rt.block_on(async move {
            let rows = sqlx::query(&explain_sql).fetch_all(&pool).await.ok()?;
            let mut out = String::new();
            for r in rows {
                // EXPLAIN returns one text column per line (Postgres "QUERY PLAN").
                if let Ok(line) = r.try_get::<String, _>(0) {
                    out.push_str(&line);
                    out.push('\n');
                }
            }
            Some(out)
        });
        text.map(|t| parse_pg_explain(&t))
    }
}
