//! CGQL evaluator: match a [`Query`] against a [`KnowledgeGraph`].
//!
//! Evaluation is lenient: a comparison against a missing/`Null` property, or a
//! type mismatch, yields no match rather than an error.

use std::collections::{HashMap, HashSet};

use codegraph_core::NodeId;
use codegraph_graph::KnowledgeGraph;
use regex::Regex;

use crate::ast::*;
use crate::QueryResult;

/// A resolved property value.
enum Val {
    S(String),
    N(f64),
    Null,
}

/// Map a file extension to a coarse language family (else the bare extension).
/// `None` when the basename has no extension (a dotless file is not a language).
fn lang_of(file: &str) -> Option<String> {
    let base = file.rsplit(['/', '\\']).next().unwrap_or(file);
    let (_, ext) = base.rsplit_once('.')?;
    let fam = match ext.to_ascii_lowercase().as_str() {
        "py" | "pyw" => "python",
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => "js",
        "go" => "go",
        "rs" => "rust",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "cs" => "csharp",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "rb" => "ruby",
        "php" => "php",
        "scala" => "scala",
        other => return Some(other.to_string()),
    };
    Some(fam.to_string())
}

/// The bare symbol name for matching: a node label without the trailing `()`
/// the extractor appends to callables. So `f.name = "announce"` matches a node
/// labeled `announce()`, and `.name` is consistent across kinds (a class label
/// is already bare). Only an exact trailing `()` is removed, so a label like
/// `Setup (Windows)` is left intact. Partial matches use the `=~` regex operator.
fn bare_name(label: &str) -> &str {
    label.strip_suffix("()").unwrap_or(label)
}

fn prop(kg: &KnowledgeGraph, id: &NodeId, field: Field) -> Val {
    let node = kg.node(id);
    match field {
        Field::Name => node
            .map(|n| Val::S(bare_name(&n.label).to_string()))
            .unwrap_or(Val::Null),
        Field::File => node
            .map(|n| Val::S(n.source_file.clone()))
            .unwrap_or(Val::Null),
        Field::Lang => node
            .and_then(|n| lang_of(&n.source_file))
            .map(Val::S)
            .unwrap_or(Val::Null),
        Field::Kind => node
            .and_then(|n| n.kind())
            .map(|k| Val::S(k.as_str().to_string()))
            .unwrap_or(Val::Null),
        Field::Visibility => node
            .and_then(|n| n.visibility())
            .map(|v| Val::S(v.as_str().to_string()))
            .unwrap_or(Val::Null),
        Field::Loc => kg.loc(id).map(|l| Val::N(l as f64)).unwrap_or(Val::Null),
        Field::FanIn => Val::N(kg.fan_in(id, &[]) as f64),
        Field::FanOut => Val::N(kg.fan_out(id, &[]) as f64),
        Field::Degree => Val::N(kg.degree(id) as f64),
        Field::Community => node
            .and_then(|n| n.community)
            .map(|c| Val::N(c as f64))
            .unwrap_or(Val::Null),
        // SQL security posture, read from node extra. rls_enabled is a JSON bool
        // rendered to the string "true"/"false" so the string comparator works
        // (e.g. WHERE t.rls_enabled = "false"). dialect/operation are strings.
        Field::RlsEnabled => node
            .and_then(|n| n.extra.get("rls_enabled"))
            .map(|v| Val::S(v.to_string()))
            .unwrap_or(Val::Null),
        Field::Dialect => node
            .and_then(|n| n.extra.get("dialect"))
            .and_then(|v| v.as_str())
            .map(|s| Val::S(s.to_string()))
            .unwrap_or(Val::Null),
        Field::Operation => node
            .and_then(|n| n.extra.get("operation"))
            .and_then(|v| v.as_str())
            .map(|s| Val::S(s.to_string()))
            .unwrap_or(Val::Null),
    }
}

fn compare(lhs: &Val, op: Op, rhs: &Value, regexes: &HashMap<String, Option<Regex>>) -> bool {
    // Null (unknown property) never matches any operator, including `!=`. A
    // present-but-wrong-type value, by contrast, IS "not equal" (the mismatch arm
    // below returns true for `!=`) — the two rules differ deliberately.
    match (lhs, rhs) {
        (Val::Null, _) => false,
        (Val::N(a), Value::Num(b)) => match op {
            Op::Eq => a == b,
            Op::Ne => a != b,
            Op::Lt => a < b,
            Op::Le => a <= b,
            Op::Gt => a > b,
            Op::Ge => a >= b,
            Op::Regex => false,
        },
        (Val::S(a), Value::Str(b)) => match op {
            Op::Eq => a == b,
            Op::Ne => a != b,
            Op::Lt => a < b,
            Op::Le => a <= b,
            Op::Gt => a > b,
            Op::Ge => a >= b,
            Op::Regex => regexes
                .get(b)
                .and_then(|r| r.as_ref())
                .map(|re| re.is_match(a))
                .unwrap_or(false),
        },
        // Type mismatch: only `!=` is true (the values are not equal); all else false.
        _ => matches!(op, Op::Ne),
    }
}

fn eval_expr(
    kg: &KnowledgeGraph,
    e: &Expr,
    binding: &HashMap<String, NodeId>,
    regexes: &HashMap<String, Option<Regex>>,
) -> bool {
    match e {
        Expr::And(a, b) => eval_expr(kg, a, binding, regexes) && eval_expr(kg, b, binding, regexes),
        Expr::Or(a, b) => eval_expr(kg, a, binding, regexes) || eval_expr(kg, b, binding, regexes),
        Expr::Not(a) => !eval_expr(kg, a, binding, regexes),
        Expr::Cmp(p, op, v) => match binding.get(&p.var) {
            Some(id) => compare(&prop(kg, id, p.field), *op, v, regexes),
            None => false,
        },
        // Per-node modifiers are not populated yet; always false.
        Expr::Has(_, _) => false,
    }
}

/// Pre-compile every regex literal used with `=~`, keyed by its pattern string.
fn collect_regexes(e: Option<&Expr>) -> HashMap<String, Option<Regex>> {
    let mut out = HashMap::new();
    fn walk(e: &Expr, out: &mut HashMap<String, Option<Regex>>) {
        match e {
            Expr::And(a, b) | Expr::Or(a, b) => {
                walk(a, out);
                walk(b, out);
            }
            Expr::Not(a) => walk(a, out),
            Expr::Cmp(_, Op::Regex, Value::Str(p)) => {
                out.entry(p.clone()).or_insert_with(|| Regex::new(p).ok());
            }
            _ => {}
        }
    }
    if let Some(e) = e {
        walk(e, &mut out);
    }
    out
}

fn kind_matches(kg: &KnowledgeGraph, id: &NodeId, want: &Option<String>) -> bool {
    match want {
        None => true,
        Some(k) => kg
            .node(id)
            .and_then(|n| n.kind())
            .map(|nk| nk.as_str().eq_ignore_ascii_case(k))
            .unwrap_or(false),
    }
}

/// Neighbours of `cur` reachable by one `rel` step, de-duplicated.
fn step(kg: &KnowledgeGraph, cur: &NodeId, rel: &RelPat) -> Vec<NodeId> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for e in kg.incident_edges(cur) {
        if let Some(want) = &rel.rel {
            if &e.relation != want {
                continue;
            }
        }
        let other = match rel.dir {
            Dir::LtoR if &e.source == cur => Some(&e.target),
            Dir::RtoL if &e.target == cur => Some(&e.source),
            Dir::Either if &e.source == cur => Some(&e.target),
            Dir::Either if &e.target == cur => Some(&e.source),
            _ => None,
        };
        if let Some(o) = other {
            if seen.insert(o.clone()) {
                out.push(o.clone());
            }
        }
    }
    out
}

/// Nodes reachable from `start` along `rel` at some depth within `[min,max]`,
/// de-duplicated. (A node first reached below `min` may still be included if it is
/// also reachable at a depth >= min via the BFS frontier.) For a plain
/// `min==max==1` relationship this is a single `step`. A `visited` set bounds
/// cycles so an unbounded `*` always terminates.
fn reachable(kg: &KnowledgeGraph, start: &NodeId, rel: &RelPat) -> Vec<NodeId> {
    if rel.min <= 1 && rel.max <= 1 {
        return step(kg, start, rel);
    }
    let mut result: HashSet<NodeId> = HashSet::new();
    let mut visited: HashSet<NodeId> = HashSet::new();
    visited.insert(start.clone());
    let mut frontier = vec![start.clone()];
    for depth in 1..=rel.max {
        let mut next = Vec::new();
        for cur in &frontier {
            for nb in step(kg, cur, rel) {
                if depth >= rel.min {
                    result.insert(nb.clone());
                }
                if visited.insert(nb.clone()) {
                    next.push(nb);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    let mut v: Vec<NodeId> = result.into_iter().collect();
    v.sort();
    v
}

/// All variable bindings that satisfy the pattern (before WHERE).
fn eval_pattern(
    kg: &KnowledgeGraph,
    pattern: &Pattern,
    vars: &[String],
) -> Vec<HashMap<String, NodeId>> {
    let mut bindings: Vec<HashMap<String, NodeId>> = kg
        .nodes()
        .filter(|n| kind_matches(kg, &n.id, &pattern.nodes[0].kind))
        .map(|n| {
            let mut m = HashMap::new();
            m.insert(vars[0].clone(), n.id.clone());
            m
        })
        .collect();

    for (i, rel) in pattern.rels.iter().enumerate() {
        let next_pat = &pattern.nodes[i + 1];
        let mut extended = Vec::new();
        for b in &bindings {
            let cur = &b[&vars[i]];
            for other in reachable(kg, cur, rel) {
                if kind_matches(kg, &other, &next_pat.kind) {
                    let mut nb = b.clone();
                    nb.insert(vars[i + 1].clone(), other);
                    extended.push(nb);
                }
            }
        }
        bindings = extended;
    }
    bindings
}

/// Validate runtime semantics that the parser can't (regex literals compile).
/// Surfaces a clear error instead of a silently-empty result.
pub fn validate_query(q: &Query) -> Result<(), String> {
    fn walk(e: &Expr) -> Result<(), String> {
        match e {
            Expr::And(a, b) | Expr::Or(a, b) => {
                walk(a)?;
                walk(b)
            }
            Expr::Not(a) => walk(a),
            Expr::Cmp(_, Op::Regex, Value::Str(p)) => Regex::new(p)
                .map(|_| ())
                .map_err(|e| format!("invalid regex '{p}': {e}")),
            _ => Ok(()),
        }
    }
    match &q.where_ {
        Some(e) => walk(e),
        None => Ok(()),
    }
}

/// A resolved property/var value as a display string (for aggregate output).
fn val_str(v: Val) -> String {
    match v {
        Val::S(s) => s,
        Val::N(n) => {
            if n.fract() == 0.0 {
                format!("{}", n as i64)
            } else {
                format!("{n}")
            }
        }
        Val::Null => String::new(),
    }
}

/// True when RETURN projects scalars (an aggregate `count` or a `var.field`),
/// requiring grouped/scalar output instead of node-id rows.
fn is_scalar(q: &Query) -> bool {
    q.ret
        .iter()
        .any(|r| matches!(r, RetItem::Count(_) | RetItem::Prop(_)))
}

/// Evaluate a query into a [`QueryResult`].
pub fn run_query(kg: &KnowledgeGraph, q: &Query) -> QueryResult {
    // Assign a (possibly synthetic) variable name to every node pattern.
    // Anonymous patterns get a synthetic name that can't be a user identifier
    // (the lexer never produces a `$`-leading ident), so it can never collide with
    // a user var like `_1`.
    let vars: Vec<String> = q
        .pattern
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| n.var.clone().unwrap_or_else(|| format!("$anon{i}")))
        .collect();

    let regexes = collect_regexes(q.where_.as_ref());
    let bindings: Vec<HashMap<String, NodeId>> = eval_pattern(kg, &q.pattern, &vars)
        .into_iter()
        .filter(|b| {
            q.where_
                .as_ref()
                .map(|e| eval_expr(kg, e, b, &regexes))
                .unwrap_or(true)
        })
        .collect();

    let columns: Vec<String> = q.ret.iter().map(|r| r.header()).collect();

    if is_scalar(q) {
        // Group by the non-Count RETURN items; count per group.
        let mut groups: std::collections::BTreeMap<Vec<String>, usize> =
            std::collections::BTreeMap::new();
        for b in &bindings {
            let key: Vec<String> = q
                .ret
                .iter()
                .filter_map(|item| match item {
                    RetItem::Var(v) => Some(
                        b.get(v)
                            .map(|id| {
                                kg.node(id)
                                    .map(|n| n.label.clone())
                                    .unwrap_or_else(|| id.0.clone())
                            })
                            .unwrap_or_default(),
                    ),
                    RetItem::Prop(p) => Some(
                        b.get(&p.var)
                            .map(|id| val_str(prop(kg, id, p.field)))
                            .unwrap_or_default(),
                    ),
                    RetItem::Count(_) => None,
                })
                .collect();
            *groups.entry(key).or_insert(0) += 1;
        }
        let mut agg: Vec<Vec<String>> = groups
            .into_iter()
            .map(|(key, count)| {
                let mut ki = key.into_iter();
                q.ret
                    .iter()
                    .map(|item| match item {
                        RetItem::Count(_) => count.to_string(),
                        _ => ki.next().unwrap_or_default(),
                    })
                    .collect()
            })
            .collect();
        if let Some(lim) = q.limit {
            agg.truncate(lim);
        }
        return QueryResult {
            columns,
            rows: Vec::new(),
            aggregates: Some(agg),
        };
    }

    // Plain projection: one node-id row per match (all RETURN items are Var).
    let mut rows: Vec<Vec<NodeId>> = bindings
        .into_iter()
        .map(|b| {
            q.ret
                .iter()
                .filter_map(|item| match item {
                    RetItem::Var(v) => b.get(v).cloned(),
                    _ => None,
                })
                .collect()
        })
        .collect();

    rows.sort();
    rows.dedup();
    if let Some(lim) = q.limit {
        rows.truncate(lim);
    }
    QueryResult {
        columns,
        rows,
        aggregates: None,
    }
}

/// Render a parsed query as a human-readable plan (for `--explain`).
pub fn explain_plan(q: &Query) -> String {
    use std::fmt::Write as _;
    let mut o = String::new();
    let _ = writeln!(o, "PLAN");
    // Pattern / scan + joins.
    let vars: Vec<String> = q
        .pattern
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| n.var.clone().unwrap_or_else(|| format!("$anon{i}")))
        .collect();
    let kind0 = q.pattern.nodes[0].kind.as_deref().unwrap_or("any");
    let _ = writeln!(o, "  SCAN {} (kind: {})", vars[0], kind0);
    for (i, rel) in q.pattern.rels.iter().enumerate() {
        let r = rel.rel.as_deref().unwrap_or("any");
        let dir = match rel.dir {
            Dir::LtoR => "->",
            Dir::RtoL => "<-",
            Dir::Either => "--",
        };
        let bound = if rel.min == 1 && rel.max == 1 {
            String::new()
        } else {
            format!(" *{}..{}", rel.min, rel.max)
        };
        let kind = q.pattern.nodes[i + 1].kind.as_deref().unwrap_or("any");
        let _ = writeln!(
            o,
            "  JOIN {} {}[{}{}] {} (kind: {})",
            vars[i],
            dir,
            r,
            bound,
            vars[i + 1],
            kind
        );
    }
    if q.where_.is_some() {
        let _ = writeln!(o, "  FILTER (WHERE)");
    }
    let cols: Vec<String> = q.ret.iter().map(|r| r.header()).collect();
    // A bare `var.field` projection groups (distinct) like an aggregate, so label
    // any scalar RETURN as AGGREGATE to match run_query's grouping path.
    let scalar = q
        .ret
        .iter()
        .any(|r| matches!(r, RetItem::Count(_) | RetItem::Prop(_)));
    let _ = writeln!(
        o,
        "  {} {}",
        if scalar { "AGGREGATE" } else { "PROJECT" },
        cols.join(", ")
    );
    if let Some(l) = q.limit {
        let _ = writeln!(o, "  LIMIT {l}");
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;
    use codegraph_core::{Confidence, Edge, GraphData, Node, NodeId, NodeKind, Span, Visibility};
    use serde_json::Map;

    fn node(id: &str, label: &str, kind: NodeKind, loc: u32) -> Node {
        let mut n = Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: codegraph_core::FileType::Code,
            source_file: format!("{id}.rs"),
            source_location: Some("L1".into()),
            community: None,
            repo: None,
            extra: Map::new(),
        };
        n.set_kind(kind);
        n.set_span(Span {
            start_line: 1,
            start_col: 1,
            end_line: loc,
            end_col: 1,
        });
        n
    }

    fn edge(s: &str, t: &str, rel: &str) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: rel.into(),
            confidence: Confidence::Extracted,
            source_file: "x.rs".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    fn graph(nodes: Vec<Node>, edges: Vec<Edge>) -> KnowledgeGraph {
        KnowledgeGraph::from_graph_data(GraphData {
            nodes,
            links: edges,
            ..Default::default()
        })
    }

    fn run(kg: &KnowledgeGraph, q: &str) -> Vec<String> {
        let res = run_query(kg, &parse(q).unwrap());
        res.rows.iter().map(|r| r[0].0.clone()).collect()
    }

    #[test]
    fn filters_tables_by_rls_enabled() {
        use serde_json::json;
        let mut secure = node("secure", "secure", NodeKind::Table, 1);
        secure.extra.insert("rls_enabled".into(), json!(true));
        let mut leaky = node("leaky", "leaky", NodeKind::Table, 1);
        leaky.extra.insert("rls_enabled".into(), json!(false));
        let kg = graph(vec![secure, leaky], vec![]);
        let rows = run(
            &kg,
            r#"MATCH (t:table) WHERE t.rls_enabled = "false" RETURN t"#,
        );
        assert!(rows.contains(&"leaky".to_string()), "rows: {rows:?}");
        assert!(!rows.contains(&"secure".to_string()), "rows: {rows:?}");
    }

    #[test]
    fn node_views_expose_signature_and_kind() {
        use codegraph_core::{Param, Signature};
        let mut greet = node("greet", "greet()", NodeKind::Function, 3);
        greet.set_signature(Signature {
            params: vec![Param {
                name: "name".into(),
                type_ref: Some("str".into()),
            }],
            return_type: Some("str".into()),
            raw: "def greet(name: str) -> str".into(),
        });
        let kg = graph(vec![greet], vec![]);
        let res = run_query(&kg, &parse("MATCH (f:function) RETURN f").unwrap());
        let views = res.node_views(&kg);
        assert_eq!(views.len(), 1, "one matched row");
        let v = &views[0][0];
        assert_eq!(v.label, "greet()");
        assert_eq!(v.kind.as_deref(), Some("function"));
        let sig = v.signature.as_ref().expect("signature present in view");
        assert_eq!(sig.params[0].name, "name");
        assert_eq!(sig.return_type.as_deref(), Some("str"));
    }

    #[test]
    fn property_filter_loc_and_kind() {
        let mut big = node("big", "Big", NodeKind::Class, 600);
        big.set_visibility(Visibility::Public);
        let kg = graph(
            vec![
                big,
                node("small", "Small", NodeKind::Class, 10),
                node("fun", "f()", NodeKind::Function, 5),
            ],
            vec![],
        );
        assert_eq!(
            run(&kg, "MATCH (c:class) WHERE c.loc > 100 RETURN c"),
            vec!["big"]
        );
        assert_eq!(
            run(
                &kg,
                "MATCH (c:class) WHERE c.visibility = \"public\" RETURN c"
            ),
            vec!["big"]
        );
        // kind filter excludes the function.
        let mut classes = run(&kg, "MATCH (c:class) RETURN c");
        classes.sort();
        assert_eq!(classes, vec!["big", "small"]);
    }

    #[test]
    fn name_matches_bare_identifier_not_the_call_label() {
        let kg = graph(
            vec![
                node("announce", "announce()", NodeKind::Function, 5),
                node("user", "User", NodeKind::Class, 5),
            ],
            vec![],
        );
        // `.name` is the bare identifier: a plain equality matches without `()`.
        assert_eq!(
            run(
                &kg,
                "MATCH (f:function) WHERE f.name = \"announce\" RETURN f"
            ),
            vec!["announce"]
        );
        // The decorated label no longer matches `.name` (parens are stripped).
        assert!(run(
            &kg,
            "MATCH (f:function) WHERE f.name = \"announce()\" RETURN f"
        )
        .is_empty());
        // A class label is already bare and keeps matching.
        assert_eq!(
            run(&kg, "MATCH (c:class) WHERE c.name = \"User\" RETURN c"),
            vec!["user"]
        );
        // Partial matches still use the `=~` regex operator.
        assert_eq!(
            run(
                &kg,
                "MATCH (f:function) WHERE f.name =~ \"nounce\" RETURN f"
            ),
            vec!["announce"]
        );
    }

    #[test]
    fn fan_in_zero_finds_uncalled() {
        let kg = graph(
            vec![
                node("a", "a()", NodeKind::Function, 3),
                node("b", "b()", NodeKind::Function, 3),
            ],
            vec![edge("a", "b", "calls")], // b is called, a is not
        );
        assert_eq!(
            run(&kg, "MATCH (f:function) WHERE f.fan_in = 0 RETURN f"),
            vec!["a"]
        );
    }

    #[test]
    fn regex_on_name() {
        let kg = graph(
            vec![
                node("foo", "FooService", NodeKind::Class, 3),
                node("bar", "BarWidget", NodeKind::Class, 3),
            ],
            vec![],
        );
        assert_eq!(
            run(&kg, "MATCH (c) WHERE c.name =~ \"Service$\" RETURN c"),
            vec!["foo"]
        );
    }

    #[test]
    fn relationship_pattern_join() {
        let kg = graph(
            vec![
                node("A", "A", NodeKind::Class, 3),
                node("I", "I", NodeKind::Interface, 3),
            ],
            vec![edge("A", "I", "implements")],
        );
        let res = run_query(
            &kg,
            &parse("MATCH (a:class)-[:implements]->(b:interface) RETURN a, b").unwrap(),
        );
        assert_eq!(res.rows, vec![vec![NodeId("A".into()), NodeId("I".into())]]);
        // Wrong direction yields nothing.
        let empty = run_query(
            &kg,
            &parse("MATCH (a:class)<-[:implements]-(b:interface) RETURN a").unwrap(),
        );
        assert!(empty.rows.is_empty());
    }

    #[test]
    fn invalid_regex_is_rejected() {
        let bad = parse("MATCH (c) WHERE c.name =~ \"(\" RETURN c").unwrap();
        assert!(validate_query(&bad).is_err());
        let good = parse("MATCH (c) WHERE c.name =~ \"^ok$\" RETURN c").unwrap();
        assert!(validate_query(&good).is_ok());
    }

    #[test]
    fn synthetic_anon_var_does_not_clobber_user_var() {
        // A user var named `_1` must not be overwritten by an anonymous node's
        // synthetic binding. `a -calls-> b`; RETURN _1 should be the source `a`.
        let kg = graph(
            vec![
                node("a", "a()", NodeKind::Function, 3),
                node("b", "b()", NodeKind::Function, 3),
            ],
            vec![edge("a", "b", "calls")],
        );
        let res = run_query(&kg, &parse("MATCH (_1)-[:calls]->() RETURN _1").unwrap());
        assert_eq!(res.rows, vec![vec![NodeId("a".into())]]);
    }

    #[test]
    fn variable_length_path_reaches_transitively() {
        // a -> b -> c -> d (calls chain)
        let kg = graph(
            vec![
                node("a", "a()", NodeKind::Function, 3),
                node("b", "b()", NodeKind::Function, 3),
                node("c", "c()", NodeKind::Function, 3),
                node("d", "d()", NodeKind::Function, 3),
            ],
            vec![
                edge("a", "b", "calls"),
                edge("b", "c", "calls"),
                edge("c", "d", "calls"),
            ],
        );
        // 1..2 hops from a reaches b and c, not d.
        let res = run_query(
            &kg,
            &parse("MATCH (a)-[:calls*1..2]->(x) WHERE a.name =~ \"^a\" RETURN x").unwrap(),
        );
        let mut got: Vec<String> = res.rows.iter().map(|r| r[0].0.clone()).collect();
        got.sort();
        assert_eq!(got, vec!["b", "c"]);
    }

    #[test]
    fn unbounded_star_terminates_on_a_cycle() {
        // a <-> b cycle; `*` must not loop forever.
        let kg = graph(
            vec![
                node("a", "a()", NodeKind::Function, 3),
                node("b", "b()", NodeKind::Function, 3),
            ],
            vec![edge("a", "b", "calls"), edge("b", "a", "calls")],
        );
        let res = run_query(&kg, &parse("MATCH (a)-[:calls*]->(x) RETURN x").unwrap());
        assert!(!res.rows.is_empty());
    }

    #[test]
    fn aggregation_counts_per_group() {
        let mut a = node("a", "A", NodeKind::Class, 3);
        a.community = Some(1);
        let mut b = node("b", "B", NodeKind::Class, 3);
        b.community = Some(1);
        let mut c = node("c", "C", NodeKind::Class, 3);
        c.community = Some(2);
        let kg = graph(vec![a, b, c], vec![]);
        let res = run_query(
            &kg,
            &parse("MATCH (c:class) RETURN c.community, count(c)").unwrap(),
        );
        assert_eq!(res.columns, vec!["c.community", "count"]);
        let agg = res.aggregates.expect("aggregate output");
        // community 1 has 2, community 2 has 1 (sorted by key).
        assert_eq!(agg, vec![vec!["1", "2"], vec!["2", "1"]]);
        assert!(res.rows.is_empty());
    }

    #[test]
    fn count_star_totals() {
        let kg = graph(
            vec![
                node("a", "A", NodeKind::Class, 3),
                node("b", "B", NodeKind::Class, 3),
            ],
            vec![],
        );
        let res = run_query(&kg, &parse("MATCH (c:class) RETURN count(*)").unwrap());
        assert_eq!(res.aggregates.unwrap(), vec![vec!["2"]]);
    }

    #[test]
    fn limit_truncates() {
        let kg = graph(
            vec![
                node("a", "a", NodeKind::Class, 3),
                node("b", "b", NodeKind::Class, 3),
                node("c", "c", NodeKind::Class, 3),
            ],
            vec![],
        );
        assert_eq!(run(&kg, "MATCH (c:class) RETURN c LIMIT 2").len(), 2);
    }
}
