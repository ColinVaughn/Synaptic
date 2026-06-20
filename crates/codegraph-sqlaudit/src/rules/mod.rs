//! The rule engine: a Rule yields Findings from an AuditCtx (the graph plus an
//! optional repo root for re-reading SQL/source). Rules are registered in
//! [`all_rules`] and grow per category module.
use std::path::Path;

use codegraph_graph::KnowledgeGraph;

use crate::findings::Finding;

pub mod design;
pub mod performance;
pub mod security;

pub struct AuditCtx<'a> {
    pub kg: &'a KnowledgeGraph,
    /// Repo root for re-reading source at a call site (N+1 detection). None ->
    /// source-dependent rules skip.
    pub root: Option<&'a Path>,
}

pub trait Rule {
    fn id(&self) -> &'static str;
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding>;
}

/// Every rule in the catalog. Order is irrelevant (the report re-sorts).
pub fn all_rules() -> Vec<Box<dyn Rule>> {
    let mut rules: Vec<Box<dyn Rule>> = Vec::new();
    security::register(&mut rules);
    performance::register(&mut rules);
    design::register(&mut rules);
    rules
}

/// Query strings on code->table edges, with their source location ("file:Lnn")
/// and the [source_id, target_id] pair. Used by the query-text rules.
///
/// Snippets that do not parse as real SQL (after placeholder normalization) are
/// dropped here, so prose/UI strings that slipped past the extractor's clause gate
/// never reach the text rules and produce false findings.
pub fn query_snippets(ctx: &AuditCtx) -> Vec<(String, Option<String>, Vec<String>)> {
    let rels = ["queries", "writes_to"];
    ctx.kg
        .edges()
        .filter(|e| rels.contains(&e.relation.as_str()))
        .filter_map(|e| {
            let snip = e.extra.get("sql").and_then(|v| v.as_str())?.to_string();
            if !sql_parses(&snip) {
                return None;
            }
            let loc = e
                .source_location
                .as_ref()
                .map(|l| format!("{}:{}", e.source_file, l));
            Some((snip, loc, vec![e.source.0.clone(), e.target.0.clone()]))
        })
        .collect()
}

/// True when `sql` parses as at least one SQL statement once host-language
/// placeholders are normalized to literals. Used to gate the query-text rules so
/// only genuine queries are evaluated.
pub fn sql_parses(sql: &str) -> bool {
    use std::sync::LazyLock;

    use regex::Regex;
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;

    // Bound parse cost: a real captured query is at most the 400-char snippet.
    if sql.trim().is_empty() || sql.len() > 2000 {
        return false;
    }
    // Normalize the common host-language placeholder forms to a literal `1` so the
    // generic dialect can parse parameterized queries: $1, ?, :name, ${expr},
    // %s / %(name)s, @name. The `::` cast operator is matched first and passed
    // through unchanged so a `:name` alternative cannot eat the second colon of a
    // Postgres `col::type` cast (which would corrupt valid SQL into a parse miss).
    static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"::|\$\{[^}]*\}|%\([A-Za-z0-9_]+\)s|%s|\$\d+|:[A-Za-z_][A-Za-z0-9_]*|@[A-Za-z_][A-Za-z0-9_]*|\?")
            .expect("placeholder regex")
    });
    let normalized = PLACEHOLDER.replace_all(sql, |c: &regex::Captures| {
        if &c[0] == "::" {
            "::".to_string()
        } else {
            "1".to_string()
        }
    });
    matches!(Parser::parse_sql(&GenericDialect {}, &normalized), Ok(stmts) if !stmts.is_empty())
}

#[cfg(test)]
mod tests {
    use codegraph_core::GraphData;
    use codegraph_graph::KnowledgeGraph;

    #[test]
    fn audit_runs_on_empty_graph() {
        let kg = KnowledgeGraph::from_graph_data(GraphData::default());
        let r = crate::audit(&kg, &crate::AuditOptions::default());
        assert_eq!(r.findings.len(), 0);
        assert_eq!(r.version, crate::findings::AUDIT_VERSION);
    }

    #[test]
    fn sql_parses_rejects_prose_accepts_queries() {
        use super::sql_parses;
        // Prose that begins with a SQL verb but is not a parseable statement
        // (UPDATE without SET fails the parser). DELETE-prose without FROM is
        // blocked earlier, by the extractor's clause gate, so it never reaches here.
        assert!(!sql_parses("Update password"));
        assert!(!sql_parses("Update the file and clear any caches/CDN"));
        // Genuine queries, including parameterized forms.
        assert!(sql_parses("SELECT id FROM users WHERE id = $1"));
        assert!(sql_parses("UPDATE accounts SET balance = 0 WHERE id = ?"));
        assert!(sql_parses("DELETE FROM sessions WHERE expired = :flag"));
        assert!(sql_parses("INSERT INTO logs (msg) VALUES (${msg})"));
        // Postgres `::` casts must survive placeholder normalization (the `:name`
        // rule must not eat the second colon).
        assert!(sql_parses("SELECT id::text FROM users WHERE id = $1"));
        assert!(sql_parses(
            "SELECT id FROM users WHERE created::date = :day"
        ));
    }
}
