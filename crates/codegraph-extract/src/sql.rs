//! SQL extractor — Bucket C (custom). Grammar: `tree-sitter-sequel`.
//!
//! `CREATE TABLE`/`VIEW`/`FUNCTION` → nodes (AST); column/table `REFERENCES`
//! (foreign keys) → `references`; a view/function's `FROM`/`JOIN` relations →
//! `reads_from`. Procedures and triggers ERROR out in this grammar, so a regex
//! recovery pass scans the raw text per statement to
//! recover those nodes plus `REFERENCES` (FK), `ON` (trigger→table), and
//! `FROM`/`JOIN` (reads), deduped against the AST pass. Names match
//! case-insensitively.

#[cfg(feature = "lang-sql")]
use std::collections::{HashMap, HashSet};
#[cfg(feature = "lang-sql")]
use std::sync::LazyLock;

#[cfg(feature = "lang-sql")]
use codegraph_core::{make_id, NodeId};
#[cfg(feature = "lang-sql")]
use regex::Regex;
#[cfg(feature = "lang-sql")]
use tree_sitter::{Node as TsNode, Parser};

#[cfg(feature = "lang-sql")]
use crate::common::Builder;
#[cfg(feature = "lang-sql")]
use crate::paths::file_node_id;
#[cfg(feature = "lang-sql")]
use crate::result::ExtractionResult;

// Regex-recovery patterns, compiled once process-wide (not per `.sql` file). M1.
// A possibly-schema-qualified, optionally-bracketed/quoted object name as one
// capture group (handles T-SQL `[schema].[name]`, MySQL backticks, plain). Callers
// reduce it to the last segment via `last_segment`.
#[cfg(feature = "lang-sql")]
const QNAME: &str =
    r#"((?:\[[^\]]+\]|"[^"]+"|`[^`]+`|\w+)(?:\.(?:\[[^\]]+\]|"[^"]+"|`[^`]+`|\w+))*)"#;
#[cfg(feature = "lang-sql")]
static CREATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?is)\bcreate\s+(?:or\s+replace\s+)?(?:global\s+|temp\w*\s+)?(table|view|function|procedure|trigger)\s+(?:if\s+not\s+exists\s+)?{QNAME}"#
    ))
    .expect("create regex")
});
#[cfg(feature = "lang-sql")]
static ON_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r#"(?is)\bon\s+{QNAME}"#)).expect("on regex"));
#[cfg(feature = "lang-sql")]
static REFERENCES_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r#"(?is)\breferences\s+{QNAME}"#)).expect("ref regex"));
#[cfg(feature = "lang-sql")]
static FROM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r#"(?is)\b(?:from|join)\s+{QNAME}"#)).expect("from regex")
});

/// The last identifier of a possibly-qualified, possibly-bracketed name
/// (`[schema].[name]` -> `name`, `dbo.t` -> `t`).
#[cfg(feature = "lang-sql")]
fn last_segment(qualified: &str) -> String {
    qualified
        .rsplit('.')
        .next()
        .unwrap_or(qualified)
        .trim_matches(|c| c == '[' || c == ']' || c == '`' || c == '"' || c == ' ')
        .to_string()
}

/// Extract a SQL file already in memory.
#[cfg(feature = "lang-sql")]
pub fn extract_sql_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_sequel::LANGUAGE.into())
        .expect("load tree-sitter-sequel");
    let Some(tree) = parser.parse(source, None) else {
        return ExtractionResult::default();
    };
    let ex = Sql { src: source };
    let root = tree.root_node();

    let mut b = Builder::new(path);
    let file_nid = file_node_id(path);
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    b.add_node(file_nid.clone(), filename, 1);

    // Pass 1: a node per CREATE TABLE / VIEW / FUNCTION; remember name -> id.
    let mut ids: HashMap<String, NodeId> = HashMap::new();
    for stmt in ex.statements(root) {
        let (kind, name, node) = match ex.created_object(stmt) {
            Some(v) => v,
            None => continue,
        };
        if name.is_empty() {
            continue;
        }
        let id = NodeId(make_id(&["sql", &name.to_lowercase()]));
        let line = node.start_position().row + 1;
        b.add_node(id.clone(), name.clone(), line);
        b.add_edge(file_nid.clone(), id.clone(), "contains", line, Some(kind));
        ids.insert(name.to_lowercase(), id);
    }

    // Pass 2: FK `references` (tables) + `reads_from` (views/functions). Each
    // emitted edge is recorded so the regex recovery pass doesn't duplicate it.
    let mut emitted: HashSet<(String, String, String)> = HashSet::new();
    for stmt in ex.statements(root) {
        let Some((kind, name, node)) = ex.created_object(stmt) else {
            continue;
        };
        let Some(src_id) = ids.get(&name.to_lowercase()).cloned() else {
            continue;
        };
        let line = node.start_position().row + 1;
        if kind == "table" {
            for reftab in ex.foreign_key_targets(node) {
                let tgt = ex.resolve(&mut b, &ids, &reftab);
                emitted.insert((src_id.0.clone(), "references".into(), tgt.0.clone()));
                b.add_edge(src_id.clone(), tgt, "references", line, Some("foreign_key"));
            }
        } else {
            for reltab in ex.read_relations(node) {
                let tgt = ex.resolve(&mut b, &ids, &reltab);
                emitted.insert((src_id.0.clone(), "reads_from".into(), tgt.0.clone()));
                b.add_edge(src_id.clone(), tgt, "reads_from", line, Some("from"));
            }
        }
    }

    // Recovery: dialects this grammar can't parse (procedures, triggers, some
    // tables) land in ERROR nodes. Scan the raw text per `;`-delimited statement
    // for CREATE objects + REFERENCES/ON/FROM, deduped against the AST pass
    // (regex fallback).
    ex.regex_recover(
        &mut b,
        &String::from_utf8_lossy(source),
        &file_nid,
        &mut ids,
        &mut emitted,
    );

    let mut result = b.into_result();
    let emit_columns = crate::sql_semantic::emit_sql_columns();
    crate::sql_semantic::enrich(path, source, emit_columns, &mut result);
    result
}

/// Read and extract a SQL file from disk.
#[cfg(feature = "lang-sql")]
pub fn extract_sql_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_sql_source(&path_str, &source))
}

#[cfg(feature = "lang-sql")]
struct Sql<'a> {
    src: &'a [u8],
}

#[cfg(feature = "lang-sql")]
impl Sql<'_> {
    fn text(&self, node: TsNode) -> String {
        node.utf8_text(self.src).unwrap_or("").to_string()
    }

    fn children(node: TsNode) -> Vec<TsNode> {
        let mut c = node.walk();
        node.children(&mut c).collect()
    }

    fn statements<'t>(&self, root: TsNode<'t>) -> Vec<TsNode<'t>> {
        Self::children(root)
            .into_iter()
            .filter(|c| c.kind() == "statement")
            .collect()
    }

    /// `("table"|"view"|"function", name, create_node)` for a CREATE statement,
    /// else `None`. (Procedures/triggers fall into ERROR nodes in this grammar —
    /// recovered by the regex pass.)
    fn created_object<'t>(&self, stmt: TsNode<'t>) -> Option<(&'static str, String, TsNode<'t>)> {
        for c in Self::children(stmt) {
            match c.kind() {
                "create_table" => return Some(("table", self.object_name(c), c)),
                "create_view" => return Some(("view", self.object_name(c), c)),
                "create_function" => return Some(("function", self.object_name(c), c)),
                _ => {}
            }
        }
        None
    }

    /// The created object's name: the first direct `object_reference` child's name.
    fn object_name(&self, create_node: TsNode) -> String {
        Self::children(create_node)
            .into_iter()
            .find(|c| c.kind() == "object_reference")
            .map(|r| self.object_reference_name(r))
            .unwrap_or_default()
    }

    /// The (last) identifier of an `object_reference` (`schema.table` → `table`).
    fn object_reference_name(&self, obj_ref: TsNode) -> String {
        if let Some(name) = obj_ref.child_by_field_name("name") {
            return self.text(name);
        }
        Self::children(obj_ref)
            .into_iter()
            .rfind(|c| c.kind() == "identifier")
            .map(|c| self.text(c))
            .unwrap_or_default()
    }

    /// Referenced table names for each `REFERENCES` (column- or table-level FK):
    /// the first `object_reference` after each `keyword_references` token.
    fn foreign_key_targets(&self, create_table: TsNode) -> Vec<String> {
        let mut out = Vec::new();
        self.each_kind(create_table, "keyword_references", &mut |kw| {
            let mut sib = kw.next_named_sibling();
            while let Some(s) = sib {
                if s.kind() == "object_reference" {
                    let n = self.object_reference_name(s);
                    if !n.is_empty() {
                        out.push(n);
                    }
                    break;
                }
                sib = s.next_named_sibling();
            }
        });
        out
    }

    /// Table names read by a view: the `object_reference` inside each `relation`.
    fn read_relations(&self, create_view: TsNode) -> Vec<String> {
        let mut out = Vec::new();
        self.each_kind(create_view, "relation", &mut |rel| {
            if let Some(or) = Self::children(rel)
                .into_iter()
                .find(|c| c.kind() == "object_reference")
            {
                let n = self.object_reference_name(or);
                if !n.is_empty() {
                    out.push(n);
                }
            }
        });
        out
    }

    /// Visit every descendant of `node` whose kind is `kind`.
    fn each_kind<'t>(&self, node: TsNode<'t>, kind: &str, f: &mut dyn FnMut(TsNode<'t>)) {
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if n.kind() == kind {
                f(n);
            }
            for c in Self::children(n) {
                stack.push(c);
            }
        }
    }

    /// Resolve a referenced table name to an existing node id, else a stub.
    fn resolve(&self, b: &mut Builder, ids: &HashMap<String, NodeId>, name: &str) -> NodeId {
        if let Some(id) = ids.get(&name.to_lowercase()) {
            return id.clone();
        }
        let id = NodeId(make_id(&["sql", &name.to_lowercase()]));
        b.add_external_node(id.clone(), name.to_string());
        id
    }

    /// Regex recovery over the raw text, per `;`-delimited statement: recover
    /// CREATE objects the AST dropped (procedures/triggers ERROR out in this
    /// grammar) plus their `REFERENCES` (table FK), `ON` (trigger target), and
    /// `FROM`/`JOIN` (view/function/procedure reads). Deduped against `emitted`.
    fn regex_recover(
        &self,
        b: &mut Builder,
        text: &str,
        file_nid: &NodeId,
        ids: &mut HashMap<String, NodeId>,
        emitted: &mut HashSet<(String, String, String)>,
    ) {
        let create = &*CREATE_RE;
        let on = &*ON_RE;
        let references = &*REFERENCES_RE;
        let from = &*FROM_RE;

        for chunk in text.split(';') {
            let Some(caps) = create.captures(chunk) else {
                continue;
            };
            let kind = caps[1].to_lowercase();
            let name = last_segment(&caps[2]);
            let name_l = name.to_lowercase();
            if name_l.is_empty() {
                continue;
            }
            // Node: reuse an AST node, else create one (procedures/triggers) and
            // give it a `contains` edge (AST nodes already have one).
            let was_new = !ids.contains_key(&name_l);
            let src_id = ids.get(&name_l).cloned().unwrap_or_else(|| {
                let id = NodeId(make_id(&["sql", &name_l]));
                b.add_node(id.clone(), name.clone(), 1);
                ids.insert(name_l.clone(), id.clone());
                id
            });
            if was_new {
                b.add_edge(file_nid.clone(), src_id.clone(), "contains", 1, Some(&kind));
            }

            match kind.as_str() {
                "trigger" => {
                    if let Some(t) = on.captures(chunk) {
                        self.recover_ref(b, &src_id, "triggers", &t[1], ids, emitted);
                    }
                }
                "table" => {
                    for t in references.captures_iter(chunk) {
                        self.recover_ref(b, &src_id, "references", &t[1], ids, emitted);
                    }
                }
                _ => {
                    // view / function / procedure read tables.
                    for t in from.captures_iter(chunk) {
                        self.recover_ref(b, &src_id, "reads_from", &t[1], ids, emitted);
                    }
                }
            }
        }
    }

    /// Emit `obj → resolved(name)` for a recovered reference (deduped).
    fn recover_ref(
        &self,
        b: &mut Builder,
        obj: &NodeId,
        relation: &str,
        name: &str,
        ids: &HashMap<String, NodeId>,
        emitted: &mut HashSet<(String, String, String)>,
    ) {
        let tgt = self.resolve(b, ids, &last_segment(name));
        if obj == &tgt {
            return;
        }
        let key = (obj.0.clone(), relation.to_string(), tgt.0.clone());
        if emitted.insert(key) {
            b.add_edge(obj.clone(), tgt, relation, 1, Some("sql"));
        }
    }
}

#[cfg(all(test, feature = "lang-sql"))]
mod tests {
    use super::extract_sql_source;
    use crate::result::ExtractionResult;

    fn rels(r: &ExtractionResult, relation: &str) -> Vec<(String, String)> {
        let lbl = |id: &codegraph_core::NodeId| {
            r.nodes
                .iter()
                .find(|n| &n.id == id)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| id.0.clone())
        };
        r.edges
            .iter()
            .filter(|e| e.relation == relation)
            .map(|e| (lbl(&e.source), lbl(&e.target)))
            .collect()
    }

    const SRC: &[u8] = b"CREATE TABLE users (id INT PRIMARY KEY);\nCREATE TABLE orders (\n  id INT,\n  user_id INT REFERENCES users(id)\n);\nCREATE VIEW recent AS SELECT * FROM orders;\n";

    #[test]
    fn tables_and_view_nodes() {
        let r = extract_sql_source("schema.sql", SRC);
        let labels: Vec<_> = r.nodes.iter().map(|n| n.label.clone()).collect();
        assert!(labels.contains(&"users".to_string()), "{labels:?}");
        assert!(labels.contains(&"orders".to_string()));
        assert!(labels.contains(&"recent".to_string()));
    }

    // Real T-SQL table DDL: bracketed schema-qualified name, bracketed types,
    // IDENTITY, CLUSTERED PK, ON [PRIMARY], wrapped in IF/BEGIN/END with GO and
    // no semicolons.
    const TSQL_TABLE: &[u8] = b"SET ANSI_NULLS ON\nGO\nIF NOT EXISTS (SELECT * FROM sys.objects WHERE object_id = OBJECT_ID(N'[Analytics].[AgentJob]') AND type in (N'U'))\nBEGIN\nCREATE TABLE [Analytics].[AgentJob](\n\t[AgentJobId] [int] IDENTITY(1,1) NOT NULL,\n\t[JobId] [uniqueidentifier] NOT NULL,\n\t[Name] [nvarchar](256) NOT NULL,\n\t[PasswordHash] [nvarchar](max) NULL,\n CONSTRAINT [PK_AgentJob] PRIMARY KEY CLUSTERED ([AgentJobId] ASC)\n) ON [PRIMARY]\nEND\nGO\n";

    #[test]
    fn tsql_bracketed_table_name_is_last_segment() {
        let r = extract_sql_source("dbo.AgentJob.sql", TSQL_TABLE);
        let table_labels: Vec<_> = r
            .nodes
            .iter()
            .filter(|n| n.kind() == Some(codegraph_core::NodeKind::Table))
            .map(|n| n.label.clone())
            .collect();
        assert!(
            table_labels.contains(&"AgentJob".to_string()),
            "table should be named AgentJob, got: {table_labels:?}"
        );
        assert!(
            !table_labels.contains(&"Analytics".to_string()),
            "must not name the table after its schema"
        );
    }

    #[test]
    fn tsql_bracketed_columns_and_pk_extracted() {
        use codegraph_core::NodeKind;
        let r = extract_sql_source("dbo.AgentJob.sql", TSQL_TABLE);
        let cols: Vec<_> = r
            .nodes
            .iter()
            .filter(|n| n.kind() == Some(NodeKind::Column))
            .map(|n| n.label.clone())
            .collect();
        for want in ["AgentJobId", "JobId", "Name", "PasswordHash"] {
            assert!(
                cols.iter().any(|c| c == want),
                "missing T-SQL column {want}; got {cols:?}"
            );
        }
        // PK comes from the CLUSTERED PRIMARY KEY constraint.
        let pk = r
            .nodes
            .iter()
            .find(|n| n.kind() == Some(NodeKind::Column) && n.label == "AgentJobId")
            .unwrap();
        assert_eq!(pk.extra.get("pk").and_then(|v| v.as_bool()), Some(true));
        // a non-pk column reflects nullability.
        let pw = r
            .nodes
            .iter()
            .find(|n| n.kind() == Some(NodeKind::Column) && n.label == "PasswordHash")
            .unwrap();
        assert_eq!(
            pw.extra.get("nullable").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(
            r.edges
                .iter()
                .filter(|e| e.relation == "has_column")
                .count()
                >= 4,
            "expected has_column edges"
        );
    }

    #[test]
    fn foreign_key_becomes_references() {
        let r = extract_sql_source("schema.sql", SRC);
        let refs = rels(&r, "references");
        assert!(
            refs.contains(&("orders".to_string(), "users".to_string())),
            "refs: {refs:?}"
        );
    }

    #[test]
    fn view_from_becomes_reads_from() {
        let r = extract_sql_source("schema.sql", SRC);
        let reads = rels(&r, "reads_from");
        assert!(
            reads.contains(&("recent".to_string(), "orders".to_string())),
            "reads_from: {reads:?}"
        );
    }

    #[test]
    fn functions_procedures_and_triggers() {
        // FUNCTION parses cleanly (AST); PROCEDURE + TRIGGER ERROR out in this
        // grammar and are recovered by the regex pass.
        let src = b"CREATE TABLE users (id INT);\nCREATE TABLE audit (id INT);\nCREATE FUNCTION recent_users() RETURNS INT AS $$ SELECT id FROM users; $$;\nCREATE PROCEDURE sync_audit() BEGIN SELECT * FROM audit; END;\nCREATE TRIGGER trg AFTER INSERT ON users FOR EACH ROW BEGIN UPDATE audit SET n=1; END;\n";
        let r = extract_sql_source("schema.sql", src);
        let labels: Vec<_> = r.nodes.iter().map(|n| n.label.clone()).collect();
        assert!(labels.contains(&"recent_users".to_string()), "{labels:?}"); // function (AST)
        assert!(labels.contains(&"sync_audit".to_string())); // procedure (regex)
        assert!(labels.contains(&"trg".to_string())); // trigger (regex)

        let reads = rels(&r, "reads_from");
        assert!(
            reads.contains(&("recent_users".to_string(), "users".to_string())),
            "reads_from: {reads:?}"
        );
        assert!(reads.contains(&("sync_audit".to_string(), "audit".to_string())));

        let trig = rels(&r, "triggers");
        assert!(
            trig.contains(&("trg".to_string(), "users".to_string())),
            "triggers: {trig:?}"
        );

        // No duplicate contains edge for AST objects after the recovery pass.
        let contains_users = r
            .edges
            .iter()
            .filter(|e| {
                e.relation == "contains" && e.target.0 == codegraph_core::make_id(&["sql", "users"])
            })
            .count();
        assert_eq!(contains_users, 1, "users contained exactly once");
    }
}
