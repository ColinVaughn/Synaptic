//! Performance rules: indexing, SELECT *, sargability, DML safety, N+1.
use std::sync::LazyLock;

use regex::Regex;
use synaptic_core::NodeKind;

use crate::findings::{Category, Finding, Severity};
use crate::graphview::{columns_of, indexed_columns, nodes_of_kind, policies_of};
use crate::rules::{query_snippets, AuditCtx, Rule};

pub fn register(rules: &mut Vec<Box<dyn Rule>>) {
    rules.push(Box::new(MissingIndexOnFilterColumn));
    rules.push(Box::new(SelectStar));
    rules.push(Box::new(NonSargablePredicate));
    rules.push(Box::new(DmlWithoutWhere));
    rules.push(Box::new(OrderByRand));
    rules.push(Box::new(QueryInLoop));
    rules.push(Box::new(JoinComplexity));
}

pub struct MissingIndexOnFilterColumn;
pub struct SelectStar;
pub struct NonSargablePredicate;
pub struct DmlWithoutWhere;
pub struct OrderByRand;
pub struct QueryInLoop;
pub struct JoinComplexity;

impl Rule for JoinComplexity {
    fn id(&self) -> &'static str {
        "PERF-JOIN-001"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        delegate(ctx, self.id())
    }
}

#[allow(clippy::too_many_arguments)]
fn finding(
    rule: &str,
    sev: Severity,
    title: String,
    detail: &str,
    fix: &str,
    loc: Option<String>,
    ids: Vec<String>,
    snip: String,
    conf: f32,
) -> Finding {
    Finding {
        rule_id: rule.into(),
        severity: sev,
        category: Category::Performance,
        title,
        detail: detail.into(),
        location: loc,
        node_ids: ids,
        snippet: Some(snip),
        remediation: fix.into(),
        confidence: conf,
        evidence: None,
    }
}

/// Run every query-text perf/security check on one SQL string. Shared by the
/// graph audit (per code->SQL edge) and the advise path (a single candidate).
pub fn evaluate_query_text(sql: &str, loc: Option<String>, ids: Vec<String>) -> Vec<Finding> {
    static FN_ON_COL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)where[^;]*\b(lower|upper|coalesce|cast|date|substr|substring|trim)\s*\(")
            .expect("sarg fn regex")
    });
    static LEAD_WILD: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)like\s+['"]%"#).expect("sarg like regex"));
    let u = sql.to_ascii_uppercase();
    let mut out = Vec::new();
    if u.contains("SELECT *") || u.contains("SELECT\t*") {
        out.push(finding(
            "PERF-SEL-001",
            Severity::Low,
            "Query selects all columns (SELECT *)".into(),
            "SELECT * fetches columns the caller may not need and blocks covering-index-only scans.",
            "List only the columns you use.",
            loc.clone(),
            ids.clone(),
            sql.to_string(),
            0.7,
        ));
    }
    if u.contains("ORDER BY RAND()") || u.contains("ORDER BY RANDOM()") {
        out.push(finding(
            "PERF-RAND-001",
            Severity::Low,
            "ORDER BY RAND() sorts the whole table".into(),
            "Ordering by a random function materializes and sorts every row to return a few; it does not scale.",
            "Pick random rows with a keyset/TABLESAMPLE approach instead of ORDER BY RAND().",
            loc.clone(),
            ids.clone(),
            sql.to_string(),
            0.8,
        ));
    }
    if (u.starts_with("UPDATE") || u.starts_with("DELETE")) && !u.contains("WHERE") {
        out.push(finding(
            "PERF-DML-001",
            Severity::High,
            "UPDATE/DELETE without a WHERE clause".into(),
            "An UPDATE or DELETE with no WHERE rewrites or removes every row; it is almost always a bug and a heavy write.",
            "Add a WHERE clause; if a full-table change is intended, say so explicitly.",
            loc.clone(),
            ids.clone(),
            sql.to_string(),
            0.85,
        ));
    }
    if FN_ON_COL.is_match(sql) || LEAD_WILD.is_match(sql) {
        out.push(finding(
            "PERF-SARG-001",
            Severity::Medium,
            "Non-sargable predicate prevents index use".into(),
            "Wrapping a column in a function, or a LIKE with a leading wildcard, stops the optimizer from using an index and forces a scan.",
            "Compare the bare column (move the function to the literal side), store a normalized/computed column, or use a trigram/expression index.",
            loc.clone(),
            ids.clone(),
            sql.to_string(),
            0.55,
        ));
    }
    static INSERT_NO_COLS: LazyLock<Regex> = LazyLock::new(|| {
        // INSERT INTO <table> VALUES ... with no (col, col) list between the
        // table name and VALUES. The explicit-list form puts `(` after the name.
        Regex::new(r#"(?is)\binsert\s+into\s+[`"\[]?[\w.]+[`"\]]?\s+values\b"#)
            .expect("insert regex")
    });
    if INSERT_NO_COLS.is_match(sql) {
        out.push(Finding {
            rule_id: "DES-INS-001".into(),
            severity: Severity::Low,
            category: Category::Correctness,
            title: "INSERT without an explicit column list".into(),
            detail: "INSERT ... VALUES with no column list binds values to columns positionally, so adding, dropping, or reordering a column later silently sends each value to the wrong place.".into(),
            location: loc.clone(),
            node_ids: ids.clone(),
            snippet: Some(sql.to_string()),
            remediation: "List the target columns explicitly: INSERT INTO t (a, b, c) VALUES (...).".into(),
            confidence: 0.6,
            evidence: Some("INSERT INTO <table> VALUES with no column list".into()),
        });
    }
    if join_count(sql) >= 4 || or_chain_overuse(sql) {
        out.push(finding(
            "PERF-JOIN-001",
            Severity::Low,
            "Query has many joins or a repeated-column OR chain".into(),
            "Many joins, or the same column ORed against several constants, are hard for the planner to optimize and often signal a query that should be split, pre-aggregated, or expressed with IN (...)/UNION.",
            "Cut the join count (split the query, denormalize, or pre-aggregate), or replace `col = a OR col = b OR ...` with `col IN (a, b, ...)`.",
            loc.clone(),
            ids.clone(),
            sql.to_string(),
            0.4,
        ));
    }
    let inj_markers = ["' +", "+ '", "\" +", "+ \"", "${", ".format", "f\"", "|| '"];
    if inj_markers.iter().any(|m| sql.contains(m)) {
        out.push(Finding {
            rule_id: "SEC-INJ-001".into(),
            severity: Severity::Critical,
            category: Category::Security,
            title: "SQL query appears to be built by string concatenation".into(),
            detail: "Interpolating values into SQL text instead of binding parameters allows SQL injection. Use parameterized queries / prepared statements.".into(),
            location: loc,
            node_ids: ids,
            snippet: Some(sql.to_string()),
            remediation: "Replace string building with bound parameters (e.g. execute(sql, [params]) / $1 placeholders / a query builder).".into(),
            confidence: 0.6,
            evidence: Some("concatenation/interpolation marker in the query string".into()),
        });
    }
    out
}

/// Number of JOIN keywords in a query (case-insensitive whole-word).
fn join_count(sql: &str) -> usize {
    static JOIN_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\bjoin\b").expect("join regex"));
    JOIN_RE.find_iter(sql).count()
}

/// True when the WHERE clause ORs the *same* column against 3+ values — a
/// pattern an `IN (...)` expresses better and the planner optimizes more
/// reliably. (Rust's regex has no backreferences, so the repeat is counted in
/// code rather than matched in one pattern.)
fn or_chain_overuse(sql: &str) -> bool {
    static WHERE_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?is)\bwhere\b(.*?)(?:\bgroup\b|\border\b|\blimit\b|;|$)")
            .expect("where regex")
    });
    static OR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bor\b").expect("or regex"));
    static EQ_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)([a-z_][a-z0-9_.]*)\s*=").expect("eq regex"));
    let Some(c) = WHERE_RE.captures(sql) else {
        return false;
    };
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for term in OR_RE.split(&c[1]) {
        if let Some(m) = EQ_RE.captures(term) {
            *counts.entry(m[1].to_lowercase()).or_insert(0) += 1;
        }
    }
    counts.values().any(|&n| n >= 3)
}

/// Run the shared query-text checks over every code->SQL snippet, keeping the
/// findings for one rule id. Shared with sibling rule modules (design) whose
/// query-text rules live in [`evaluate_query_text`].
pub(crate) fn delegate(ctx: &AuditCtx, rule_id: &str) -> Vec<Finding> {
    query_snippets(ctx)
        .into_iter()
        .flat_map(|(s, loc, ids)| evaluate_query_text(&s, loc, ids))
        .filter(|f| f.rule_id == rule_id)
        .collect()
}

impl Rule for MissingIndexOnFilterColumn {
    fn id(&self) -> &'static str {
        "PERF-IDX-001"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        let mut out = Vec::new();
        for t in nodes_of_kind(ctx.kg, NodeKind::Table) {
            let indexed = indexed_columns(ctx.kg, &t.id);
            // 1) FK-shaped columns (*_id) with no index -> join/filter scans.
            for c in columns_of(ctx.kg, &t.id) {
                let name = c.label.to_lowercase();
                let looks_fk = name.ends_with("_id") && name != "id";
                if looks_fk && !indexed.contains(&name) {
                    out.push(Finding {
                        rule_id: self.id().into(),
                        // Name-only heuristic (column ends in `_id`, no structural
                        // evidence): Medium, not High. A wall of these must not
                        // outrank evidenced security findings (RLS gaps, injection).
                        severity: Severity::Medium,
                        category: Category::Performance,
                        title: format!(
                            "Likely-foreign-key column `{}.{}` is not indexed",
                            t.label, c.label
                        ),
                        detail: "Filtering or joining on an unindexed key forces a full table scan on every query that uses it.".into(),
                        location: c.source_location.as_ref().map(|l| format!("{}:{}", c.source_file, l)),
                        node_ids: vec![t.id.0.clone(), c.id.0.clone()],
                        snippet: None,
                        remediation: format!("CREATE INDEX ON {} ({});", t.label, c.label),
                        confidence: 0.5,
                        evidence: Some("column name ends with _id and has no covering index".into()),
                    });
                }
            }
            // 2) RLS policy filter columns with no index (PERF-IDX-002).
            for p in policies_of(ctx.kg, &t.id) {
                let expr = p
                    .extra
                    .get("using_expr")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let table_cols: Vec<String> = columns_of(ctx.kg, &t.id)
                    .iter()
                    .map(|c| c.label.to_lowercase())
                    .collect();
                for col in columns_referenced(expr) {
                    if !indexed.contains(&col) && table_cols.contains(&col) {
                        out.push(Finding {
                            rule_id: "PERF-IDX-002".into(),
                            severity: Severity::Medium,
                            category: Category::Performance,
                            title: format!("RLS filter column `{}.{}` is not indexed", t.label, col),
                            detail: "The RLS policy runs as a predicate on every query; without an index on its filter column, each request scans the whole table.".into(),
                            location: p.source_location.as_ref().map(|l| format!("{}:{}", p.source_file, l)),
                            node_ids: vec![t.id.0.clone(), p.id.0.clone()],
                            snippet: Some(expr.to_string()),
                            remediation: format!("CREATE INDEX ON {} ({});", t.label, col),
                            confidence: 0.6,
                            evidence: Some("policy predicate references an unindexed column".into()),
                        });
                    }
                }
            }
        }
        out
    }
}

/// Lowercased identifiers referenced in a predicate expression (best-effort:
/// word tokens that are not SQL keywords/literals).
fn columns_referenced(expr: &str) -> Vec<String> {
    static IDENT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").expect("ident regex"));
    let kw = [
        "and",
        "or",
        "not",
        "null",
        "true",
        "false",
        "current_setting",
        "current_user",
        "select",
        "in",
        "is",
    ];
    IDENT
        .find_iter(expr)
        .map(|m| m.as_str().to_lowercase())
        .filter(|w| !kw.contains(&w.as_str()))
        .collect()
}

impl Rule for SelectStar {
    fn id(&self) -> &'static str {
        "PERF-SEL-001"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        delegate(ctx, self.id())
    }
}

impl Rule for OrderByRand {
    fn id(&self) -> &'static str {
        "PERF-RAND-001"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        delegate(ctx, self.id())
    }
}

impl Rule for DmlWithoutWhere {
    fn id(&self) -> &'static str {
        "PERF-DML-001"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        delegate(ctx, self.id())
    }
}

impl Rule for NonSargablePredicate {
    fn id(&self) -> &'static str {
        "PERF-SARG-001"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        delegate(ctx, self.id())
    }
}

impl Rule for QueryInLoop {
    fn id(&self) -> &'static str {
        "PERF-N1-001"
    }
    fn check(&self, ctx: &AuditCtx) -> Vec<Finding> {
        static LOOP_KW: LazyLock<Regex> = LazyLock::new(|| {
            // line-start `for`/`while` (most languages), or a method-chain
            // `.map(` / `.each(` / `.forEach(` (JS/Ruby iteration).
            Regex::new(r"(?im)^\s*(for|while)\b|\.(map|each|for_?each)\s*\(").expect("loop regex")
        });
        let Some(root) = ctx.root else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for e in ctx
            .kg
            .edges()
            .filter(|e| e.relation == "queries" || e.relation == "writes_to")
        {
            let Some(loc) = e
                .source_location
                .as_ref()
                .and_then(|l| l.trim_start_matches('L').parse::<usize>().ok())
            else {
                continue;
            };
            let path = root.join(&e.source_file);
            let Ok(src) = std::fs::read_to_string(&path) else {
                continue;
            };
            let lines: Vec<&str> = src.lines().collect();
            if loc == 0 || loc > lines.len() {
                continue;
            }
            let start = loc.saturating_sub(15);
            let in_loop = lines[start..loc.min(lines.len())]
                .iter()
                .any(|l| LOOP_KW.is_match(l));
            if in_loop {
                out.push(Finding {
                    rule_id: self.id().into(),
                    severity: Severity::High,
                    category: Category::Performance,
                    title: "Query executed inside a loop (possible N+1)".into(),
                    detail: "Running a query once per loop iteration multiplies round-trips. Fetch the set in one query (IN / JOIN / batch) instead.".into(),
                    location: Some(format!("{}:L{}", e.source_file, loc)),
                    node_ids: vec![e.source.0.clone(), e.target.0.clone()],
                    snippet: e.extra.get("sql").and_then(|v| v.as_str()).map(str::to_string),
                    remediation: "Hoist the query out of the loop: load all needed rows with a single WHERE ... IN (...) or a JOIN.".into(),
                    confidence: 0.45,
                    evidence: Some("a loop header appears just above the query call site".into()),
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

    fn ctx(kg: &synaptic_graph::KnowledgeGraph) -> AuditCtx<'_> {
        AuditCtx { kg, root: None }
    }
    fn graph_from(j: serde_json::Value) -> synaptic_graph::KnowledgeGraph {
        synaptic_graph::KnowledgeGraph::from_graph_data(serde_json::from_value(j).unwrap())
    }

    #[test]
    fn flags_unindexed_fk_column() {
        let kg = graph_from(json!({
            "nodes": [
                {"id":"sql:orders","label":"orders","file_type":"code","source_file":"s.sql","kind":"table"},
                {"id":"sql:orders:col:customer_id","label":"customer_id","file_type":"code","source_file":"s.sql","kind":"column"}
            ],
            "links": [
                {"source":"sql:orders","target":"sql:orders:col:customer_id","relation":"has_column","confidence":"EXTRACTED","source_file":"s.sql"}
            ]
        }));
        let f = MissingIndexOnFilterColumn.check(&ctx(&kg));
        let idx = f
            .iter()
            .find(|x| x.rule_id == "PERF-IDX-001")
            .unwrap_or_else(|| panic!("{f:?}"));
        // A pure name heuristic (column ends in `_id`, 0.5 confidence) is Medium,
        // not High: it must not outrank structurally-evidenced security findings.
        assert_eq!(idx.severity, Severity::Medium);
    }

    #[test]
    fn flags_select_star_and_order_by_rand() {
        let kg = graph_from(json!({
            "nodes": [
                {"id":"app.f","label":"f()","file_type":"code","source_file":"a.py","kind":"function"},
                {"id":"sql:t","label":"t","file_type":"code","source_file":"s.sql","kind":"table"}
            ],
            "links": [{"source":"app.f","target":"sql:t","relation":"queries","confidence":"INFERRED","source_file":"a.py","source_location":"L2","sql":"SELECT * FROM t ORDER BY RANDOM()"}]
        }));
        assert!(SelectStar
            .check(&ctx(&kg))
            .iter()
            .any(|x| x.rule_id == "PERF-SEL-001"));
        assert!(OrderByRand
            .check(&ctx(&kg))
            .iter()
            .any(|x| x.rule_id == "PERF-RAND-001"));
    }

    #[test]
    fn flags_update_without_where() {
        let kg = graph_from(json!({
            "nodes": [
                {"id":"app.f","label":"f()","file_type":"code","source_file":"a.py","kind":"function"},
                {"id":"sql:t","label":"t","file_type":"code","source_file":"s.sql","kind":"table"}
            ],
            "links": [{"source":"app.f","target":"sql:t","relation":"writes_to","confidence":"INFERRED","source_file":"a.py","source_location":"L2","sql":"UPDATE t SET active = 1"}]
        }));
        assert!(DmlWithoutWhere
            .check(&ctx(&kg))
            .iter()
            .any(|x| x.rule_id == "PERF-DML-001"));
    }

    #[test]
    fn flags_non_sargable_function_on_column() {
        let kg = graph_from(json!({
            "nodes": [
                {"id":"app.f","label":"f()","file_type":"code","source_file":"a.py","kind":"function"},
                {"id":"sql:t","label":"t","file_type":"code","source_file":"s.sql","kind":"table"}
            ],
            "links": [{"source":"app.f","target":"sql:t","relation":"queries","confidence":"INFERRED","source_file":"a.py","source_location":"L2","sql":"SELECT id FROM t WHERE LOWER(email) = 'x'"}]
        }));
        assert!(NonSargablePredicate
            .check(&ctx(&kg))
            .iter()
            .any(|x| x.rule_id == "PERF-SARG-001"));
    }

    #[test]
    fn flags_n1_query_in_loop() {
        let dir = tempfile::tempdir().unwrap();
        let src = "def f(items, conn):\n    for it in items:\n        conn.execute(\"SELECT * FROM t WHERE id = ?\")\n";
        std::fs::write(dir.path().join("app.py"), src).unwrap();
        let kg = graph_from(json!({
            "nodes": [
                {"id":"app.f","label":"f()","file_type":"code","source_file":"app.py","kind":"function"},
                {"id":"sql:t","label":"t","file_type":"code","source_file":"s.sql","kind":"table"}
            ],
            "links": [{"source":"app.f","target":"sql:t","relation":"queries","confidence":"INFERRED","source_file":"app.py","source_location":"L3","sql":"SELECT * FROM t WHERE id = ?"}]
        }));
        let cx = AuditCtx {
            kg: &kg,
            root: Some(dir.path()),
        };
        let f = QueryInLoop.check(&cx);
        assert!(f.iter().any(|x| x.rule_id == "PERF-N1-001"), "{f:?}");
    }

    fn query_graph(sql: &str) -> synaptic_graph::KnowledgeGraph {
        graph_from(json!({
            "nodes": [
                {"id":"app.f","label":"f()","file_type":"code","source_file":"a.py","kind":"function"},
                {"id":"sql:t","label":"t","file_type":"code","source_file":"s.sql","kind":"table"}
            ],
            "links": [{"source":"app.f","target":"sql:t","relation":"queries","confidence":"INFERRED","source_file":"a.py","source_location":"L2","sql":sql}]
        }))
    }

    #[test]
    fn flags_excessive_joins() {
        let kg = query_graph("SELECT * FROM a JOIN b ON a.id=b.a JOIN c ON b.id=c.b JOIN d ON c.id=d.c JOIN e ON d.id=e.d");
        assert!(JoinComplexity
            .check(&ctx(&kg))
            .iter()
            .any(|x| x.rule_id == "PERF-JOIN-001"));
    }

    #[test]
    fn flags_or_chain_that_should_be_in() {
        let kg = query_graph("SELECT id FROM t WHERE status = 'a' OR status = 'b' OR status = 'c'");
        assert!(JoinComplexity
            .check(&ctx(&kg))
            .iter()
            .any(|x| x.rule_id == "PERF-JOIN-001"));
    }

    #[test]
    fn simple_join_and_distinct_column_or_are_not_flagged() {
        let kg = query_graph("SELECT * FROM a JOIN b ON a.id = b.a WHERE a.x = 1 OR a.y = 2");
        assert!(JoinComplexity
            .check(&ctx(&kg))
            .iter()
            .all(|x| x.rule_id != "PERF-JOIN-001"));
    }

    #[test]
    fn flags_n1_query_in_js_foreach() {
        let dir = tempfile::tempdir().unwrap();
        let src = "function f(items, conn) {\n  items.forEach(function (it) {\n    conn.query(\"SELECT * FROM t WHERE id = ?\");\n  });\n}\n";
        std::fs::write(dir.path().join("app.js"), src).unwrap();
        let kg = graph_from(json!({
            "nodes": [
                {"id":"app.f","label":"f()","file_type":"code","source_file":"app.js","kind":"function"},
                {"id":"sql:t","label":"t","file_type":"code","source_file":"s.sql","kind":"table"}
            ],
            "links": [{"source":"app.f","target":"sql:t","relation":"queries","confidence":"INFERRED","source_file":"app.js","source_location":"L3","sql":"SELECT * FROM t WHERE id = ?"}]
        }));
        let cx = AuditCtx {
            kg: &kg,
            root: Some(dir.path()),
        };
        assert!(QueryInLoop
            .check(&cx)
            .iter()
            .any(|x| x.rule_id == "PERF-N1-001"));
    }
}
