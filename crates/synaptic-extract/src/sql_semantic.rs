//! Semantic SQL enrichment over the tree-sitter object graph: adds NodeKinds,
//! column/index nodes (via sqlparser), and RLS/policy/grant facts (regex). All
//! best-effort: a parse failure leaves the base object graph untouched.

use regex::Regex;
use serde_json::{json, Map, Value};
use sqlparser::ast::{ColumnOption, Expr, Statement, TableConstraint};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use synaptic_core::{make_id, Confidence, Edge, FileType, Node, NodeId, NodeKind};

use crate::result::ExtractionResult;

/// Process-wide switch for emitting SQL column + index nodes (default on). The
/// `extract --no-columns` flag flips it off before a run to bound graph.json on
/// column-heavy schemas (columns can be 10-50x table count). It is a global
/// rather than a threaded option because the per-file extractor dispatch
/// (`extract_source`) has a fixed signature shared by every language and runs in
/// parallel; the flag is set once, before the run starts.
static EMIT_SQL_COLUMNS: AtomicBool = AtomicBool::new(true);

/// Whether SQL column/index nodes are currently emitted.
pub fn emit_sql_columns() -> bool {
    EMIT_SQL_COLUMNS.load(Ordering::Relaxed)
}

/// Set whether SQL column/index nodes are emitted. Call once before extraction.
pub fn set_emit_sql_columns(on: bool) {
    EMIT_SQL_COLUMNS.store(on, Ordering::Relaxed);
}

static RLS_ENABLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\balter\s+table\s+[`"\[]?([\w.]+)[`"\]]?\s+(enable|force)\s+row\s+level\s+security"#)
        .expect("rls enable regex")
});
static POLICY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)\bcreate\s+policy\s+[`"\[]?(\w+)[`"\]]?\s+on\s+[`"\[]?([\w.]+)[`"\]]?(.*?)(?:;|$)"#,
    )
    .expect("policy regex")
});
static USING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\busing\s*\((.*?)\)\s*(?:with\s+check|$|;)"#).expect("using regex")
});
static WITH_CHECK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\bwith\s+check\s*\((.*?)\)\s*(?:$|;)"#).expect("with check regex")
});
static GRANT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)\bgrant\s+(.+?)\s+on\s+(?:table\s+)?[`"\[]?([\w.]+)[`"\]]?\s+to\s+[`"\[]?(\w+)"#,
    )
    .expect("grant regex")
});
static MSSQL_SECPOL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)\bcreate\s+security\s+policy\s+[`"\[]?(\w+)[`"\]]?.*?\bon\s+[`"\[]?([\w.]+)"#,
    )
    .expect("mssql secpol regex")
});
static VIEW_WITH_RE: LazyLock<Regex> = LazyLock::new(|| {
    // The name may be schema-qualified (`public.v`); capture the dotted form and
    // reduce to the last identifier (matching the `sql:<name>` node id scheme).
    Regex::new(
        r#"(?is)\bcreate\s+(?:or\s+replace\s+)?(?:temp\w*\s+|materialized\s+)?view\s+[`"\[]?([\w.]+)[`"\]]?\s+with\s*\(([^)]*)\)"#,
    )
    .expect("view with-options regex")
});
static SECURITY_INVOKER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bsecurity_invoker\b\s*=?\s*(?:true|on|1|yes)\b")
        .expect("security_invoker regex")
});

/// Enrich an already-extracted `.sql` result in place. Never panics on bad SQL.
/// `emit_columns` gates the (large) column + index node passes.
pub fn enrich(_path: &str, source: &[u8], emit_columns: bool, result: &mut ExtractionResult) {
    let sql = String::from_utf8_lossy(source);
    set_object_kinds(result);
    enrich_dialect(&sql, result);
    // Column + index nodes are the bulk of a SQL graph; --no-columns skips them
    // (and their edges) while keeping table/RLS/policy/grant/view facts.
    if emit_columns {
        if detect_dialect(&sql) == "sqlserver" {
            // sqlparser can't parse bracketed T-SQL DDL; recover via regex.
            enrich_columns_tsql(&sql, result);
            enrich_tsql_constraints(&sql, result);
            enrich_tsql_indexes(&sql, result);
        } else {
            enrich_columns(&sql, result);
            enrich_indexes(&sql, result);
        }
    }
    enrich_security(&sql, result);
    enrich_views(&sql, result);
}

/// Mark a view node `security_invoker` when its DDL sets that option true. The
/// absence of the flag is read as "owner rights" (the RLS-bypass default), so
/// only the truthy case is recorded.
fn enrich_views(sql: &str, result: &mut ExtractionResult) {
    for caps in VIEW_WITH_RE.captures_iter(sql) {
        if !SECURITY_INVOKER_RE.is_match(&caps[2]) {
            continue;
        }
        let name = last_ident(&caps[1]);
        let vid = NodeId(make_id(&["sql", &name]));
        if let Some(v) = result.nodes.iter_mut().find(|n| n.id == vid) {
            v.extra.insert("security_invoker".into(), json!(true));
        }
    }
}

/// File-level SQL dialect from marker syntax. Only distinguishes SQL Server
/// (T-SQL) from a generic dialect today, so SQL Server-specific rules (security
/// policies) fire only on SQL Server schemas.
fn detect_dialect(sql: &str) -> &'static str {
    static TSQL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?im)\bcreate\s+security\s+policy\b|\bnvarchar\b|\buniqueidentifier\b|\bdatetime2\b|^\s*go\s*$|\[[A-Za-z_]\w*\]",
        )
        .expect("tsql markers regex")
    });
    if TSQL.is_match(sql) {
        "sqlserver"
    } else {
        "generic"
    }
}

/// Tag every table node with the detected dialect. Done over the node list
/// (not the sqlparser pass) so the tag is set even when a statement fails to
/// parse.
fn enrich_dialect(sql: &str, result: &mut ExtractionResult) {
    let dialect = detect_dialect(sql);
    for n in result.nodes.iter_mut() {
        if n.kind() == Some(NodeKind::Table) {
            n.extra.insert("dialect".into(), json!(dialect));
        }
    }
}

/// Set a NodeKind on each SQL object node from its `contains` edge context
/// (the tree-sitter pass tags table/view/function/procedure/trigger there).
fn set_object_kinds(result: &mut ExtractionResult) {
    let tagged: Vec<(NodeId, NodeKind)> = result
        .edges
        .iter()
        .filter(|e| e.relation == "contains")
        .filter_map(|e| {
            let kind = match e.context.as_deref()? {
                "table" => NodeKind::Table,
                "view" => NodeKind::View,
                "function" => NodeKind::Function,
                "procedure" => NodeKind::Procedure,
                "trigger" => NodeKind::Trigger,
                _ => return None,
            };
            Some((e.target.clone(), kind))
        })
        .collect();
    for (id, kind) in tagged {
        if let Some(n) = result.nodes.iter_mut().find(|n| n.id == id) {
            if n.kind().is_none() {
                n.set_kind(kind);
            }
        }
    }
}

/// Parse statement-tolerantly: try the whole file, else per-`;` chunk, skipping
/// chunks sqlparser rejects so one bad statement does not drop the rest.
fn parse_tolerant(sql: &str) -> Vec<Statement> {
    let dialect = GenericDialect {};
    if let Ok(stmts) = Parser::parse_sql(&dialect, sql) {
        return stmts;
    }
    sql.split(';')
        .filter_map(|chunk| {
            let c = chunk.trim();
            if c.is_empty() {
                return None;
            }
            Parser::parse_sql(&dialect, c).ok()
        })
        .flatten()
        .collect()
}

/// The (last) identifier of a possibly-qualified object name (`schema.table` ->
/// `table`), with surrounding quotes/brackets stripped. Callers lowercase it to
/// match the `make_id(["sql", name])` id scheme.
fn object_name(name: &sqlparser::ast::ObjectName) -> String {
    name.0
        .last()
        .map(|i| i.to_string())
        .unwrap_or_default()
        .trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']')
        .to_string()
}

fn col_id(table_lower: &str, col_lower: &str) -> NodeId {
    NodeId(make_id(&["sql", table_lower, "col", col_lower]))
}

fn add_node(
    result: &mut ExtractionResult,
    id: NodeId,
    label: &str,
    kind: NodeKind,
    extra: Map<String, Value>,
) {
    if result.nodes.iter().any(|n| n.id == id) {
        return;
    }
    let mut n = Node {
        id,
        label: label.to_string(),
        file_type: FileType::Code,
        source_file: String::new(),
        source_location: None,
        community: None,
        repo: None,
        extra,
    };
    n.set_kind(kind);
    result.nodes.push(n);
}

fn add_edge(
    result: &mut ExtractionResult,
    source: NodeId,
    target: NodeId,
    relation: &str,
    context: &str,
) {
    result.edges.push(Edge {
        source,
        target,
        relation: relation.to_string(),
        confidence: Confidence::Extracted,
        source_file: String::new(),
        source_location: None,
        confidence_score: None,
        weight: 1.0,
        context: Some(context.to_string()),
        cross_repo: false,
        extra: Map::new(),
    });
}

/// Emit a Column node + `has_column` edge for every column of every CREATE TABLE.
fn enrich_columns(sql: &str, result: &mut ExtractionResult) {
    for stmt in parse_tolerant(sql) {
        let Statement::CreateTable(ct) = stmt else {
            continue;
        };
        let table_lower = object_name(&ct.name).to_lowercase();
        if table_lower.is_empty() {
            continue;
        }
        let table_id = NodeId(make_id(&["sql", &table_lower]));
        if let Some(t) = result.nodes.iter_mut().find(|n| n.id == table_id) {
            // dialect is set by enrich_dialect. Default RLS off so "tables
            // without row-level security" is a graph query; enrich_security
            // overrides this to true on ENABLE.
            t.extra.entry("rls_enabled").or_insert(json!(false));
        }
        // collect PK columns from table-level PRIMARY KEY constraints
        let pk_constraint_cols: std::collections::HashSet<String> = ct
            .constraints
            .iter()
            .filter_map(|c| {
                if let TableConstraint::PrimaryKey { columns, .. } = c {
                    Some(columns)
                } else {
                    None
                }
            })
            .flatten()
            .map(|ident| {
                ident
                    .to_string()
                    .trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']')
                    .to_lowercase()
            })
            .collect();
        // table-level FOREIGN KEY (col, ...) REFERENCES target: column -> target.
        let mut fk_constraint_cols: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for c in &ct.constraints {
            if let TableConstraint::ForeignKey {
                columns,
                foreign_table,
                ..
            } = c
            {
                let target = object_name(foreign_table).to_lowercase();
                for col in columns {
                    let key = col
                        .to_string()
                        .trim_matches(|ch| ch == '"' || ch == '`' || ch == '[' || ch == ']')
                        .to_lowercase();
                    fk_constraint_cols.insert(key, target.clone());
                }
            }
        }
        for col in &ct.columns {
            let name = col.name.to_string();
            let name_lower = name.to_lowercase();
            let not_null = col
                .options
                .iter()
                .any(|o| matches!(o.option, ColumnOption::NotNull));
            let inline_pk = col.options.iter().any(|o| {
                matches!(
                    o.option,
                    ColumnOption::Unique {
                        is_primary: true,
                        ..
                    }
                )
            });
            let is_pk = inline_pk || pk_constraint_cols.contains(&name_lower);
            let mut extra = Map::new();
            extra.insert("_origin".into(), json!("ast"));
            extra.insert("data_type".into(), json!(col.data_type.to_string()));
            extra.insert("nullable".into(), json!(!not_null && !is_pk));
            extra.insert("pk".into(), json!(is_pk));
            // fk_target: inline `REFERENCES t` on the column, else a table-level
            // FOREIGN KEY naming it. Lets DES-FK-001 exempt real foreign keys.
            let fk_target = col
                .options
                .iter()
                .find_map(|o| match &o.option {
                    ColumnOption::ForeignKey { foreign_table, .. } => {
                        Some(object_name(foreign_table).to_lowercase())
                    }
                    _ => None,
                })
                .or_else(|| fk_constraint_cols.get(&name_lower).cloned());
            if let Some(t) = fk_target {
                extra.insert("fk_target".into(), json!(t));
            }
            let cid = col_id(&table_lower, &name_lower);
            add_node(result, cid.clone(), &name, NodeKind::Column, extra);
            add_edge(result, table_id.clone(), cid, "has_column", "column");
        }
    }
}

/// T-SQL CREATE TABLE column extraction (regex). sqlparser's GenericDialect
/// cannot parse bracketed identifiers, bracketed types (`[int]`), IDENTITY,
/// CLUSTERED, `ON [PRIMARY]`, or the IF/BEGIN/END + GO batch wrapping that real
/// T-SQL DDL uses, so for SQL Server files we recover columns, primary keys, and
/// foreign keys from the text. Also emits an index node per PRIMARY KEY / UNIQUE
/// constraint so PERF-IDX rules see those columns as indexed.
fn enrich_columns_tsql(sql: &str, result: &mut ExtractionResult) {
    static TABLE_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?is)\bcreate\s+table\s+((?:\[[^\]]+\]|"[^"]+"|\w+)(?:\.(?:\[[^\]]+\]|"[^"]+"|\w+))*)\s*\("#,
        )
        .expect("tsql table regex")
    });
    static COLDEF_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?is)^\s*(\[[^\]]+\]|"[^"]+"|\w+)\s+(\[[^\]]+\]|\w+)"#).expect("tsql coldef")
    });
    static NOT_NULL_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)\bnot\s+null\b").expect("notnull"));
    static INLINE_PK_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)\bprimary\s+key\b").expect("inline pk"));
    static IDENTITY_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)\bidentity\b").expect("identity"));

    for m in TABLE_RE.captures_iter(sql) {
        let table_lower = last_ident(&m[1]);
        if table_lower.is_empty() {
            continue;
        }
        let open = m.get(0).unwrap().end() - 1; // index of the '('
        let Some(block) = balanced_block(sql, open) else {
            continue;
        };
        let table_id = NodeId(make_id(&["sql", &table_lower]));
        if let Some(t) = result.nodes.iter_mut().find(|n| n.id == table_id) {
            t.extra.entry("rls_enabled").or_insert(json!(false));
        }

        let mut pk_cols: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut fk_cols: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut indexed: Vec<Vec<String>> = Vec::new();
        // (name, data_type, nullable, inline_pk, identity, fk_target)
        let mut cols: Vec<(String, String, bool, bool, bool, Option<String>)> = Vec::new();

        for item in split_top_level(&block) {
            let upper = item.trim_start().to_ascii_uppercase();
            let is_constraint = upper.starts_with("CONSTRAINT")
                || upper.starts_with("PRIMARY KEY")
                || upper.starts_with("UNIQUE")
                || upper.starts_with("FOREIGN KEY")
                || upper.starts_with("CHECK")
                || upper.starts_with("INDEX")
                || upper.starts_with("PERIOD");
            if is_constraint {
                if upper.contains("PRIMARY KEY") {
                    let c = constraint_columns(&item);
                    pk_cols.extend(c.iter().cloned());
                    indexed.push(c);
                } else if upper.contains("UNIQUE") {
                    indexed.push(constraint_columns(&item));
                } else if upper.contains("FOREIGN KEY") {
                    if let Some(t) = references_target(&item) {
                        for c in constraint_columns(&item) {
                            fk_cols.insert(c, t.clone());
                        }
                    }
                }
                continue;
            }
            let Some(caps) = COLDEF_RE.captures(&item) else {
                continue;
            };
            let name = strip_brackets(&caps[1]);
            if name.is_empty() {
                continue;
            }
            let dtype = strip_brackets(&caps[2]);
            let not_null = NOT_NULL_RE.is_match(&item);
            let inline_pk = INLINE_PK_RE.is_match(&item);
            let identity = IDENTITY_RE.is_match(&item);
            cols.push((
                name,
                dtype,
                !not_null,
                inline_pk,
                identity,
                references_target(&item),
            ));
        }

        for (name, dtype, nullable, inline_pk, identity, fk) in cols {
            let name_lower = name.to_lowercase();
            let is_pk = inline_pk || pk_cols.contains(&name_lower);
            if inline_pk {
                indexed.push(vec![name_lower.clone()]);
            }
            let mut extra = Map::new();
            extra.insert("_origin".into(), json!("tsql"));
            extra.insert("data_type".into(), json!(dtype));
            extra.insert("nullable".into(), json!(nullable && !is_pk));
            extra.insert("pk".into(), json!(is_pk));
            if identity {
                extra.insert("identity".into(), json!(true));
            }
            if let Some(t) = fk.or_else(|| fk_cols.get(&name_lower).cloned()) {
                extra.insert("fk_target".into(), json!(t));
            }
            let cid = col_id(&table_lower, &name_lower);
            add_node(result, cid.clone(), &name, NodeKind::Column, extra);
            add_edge(result, table_id.clone(), cid, "has_column", "column");
        }

        // index node per PK/UNIQUE constraint, so PERF-IDX sees indexed columns.
        for (i, idx_cols) in indexed.iter().enumerate() {
            if idx_cols.is_empty() {
                continue;
            }
            let idx_label = format!("ix_{table_lower}_{i}");
            let idx_id = NodeId(make_id(&["sql", &table_lower, "idx", &idx_label]));
            let mut extra = Map::new();
            extra.insert("_origin".into(), json!("tsql"));
            add_node(result, idx_id.clone(), &idx_label, NodeKind::Index, extra);
            add_edge(
                result,
                table_id.clone(),
                idx_id.clone(),
                "has_index",
                "index",
            );
            for c in idx_cols {
                let cid = col_id(&table_lower, c);
                add_edge(result, idx_id.clone(), cid, "indexes", "index_column");
            }
        }
    }
}

/// The substring inside the parenthesis group opened at `open_idx` (exclusive of
/// the outer parens), respecting nesting and single-quoted strings.
fn balanced_block(s: &str, open_idx: usize) -> Option<String> {
    let b = s.as_bytes();
    if b.get(open_idx) != Some(&b'(') {
        return None;
    }
    let mut depth = 0i32;
    let mut start = open_idx + 1;
    let mut in_str = false;
    for (i, &c) in b.iter().enumerate().skip(open_idx) {
        match c {
            b'\'' => in_str = !in_str,
            b'(' if !in_str => {
                if depth == 0 {
                    start = i + 1;
                }
                depth += 1;
            }
            b')' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return Some(s[start..i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Split a column/constraint block on top-level commas (respecting `()`, `[]`,
/// and single-quoted strings).
fn split_top_level(block: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut cur = String::new();
    for c in block.chars() {
        match c {
            '\'' => {
                in_str = !in_str;
                cur.push(c);
            }
            '(' | '[' if !in_str => {
                depth += 1;
                cur.push(c);
            }
            ')' | ']' if !in_str => {
                depth -= 1;
                cur.push(c);
            }
            ',' if !in_str && depth == 0 => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Lowercased column names in the first parenthesis group of a constraint item
/// (`PRIMARY KEY CLUSTERED ([Id] ASC)` -> `["id"]`), stripping sort direction.
fn constraint_columns(item: &str) -> Vec<String> {
    let Some(start) = item.find('(') else {
        return Vec::new();
    };
    let Some(block) = balanced_block(item, start) else {
        return Vec::new();
    };
    split_top_level(&block)
        .iter()
        .filter_map(|c| {
            let tok = c.split_whitespace().next()?;
            let n = strip_brackets(tok).to_lowercase();
            (!n.is_empty()).then_some(n)
        })
        .collect()
}

/// The lowercased last-segment target table of a `REFERENCES <name>` in `item`.
fn references_target(item: &str) -> Option<String> {
    static REF_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?is)\breferences\s+((?:\[[^\]]+\]|"[^"]+"|\w+)(?:\.(?:\[[^\]]+\]|"[^"]+"|\w+))*)"#,
        )
        .expect("tsql ref regex")
    });
    REF_RE.captures(item).map(|c| last_ident(&c[1]))
}

fn strip_brackets(s: &str) -> String {
    s.trim()
        .trim_matches(|c| c == '[' || c == ']' || c == '`' || c == '"')
        .to_string()
}

/// Apply T-SQL `ALTER TABLE ... ADD ... PRIMARY KEY / FOREIGN KEY` constraints
/// (the dominant way PKs/FKs are declared in real T-SQL) to already-emitted
/// column nodes: set `fk_target` and `pk`. Runs after the CREATE TABLE pass.
fn enrich_tsql_constraints(sql: &str, result: &mut ExtractionResult) {
    static ALTER_FK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?is)\balter\s+table\s+((?:\[[^\]]+\]|"[^"]+"|\w+)(?:\.(?:\[[^\]]+\]|"[^"]+"|\w+))*)\s+(?:with\s+(?:no\s*)?check\s+)?add\s+(?:with\s+(?:no\s*)?check\s+)?(?:constraint\s+(?:\[[^\]]+\]|\w+)\s+)?foreign\s+key\s*\(([^)]*)\)\s*references\s+((?:\[[^\]]+\]|"[^"]+"|\w+)(?:\.(?:\[[^\]]+\]|"[^"]+"|\w+))*)"#,
        )
        .expect("alter fk regex")
    });
    static ALTER_PK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?is)\balter\s+table\s+((?:\[[^\]]+\]|"[^"]+"|\w+)(?:\.(?:\[[^\]]+\]|"[^"]+"|\w+))*)\s+(?:with\s+(?:no\s*)?check\s+)?add\s+(?:constraint\s+(?:\[[^\]]+\]|\w+)\s+)?primary\s+key(?:\s+(?:clustered|nonclustered))?\s*\(([^)]*)\)"#,
        )
        .expect("alter pk regex")
    });
    let cols_of = |raw: &str| -> Vec<String> {
        raw.split(',')
            .map(|c| strip_brackets(c.split_whitespace().next().unwrap_or("")).to_lowercase())
            .filter(|c| !c.is_empty())
            .collect::<Vec<_>>()
    };
    for caps in ALTER_FK.captures_iter(sql) {
        let table_lower = last_ident(&caps[1]);
        let target = last_ident(&caps[3]);
        for col in cols_of(&caps[2]) {
            let cid = col_id(&table_lower, &col);
            if let Some(n) = result.nodes.iter_mut().find(|n| n.id == cid) {
                n.extra.entry("fk_target").or_insert(json!(target.clone()));
            }
        }
    }
    for caps in ALTER_PK.captures_iter(sql) {
        let table_lower = last_ident(&caps[1]);
        for col in cols_of(&caps[2]) {
            let cid = col_id(&table_lower, &col);
            if let Some(n) = result.nodes.iter_mut().find(|n| n.id == cid) {
                n.extra.insert("pk".into(), json!(true));
                n.extra.insert("nullable".into(), json!(false));
            }
        }
    }
}

/// T-SQL `CREATE [UNIQUE] [NON]CLUSTERED INDEX <name> ON <table> (<cols>)`
/// (the dominant way secondary indexes are declared) -> Index node + has_index +
/// per-column `indexes` edges, so PERF-IDX rules don't flag indexed columns.
fn enrich_tsql_indexes(sql: &str, result: &mut ExtractionResult) {
    static IDX_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?is)\bcreate\s+(?:unique\s+)?(?:(?:non)?clustered\s+)?index\s+(\[[^\]]+\]|"[^"]+"|\w+)\s+on\s+((?:\[[^\]]+\]|"[^"]+"|\w+)(?:\.(?:\[[^\]]+\]|"[^"]+"|\w+))*)\s*\(([^)]*)\)"#,
        )
        .expect("tsql index regex")
    });
    for caps in IDX_RE.captures_iter(sql) {
        let idx_label = strip_brackets(&caps[1]);
        let table_lower = last_ident(&caps[2]);
        if idx_label.is_empty() || table_lower.is_empty() {
            continue;
        }
        let table_id = NodeId(make_id(&["sql", &table_lower]));
        let idx_id = NodeId(make_id(&[
            "sql",
            &table_lower,
            "idx",
            &idx_label.to_lowercase(),
        ]));
        let mut extra = Map::new();
        extra.insert("_origin".into(), json!("tsql"));
        add_node(result, idx_id.clone(), &idx_label, NodeKind::Index, extra);
        add_edge(result, table_id, idx_id.clone(), "has_index", "index");
        for c in split_top_level(&caps[3]) {
            let col = strip_brackets(c.split_whitespace().next().unwrap_or("")).to_lowercase();
            if col.is_empty() {
                continue;
            }
            let cid = col_id(&table_lower, &col);
            add_edge(result, idx_id.clone(), cid, "indexes", "index_column");
        }
    }
}

/// Emit an Index node, a table->index `has_index` edge, and an index->column
/// `indexes` edge per indexed column. CreateIndex columns are OrderByExpr, so
/// the column name is unwrapped from `expr`.
fn enrich_indexes(sql: &str, result: &mut ExtractionResult) {
    for stmt in parse_tolerant(sql) {
        let Statement::CreateIndex(ci) = stmt else {
            continue;
        };
        let table_lower = object_name(&ci.table_name).to_lowercase();
        if table_lower.is_empty() {
            continue;
        }
        let table_id = NodeId(make_id(&["sql", &table_lower]));
        let idx_label = ci
            .name
            .as_ref()
            .map(object_name)
            .unwrap_or_else(|| format!("ix_{table_lower}"));
        let idx_id = NodeId(make_id(&[
            "sql",
            &table_lower,
            "idx",
            &idx_label.to_lowercase(),
        ]));
        let mut extra = Map::new();
        extra.insert("_origin".into(), json!("ast"));
        extra.insert("unique".into(), json!(ci.unique));
        add_node(result, idx_id.clone(), &idx_label, NodeKind::Index, extra);
        add_edge(result, table_id, idx_id.clone(), "has_index", "index");
        for c in &ci.columns {
            let col_lower = index_column_name(&c.expr);
            if col_lower.is_empty() {
                continue;
            }
            let cid = col_id(&table_lower, &col_lower);
            add_edge(result, idx_id.clone(), cid, "indexes", "index_column");
        }
    }
}

/// The lowercased column name an index entry references. Plain column indexes
/// give an identifier; expression indexes fall back to the leading token.
fn index_column_name(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(id) => id.value.to_lowercase(),
        other => other
            .to_string()
            .split(|ch: char| ch.is_whitespace() || ch == '(')
            .next()
            .unwrap_or("")
            .trim_matches(|ch| ch == '"' || ch == '`' || ch == '[' || ch == ']')
            .to_lowercase(),
    }
}

fn last_ident(qualified: &str) -> String {
    qualified
        .rsplit('.')
        .next()
        .unwrap_or(qualified)
        .trim_matches(|c| c == '[' || c == ']' || c == '`' || c == '"' || c == ' ')
        .to_lowercase()
}

fn enrich_security(sql: &str, result: &mut ExtractionResult) {
    // RLS enable/force flags on the table node.
    for caps in RLS_ENABLE_RE.captures_iter(sql) {
        let table_lower = last_ident(&caps[1]);
        let table_id = NodeId(make_id(&["sql", &table_lower]));
        let forced = caps[2].eq_ignore_ascii_case("force");
        if let Some(t) = result.nodes.iter_mut().find(|n| n.id == table_id) {
            t.extra.insert("rls_enabled".into(), json!(true));
            if forced {
                t.extra.insert("rls_forced".into(), json!(true));
            } else {
                t.extra.entry("rls_forced").or_insert(json!(false));
            }
        }
    }

    // Postgres CREATE POLICY -> Policy node + protected_by.
    for caps in POLICY_RE.captures_iter(sql) {
        let pol_name = caps[1].to_string();
        let table_lower = last_ident(&caps[2]);
        let body = &caps[3];
        let mut extra = Map::new();
        extra.insert("_origin".into(), json!("ast"));
        if let Some(u) = USING_RE.captures(body) {
            extra.insert("using_expr".into(), json!(u[1].trim()));
        }
        if let Some(w) = WITH_CHECK_RE.captures(body) {
            extra.insert("with_check_expr".into(), json!(w[1].trim()));
        }
        let pid = NodeId(make_id(&[
            "sql",
            &table_lower,
            "policy",
            &pol_name.to_lowercase(),
        ]));
        add_node(result, pid.clone(), &pol_name, NodeKind::Policy, extra);
        let table_id = NodeId(make_id(&["sql", &table_lower]));
        add_edge(result, table_id, pid, "protected_by", "rls");
    }

    // SQL Server CREATE SECURITY POLICY -> Policy node + protected_by.
    for caps in MSSQL_SECPOL_RE.captures_iter(sql) {
        let pol_name = caps[1].to_string();
        let table_lower = last_ident(&caps[2]);
        let mut extra = Map::new();
        extra.insert("_origin".into(), json!("ast"));
        extra.insert("engine".into(), json!("sqlserver"));
        let pid = NodeId(make_id(&[
            "sql",
            &table_lower,
            "secpol",
            &pol_name.to_lowercase(),
        ]));
        add_node(result, pid.clone(), &pol_name, NodeKind::Policy, extra);
        let table_id = NodeId(make_id(&["sql", &table_lower]));
        add_edge(result, table_id, pid, "protected_by", "security_policy");
    }

    // GRANT -> Role node + grants edge (role -> table), privilege in context.
    for caps in GRANT_RE.captures_iter(sql) {
        let priv_ = caps[1].trim().to_string();
        let table_lower = last_ident(&caps[2]);
        let role = caps[3].to_string();
        let rid = NodeId(make_id(&["sql", "role", &role.to_lowercase()]));
        let mut extra = Map::new();
        extra.insert("_origin".into(), json!("ast"));
        add_node(result, rid.clone(), &role, NodeKind::Role, extra);
        let table_id = NodeId(make_id(&["sql", &table_lower]));
        add_edge(result, rid, table_id, "grants", &priv_);
    }
}

#[cfg(all(test, feature = "lang-sql"))]
mod tests {
    use super::enrich;
    use crate::result::ExtractionResult;
    use crate::sql::extract_sql_source;
    use synaptic_core::NodeKind;

    #[test]
    fn enrich_with_emit_columns_false_skips_columns() {
        let mut result = ExtractionResult::default();
        enrich(
            "schema.sql",
            b"CREATE TABLE t (id INT PRIMARY KEY, email TEXT);\nCREATE INDEX ix ON t (email);",
            false,
            &mut result,
        );
        assert!(
            result
                .nodes
                .iter()
                .all(|n| n.kind() != Some(NodeKind::Column)),
            "no column nodes when emit_columns is false"
        );
        assert!(
            result
                .nodes
                .iter()
                .all(|n| n.kind() != Some(NodeKind::Index)),
            "no index nodes when emit_columns is false"
        );
        assert!(
            result.edges.iter().all(|e| e.relation != "has_column"),
            "no has_column edges when emit_columns is false"
        );
    }

    #[test]
    fn enrich_with_emit_columns_true_keeps_columns() {
        let mut result = ExtractionResult::default();
        enrich(
            "schema.sql",
            b"CREATE TABLE t (id INT PRIMARY KEY, email TEXT);",
            true,
            &mut result,
        );
        assert!(
            result
                .nodes
                .iter()
                .any(|n| n.kind() == Some(NodeKind::Column) && n.label == "email"),
            "columns present when emit_columns is true"
        );
    }

    const SRC: &[u8] =
        b"CREATE TABLE users (id INT PRIMARY KEY, email TEXT NOT NULL, tenant_id INT);";

    #[test]
    fn table_node_gets_table_kind_and_dialect() {
        let r = extract_sql_source("schema.sql", SRC);
        let t = r
            .nodes
            .iter()
            .find(|n| n.label == "users")
            .expect("users node");
        assert_eq!(t.kind(), Some(NodeKind::Table));
        assert_eq!(
            t.extra.get("dialect").and_then(|v| v.as_str()),
            Some("generic")
        );
    }

    #[test]
    fn columns_become_nodes_with_has_column_edges() {
        let r = extract_sql_source("schema.sql", SRC);
        let cols: Vec<&str> = r
            .nodes
            .iter()
            .filter(|n| n.kind() == Some(NodeKind::Column))
            .map(|n| n.label.as_str())
            .collect();
        assert!(cols.contains(&"email"), "columns: {cols:?}");
        assert!(cols.contains(&"tenant_id"), "columns: {cols:?}");

        let has_col = r
            .edges
            .iter()
            .filter(|e| e.relation == "has_column")
            .count();
        assert_eq!(has_col, 3, "one has_column edge per column");
    }

    #[test]
    fn primary_key_and_nullability_recorded() {
        let r = extract_sql_source("schema.sql", SRC);
        let id_col = r
            .nodes
            .iter()
            .find(|n| n.kind() == Some(NodeKind::Column) && n.label == "id")
            .expect("id column");
        assert_eq!(id_col.extra.get("pk").and_then(|v| v.as_bool()), Some(true));
        let email_col = r
            .nodes
            .iter()
            .find(|n| n.kind() == Some(NodeKind::Column) && n.label == "email")
            .expect("email column");
        assert_eq!(
            email_col.extra.get("nullable").and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn table_level_primary_key_constraint_marks_column_pk() {
        let src = b"CREATE TABLE t (id INT, name TEXT, PRIMARY KEY (id));";
        let r = extract_sql_source("schema.sql", src);
        let id_col = r
            .nodes
            .iter()
            .find(|n| n.kind() == Some(NodeKind::Column) && n.label == "id")
            .expect("id column");
        assert_eq!(
            id_col.extra.get("pk").and_then(|v| v.as_bool()),
            Some(true),
            "table-level PK should mark id as pk"
        );
        assert_eq!(
            id_col.extra.get("nullable").and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn unparseable_sql_does_not_panic_and_keeps_objects() {
        let r = extract_sql_source("schema.sql", b"CREATE TABLE t (id INT); @@@ not sql @@@");
        assert!(r.nodes.iter().any(|n| n.label == "t"));
    }

    const SRC_IDX: &[u8] = b"CREATE TABLE users (id INT PRIMARY KEY, email TEXT);\nCREATE INDEX ix_email ON users (email);";

    #[test]
    fn create_index_becomes_index_node_and_edges() {
        use synaptic_core::NodeKind;
        let r = extract_sql_source("schema.sql", SRC_IDX);
        let idx = r
            .nodes
            .iter()
            .find(|n| n.kind() == Some(NodeKind::Index))
            .expect("index node");
        assert_eq!(idx.label, "ix_email");
        let indexes_email = r
            .edges
            .iter()
            .any(|e| e.relation == "indexes" && e.source == idx.id);
        assert!(indexes_email, "expected an indexes edge from ix_email");
        let has_index = r.edges.iter().any(|e| e.relation == "has_index");
        assert!(has_index, "expected a has_index edge");
    }

    #[test]
    fn rls_enable_and_force_recorded_on_table() {
        let src = b"CREATE TABLE orders (id INT);\nALTER TABLE orders ENABLE ROW LEVEL SECURITY;\nALTER TABLE orders FORCE ROW LEVEL SECURITY;";
        let r = extract_sql_source("schema.sql", src);
        let t = r.nodes.iter().find(|n| n.label == "orders").unwrap();
        assert_eq!(
            t.extra.get("rls_enabled").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            t.extra.get("rls_forced").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn create_policy_becomes_policy_node_and_protected_by() {
        use synaptic_core::NodeKind;
        let src = b"CREATE TABLE orders (id INT, tenant_id INT);\nCREATE POLICY tenant_isolation ON orders USING (tenant_id = current_setting('app.tenant')::int);";
        let r = extract_sql_source("schema.sql", src);
        let p = r
            .nodes
            .iter()
            .find(|n| n.kind() == Some(NodeKind::Policy))
            .expect("policy node");
        assert_eq!(p.label, "tenant_isolation");
        assert!(p.extra.get("using_expr").is_some());
        let protected = r
            .edges
            .iter()
            .any(|e| e.relation == "protected_by" && e.target == p.id);
        assert!(protected, "orders should be protected_by the policy");
    }

    #[test]
    fn sqlserver_markers_set_dialect_and_keep_columns() {
        let src = b"CREATE TABLE patients (id INT PRIMARY KEY, ssn NVARCHAR(11));";
        let r = extract_sql_source("schema.sql", src);
        let t = r
            .nodes
            .iter()
            .find(|n| n.label == "patients")
            .expect("patients node");
        assert_eq!(
            t.extra.get("dialect").and_then(|v| v.as_str()),
            Some("sqlserver"),
            "NVARCHAR is a T-SQL marker"
        );
        let has_ssn = r
            .nodes
            .iter()
            .any(|n| n.kind() == Some(NodeKind::Column) && n.label == "ssn");
        assert!(has_ssn, "ssn column should still be extracted");
    }

    #[test]
    fn create_view_security_invoker_flag_recorded() {
        let src = b"CREATE TABLE orders (id INT, tenant_id INT);\nCREATE VIEW v_secure WITH (security_invoker = true) AS SELECT * FROM orders;\nCREATE VIEW v_plain AS SELECT * FROM orders;";
        let r = extract_sql_source("schema.sql", src);
        let secure = r
            .nodes
            .iter()
            .find(|n| n.label == "v_secure")
            .expect("v_secure node");
        assert_eq!(
            secure
                .extra
                .get("security_invoker")
                .and_then(|v| v.as_bool()),
            Some(true),
            "security_invoker view should be flagged true"
        );
        let plain = r
            .nodes
            .iter()
            .find(|n| n.label == "v_plain")
            .expect("v_plain node");
        assert_ne!(
            plain
                .extra
                .get("security_invoker")
                .and_then(|v| v.as_bool()),
            Some(true),
            "plain view must not be marked security_invoker"
        );
    }

    #[test]
    fn tsql_identity_column_is_flagged_identity() {
        let src = b"CREATE TABLE [dbo].[t]([bal_id] [int] IDENTITY(1,1) NOT NULL, [name] [nvarchar](50) NULL)\nGO\n";
        let r = extract_sql_source("dbo.t.sql", src);
        let bal = r
            .nodes
            .iter()
            .find(|n| n.kind() == Some(NodeKind::Column) && n.label == "bal_id")
            .unwrap();
        assert_eq!(
            bal.extra.get("identity").and_then(|v| v.as_bool()),
            Some(true)
        );
        let name = r.nodes.iter().find(|n| n.label == "name").unwrap();
        assert!(name.extra.get("identity").is_none());
    }

    #[test]
    fn tsql_create_nonclustered_index_emits_index_edges() {
        let src = b"CREATE TABLE [dbo].[cart_transactions](\n  [order_id] [int] NOT NULL\n)\nGO\nCREATE NONCLUSTERED INDEX [IX_order] ON [dbo].[cart_transactions] ([order_id] ASC)\nGO\n";
        let r = extract_sql_source("dbo.cart_transactions.sql", src);
        let idx = r
            .nodes
            .iter()
            .find(|n| n.kind() == Some(NodeKind::Index))
            .expect("index node from CREATE NONCLUSTERED INDEX");
        let order_col = r
            .nodes
            .iter()
            .find(|n| n.kind() == Some(NodeKind::Column) && n.label == "order_id")
            .unwrap();
        assert!(
            r.edges
                .iter()
                .any(|e| e.relation == "indexes" && e.source == idx.id && e.target == order_col.id),
            "expected an indexes edge to order_id"
        );
    }

    #[test]
    fn tsql_alter_table_foreign_key_and_primary_key() {
        // Real T-SQL: PK/FK added via separate ALTER TABLE statements (the
        // dominant pattern), not inline in CREATE TABLE.
        let src = b"CREATE TABLE [dbo].[cart_transactions](\n  [transaction_id] [int] NOT NULL,\n  [order_id] [int] NOT NULL\n)\nGO\nALTER TABLE [dbo].[cart_transactions]  WITH NOCHECK ADD  CONSTRAINT [FK_x] FOREIGN KEY([order_id])\n REFERENCES [dbo].[cart_orders] ([order_id])\nGO\nALTER TABLE [dbo].[cart_transactions] ADD CONSTRAINT [PK_x] PRIMARY KEY CLUSTERED ([transaction_id] ASC)\nGO\n";
        let r = extract_sql_source("dbo.cart_transactions.sql", src);
        let col = |name: &str| {
            r.nodes
                .iter()
                .find(|n| n.kind() == Some(NodeKind::Column) && n.label == name)
                .unwrap_or_else(|| panic!("missing column {name}"))
        };
        assert_eq!(
            col("order_id")
                .extra
                .get("fk_target")
                .and_then(|v| v.as_str()),
            Some("cart_orders"),
            "ALTER ADD FOREIGN KEY must set fk_target"
        );
        assert_eq!(
            col("transaction_id")
                .extra
                .get("pk")
                .and_then(|v| v.as_bool()),
            Some(true),
            "ALTER ADD PRIMARY KEY must mark the column pk"
        );
    }

    #[test]
    fn inline_foreign_key_sets_column_fk_target() {
        let src = b"CREATE TABLE messages (id uuid PRIMARY KEY, conversation_id uuid REFERENCES chat_conversations(id) ON DELETE CASCADE NOT NULL, note text);";
        let r = extract_sql_source("schema.sql", src);
        let conv = r
            .nodes
            .iter()
            .find(|n| n.kind() == Some(NodeKind::Column) && n.label == "conversation_id")
            .expect("conversation_id column");
        assert_eq!(
            conv.extra.get("fk_target").and_then(|v| v.as_str()),
            Some("chat_conversations"),
            "inline REFERENCES must set fk_target"
        );
        let note = r.nodes.iter().find(|n| n.label == "note").unwrap();
        assert!(
            note.extra.get("fk_target").is_none(),
            "non-FK column has no fk_target"
        );
    }

    #[test]
    fn table_level_foreign_key_sets_column_fk_target() {
        let src = b"CREATE TABLE messages (id uuid, conversation_id uuid, FOREIGN KEY (conversation_id) REFERENCES chat_conversations(id));";
        let r = extract_sql_source("schema.sql", src);
        let conv = r
            .nodes
            .iter()
            .find(|n| n.kind() == Some(NodeKind::Column) && n.label == "conversation_id")
            .expect("conversation_id column");
        assert_eq!(
            conv.extra.get("fk_target").and_then(|v| v.as_str()),
            Some("chat_conversations"),
            "table-level FOREIGN KEY must set fk_target"
        );
    }

    #[test]
    fn schema_qualified_view_security_invoker_recorded() {
        // Real Supabase shape: CREATE VIEW public.<name> WITH (security_invoker=true).
        let src = b"CREATE TABLE orders (id INT);\nCREATE VIEW public.v_secure\nWITH (security_invoker = true)\nAS SELECT * FROM orders;";
        let r = extract_sql_source("schema.sql", src);
        let v = r
            .nodes
            .iter()
            .find(|n| n.label == "v_secure")
            .expect("v_secure node");
        assert_eq!(
            v.extra.get("security_invoker").and_then(|x| x.as_bool()),
            Some(true),
            "schema-qualified view must record security_invoker"
        );
    }

    #[test]
    fn plain_sql_keeps_generic_dialect() {
        let r = extract_sql_source("schema.sql", SRC);
        let t = r.nodes.iter().find(|n| n.label == "users").unwrap();
        assert_eq!(
            t.extra.get("dialect").and_then(|v| v.as_str()),
            Some("generic")
        );
    }

    #[test]
    fn grant_becomes_role_node_and_grants_edge() {
        use synaptic_core::NodeKind;
        let src = b"CREATE TABLE orders (id INT);\nGRANT SELECT ON orders TO app_reader;";
        let r = extract_sql_source("schema.sql", src);
        let role = r
            .nodes
            .iter()
            .find(|n| n.kind() == Some(NodeKind::Role))
            .expect("role node");
        assert_eq!(role.label, "app_reader");
        let granted = r
            .edges
            .iter()
            .any(|e| e.relation == "grants" && e.source == role.id);
        assert!(granted, "app_reader should grant on orders");
    }
}
