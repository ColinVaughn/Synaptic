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
use regex::Regex;
#[cfg(feature = "lang-sql")]
use synaptic_core::{NodeId, make_id};
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
/// Modifiers a dialect may put between `CREATE` and the object kind. Allowing
/// only `global`/`temp*` dropped the object outright for every warehouse
/// dialect: on a 2,575-file corpus `CREATE MATERIALIZED VIEW` and `CREATE
/// EXTERNAL TABLE` alone accounted for 112 of 125 missing declarations. Repeated
/// rather than optional-once, because dialects stack them
/// (`CREATE OR REPLACE SECURE TRANSIENT TABLE`).
#[cfg(feature = "lang-sql")]
const CREATE_MODIFIERS: &str = r"(?:or\s+replace|global|temporary|temp|external|materialized|unlogged|secure|transient|volatile|virtual|dynamic|managed|streaming|foreign|iceberg|hybrid|local|private|public|shared|cached|live|incremental)\s+";
#[cfg(feature = "lang-sql")]
static CREATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?is)\bcreate\s+(?:{CREATE_MODIFIERS})*(table|view|function|procedure|trigger)\s+(?:if\s+not\s+exists\s+)?{QNAME}"#
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

/// Blank out SQL comments, preserving every byte offset and newline.
///
/// The recovery pass scans raw text, so a comment that merely mentions DDL
/// ("-- CREATE DYNAMIC TABLE clauses") invented a table. String literals are
/// tracked so a `--` inside one stays data: blanking from there would swallow
/// the rest of a real statement.
#[cfg(feature = "lang-sql")]
fn blank_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = bytes.to_vec();
    let mut i = 0usize;
    // Blank a range, keeping newlines so line numbers are unchanged.
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for b in &mut out[from..to] {
            if *b != b'\n' {
                *b = b' ';
            }
        }
    };
    while i < bytes.len() {
        match bytes[i] {
            // Quoted string or identifier: skip to its close, honouring the
            // doubled-quote escape both SQL dialects use.
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == q {
                        if bytes.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                blank(&mut out, start, i);
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let start = i;
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                blank(&mut out, start, i);
            }
            _ => i += 1,
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
}

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
    // dbt models are `.sql` files that are not valid SQL. Neutralize the Jinja
    // first, preserving every byte offset, so the grammar sees parseable text and
    // every line number below still refers to the real file. Plain SQL comes
    // through this untouched.
    let raw = String::from_utf8_lossy(source);
    let neutralized = crate::dbt::neutralize(&raw);
    let source: &[u8] = neutralized.as_bytes();

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
    b.note_parse_health(root);

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
        &blank_comments(&neutralized),
        &file_nid,
        &mut ids,
        &mut emitted,
    );

    // dbt lineage. The model is named for its file (that is how dbt names it),
    // so it is anchored at line 1: the declaration really is the whole file, and
    // no line inside ever spells the name.
    if crate::dbt::is_dbt(&raw)
        && let Some(model) = crate::dbt::model_name(path)
    {
        let model_l = model.to_lowercase();
        let model_id = ids.get(&model_l).cloned().unwrap_or_else(|| {
            let id = NodeId(make_id(&["sql", &model_l]));
            b.add_node(id.clone(), model.clone(), 1);
            b.add_edge(file_nid.clone(), id.clone(), "contains", 1, Some("view"));
            ids.insert(model_l.clone(), id.clone());
            id
        });
        for r in crate::dbt::references(&raw) {
            let tgt = ex.resolve(&mut b, &ids, &r.name);
            if tgt == model_id {
                continue;
            }
            let key = (model_id.0.clone(), "reads_from".to_string(), tgt.0.clone());
            if emitted.insert(key) {
                b.add_edge(model_id.clone(), tgt, "reads_from", r.line, Some("dbt_ref"));
            }
        }
    }

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

        // Statements are matched inside `;`-delimited chunks, but a node's line
        // has to be expressed against the whole file. Track each chunk's absolute
        // offset and binary-search the newline table, rather than anchoring every
        // recovered object at line 1 -- which sent "go to definition" to the file
        // header for every procedure and trigger in the corpus.
        let newlines: Vec<usize> = text.match_indices('\n').map(|(i, _)| i).collect();
        let line_at = |offset: usize| newlines.partition_point(|&n| n < offset) + 1;

        let mut chunk_start = 0usize;
        for chunk in text.split(';') {
            let chunk_offset = chunk_start;
            // `+ 1` steps over the `;` that `split` consumed.
            chunk_start += chunk.len() + 1;

            // A chunk can hold more than one statement when a file omits its
            // semicolons, so every CREATE is recovered, and each one's
            // REFERENCES/ON/FROM are read from its own slice rather than from the
            // whole chunk -- otherwise one statement's tables would be attributed
            // to its neighbour.
            let starts: Vec<usize> = create.find_iter(chunk).map(|m| m.start()).collect();
            for (i, &begin) in starts.iter().enumerate() {
                let end = starts.get(i + 1).copied().unwrap_or(chunk.len());
                let chunk = &chunk[begin..end];
                let Some(caps) = create.captures(chunk) else {
                    continue;
                };
                let line = line_at(chunk_offset + begin);
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
                    b.add_node(id.clone(), name.clone(), line);
                    ids.insert(name_l.clone(), id.clone());
                    id
                });
                if was_new {
                    b.add_edge(
                        file_nid.clone(),
                        src_id.clone(),
                        "contains",
                        line,
                        Some(&kind),
                    );
                }

                match kind.as_str() {
                    "trigger" => {
                        if let Some(t) = on.captures(chunk) {
                            self.recover_ref(b, &src_id, "triggers", &t[1], ids, emitted, line);
                        }
                    }
                    "table" => {
                        for t in references.captures_iter(chunk) {
                            self.recover_ref(b, &src_id, "references", &t[1], ids, emitted, line);
                        }
                    }
                    _ => {
                        // view / function / procedure read tables.
                        for t in from.captures_iter(chunk) {
                            self.recover_ref(b, &src_id, "reads_from", &t[1], ids, emitted, line);
                        }
                    }
                }
            }
        }
    }

    /// Emit `obj → resolved(name)` for a recovered reference (deduped).
    #[allow(clippy::too_many_arguments)]
    fn recover_ref(
        &self,
        b: &mut Builder,
        obj: &NodeId,
        relation: &str,
        name: &str,
        ids: &HashMap<String, NodeId>,
        emitted: &mut HashSet<(String, String, String)>,
        line: usize,
    ) {
        let tgt = self.resolve(b, ids, &last_segment(name));
        if obj == &tgt {
            return;
        }
        let key = (obj.0.clone(), relation.to_string(), tgt.0.clone());
        if emitted.insert(key) {
            b.add_edge(obj.clone(), tgt, relation, line, Some("sql"));
        }
    }
}

#[cfg(all(test, feature = "lang-sql"))]
mod tests {
    use super::extract_sql_source;
    use crate::result::ExtractionResult;

    fn rels(r: &ExtractionResult, relation: &str) -> Vec<(String, String)> {
        let lbl = |id: &synaptic_core::NodeId| {
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
            .filter(|n| n.kind() == Some(synaptic_core::NodeKind::Table))
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
        use synaptic_core::NodeKind;
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

    /// Every regex-recovered object was anchored at line 1 regardless of where
    /// it was declared, because the recovery pass split the text on `;` and had
    /// no byte offset to derive a line from. A procedure 40 lines into a file
    /// reported `L1`, so "go to definition" landed on the file header and the
    /// anchor benchmark scored it wrong.
    #[test]
    fn recovered_objects_are_anchored_at_their_declaration_line() {
        let src = b"-- header\n-- more header\n\nCREATE TABLE users (id INT);\n\nCREATE PROCEDURE sync_audit() BEGIN SELECT * FROM users; END;\n\nCREATE TRIGGER trg AFTER INSERT ON users FOR EACH ROW BEGIN UPDATE users SET n=1; END;\n";
        let r = extract_sql_source("schema.sql", src);
        let line_of = |label: &str| {
            r.nodes
                .iter()
                .find(|n| n.label == label)
                .unwrap_or_else(|| {
                    let all: Vec<&str> = r.nodes.iter().map(|n| n.label.as_str()).collect();
                    panic!("no node {label}; got {all:?}")
                })
                .source_location
                .clone()
        };
        assert_eq!(line_of("sync_audit").as_deref(), Some("L6"));
        assert_eq!(line_of("trg").as_deref(), Some("L8"));
        // The AST pass already anchored correctly; it must not regress.
        assert_eq!(line_of("users").as_deref(), Some("L4"));
    }

    /// A file whose only statement is recovered by regex still declares
    /// something. Anchoring it at line 1 collided with the file node's own line
    /// and made the declaration indistinguishable from the file header.
    #[test]
    fn a_recovered_object_on_line_one_still_reports_line_one() {
        let src = b"CREATE PROCEDURE first_thing() BEGIN SELECT 1; END;\n";
        let r = extract_sql_source("p.sql", src);
        let n = r
            .nodes
            .iter()
            .find(|n| n.label == "first_thing")
            .expect("recovered procedure");
        assert_eq!(n.source_location.as_deref(), Some("L1"));
    }

    /// A dbt model is a `.sql` file that is not valid SQL. Before Jinja was
    /// neutralized the grammar errored on the whole file and it contributed
    /// nothing but its own file node, so every dbt project was invisible.
    #[test]
    fn dbt_model_declares_itself_and_its_refs() {
        let src = b"{% set methods = ['card', 'coupon'] %}\n\nwith orders as (\n\n    select * from {{ ref('stg_orders') }}\n\n),\n\npayments as (\n\n    select * from {{ source('raw', 'payments') }}\n\n)\n\nselect * from orders\n";
        let r = extract_sql_source("models/customers.sql", src);
        let labels: Vec<&str> = r.nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(
            labels.contains(&"customers"),
            "the model is named for its file; got {labels:?}"
        );

        let reads = rels(&r, "reads_from");
        assert!(
            reads.contains(&("customers".to_string(), "stg_orders".to_string())),
            "ref() lineage missing: {reads:?}"
        );
        assert!(
            reads.contains(&("customers".to_string(), "payments".to_string())),
            "source() lineage missing: {reads:?}"
        );
    }

    /// Neutralization must not shift line numbers: the anchor benchmark scores
    /// every node against the line it claims.
    #[test]
    fn dbt_neutralization_keeps_declaration_lines_exact() {
        let src = b"{% set x = 1 %}\n\n{#- a comment -#}\n\nCREATE TABLE late_table (id INT);\n";
        let r = extract_sql_source("models/m.sql", src);
        let t = r
            .nodes
            .iter()
            .find(|n| n.label == "late_table")
            .expect("table after jinja");
        assert_eq!(t.source_location.as_deref(), Some("L5"));
    }

    /// Plain SQL must not acquire a dbt model node just for living in a repo
    /// that also uses dbt.
    #[test]
    fn plain_sql_gets_no_dbt_model_node() {
        let r = extract_sql_source("schema.sql", b"CREATE TABLE users (id INT);\n");
        let labels: Vec<&str> = r.nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(!labels.contains(&"schema"), "{labels:?}");
        assert!(labels.contains(&"users"));
    }

    /// The recovery regex allowed only `global`/`temp*` between CREATE and the
    /// object kind, so every warehouse dialect's modifiers dropped the object
    /// entirely. Measured on a 2,575-file dialect corpus this was 112 of 125
    /// missing declarations -- 90% of the total miss.
    #[test]
    fn create_modifiers_do_not_hide_the_object() {
        let cases: &[(&str, &str)] = &[
            (
                "CREATE MATERIALIZED VIEW mydataset.my_mv AS SELECT 1;",
                "my_mv",
            ),
            (
                "CREATE MATERIALIZED VIEW IF NOT EXISTS mydataset.my_mv2 AS SELECT 1;",
                "my_mv2",
            ),
            ("CREATE EXTERNAL TABLE dataset.ext_t (id INT);", "ext_t"),
            ("CREATE TRANSIENT TABLE t_trans (id INT);", "t_trans"),
            ("CREATE SECURE VIEW v_secure AS SELECT 1;", "v_secure"),
            ("CREATE UNLOGGED TABLE t_unlogged (id INT);", "t_unlogged"),
            ("CREATE OR REPLACE TEMPORARY TABLE t_tmp (id INT);", "t_tmp"),
            (
                "CREATE EXTERNAL FUNCTION exfunc_sum() RETURNS INT;",
                "exfunc_sum",
            ),
        ];
        for (src, want) in cases {
            let r = extract_sql_source("d.sql", src.as_bytes());
            let labels: Vec<&str> = r.nodes.iter().map(|n| n.label.as_str()).collect();
            assert!(
                labels.iter().any(|l| l.eq_ignore_ascii_case(want)),
                "{src:?} should declare {want}; got {labels:?}"
            );
        }
    }

    /// Recovery matched one CREATE per `;`-delimited chunk, so a file that omits
    /// its final semicolons lost every statement after the first.
    #[test]
    fn every_create_in_a_chunk_is_recovered() {
        // No semicolon anywhere, so `split(';')` yields a single chunk holding
        // both statements.
        let src = b"CREATE PROCEDURE p_one() BEGIN SELECT 1 END\nCREATE PROCEDURE p_two() BEGIN SELECT 2 END\n";
        let r = extract_sql_source("multi.sql", src);
        let labels: Vec<&str> = r.nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.contains(&"p_one"), "{labels:?}");
        assert!(
            labels.contains(&"p_two"),
            "second CREATE in the chunk: {labels:?}"
        );
    }

    /// The recovery regex scans raw text, so a comment that merely *mentions*
    /// DDL invented a node. A dialect corpus had 121 such comment lines, each
    /// one a phantom table in the graph.
    #[test]
    fn a_create_inside_a_comment_declares_nothing() {
        let src = b"-- CREATE DYNAMIC TABLE clauses\n/* CREATE VIEW old_view AS SELECT 1; */\nCREATE TABLE real_table (id INT);\n";
        let r = extract_sql_source("c.sql", src);
        let labels: Vec<&str> = r.nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.contains(&"real_table"), "{labels:?}");
        assert!(!labels.contains(&"clauses"), "line comment: {labels:?}");
        assert!(!labels.contains(&"old_view"), "block comment: {labels:?}");
    }

    /// A `--` inside a string literal is data, not a comment; blanking it would
    /// silently eat the rest of a real statement.
    #[test]
    fn a_comment_marker_inside_a_string_is_not_a_comment() {
        let src =
            b"CREATE TABLE t_dash (note TEXT DEFAULT 'a -- b');\nCREATE TABLE after_it (id INT);\n";
        let r = extract_sql_source("s.sql", src);
        let labels: Vec<&str> = r.nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.contains(&"t_dash"), "{labels:?}");
        assert!(labels.contains(&"after_it"), "{labels:?}");
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
                e.relation == "contains" && e.target.0 == synaptic_core::make_id(&["sql", "users"])
            })
            .count();
        assert_eq!(contains_users, 1, "users contained exactly once");
    }
}
