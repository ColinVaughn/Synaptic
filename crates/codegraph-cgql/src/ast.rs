//! CGQL abstract syntax tree.

/// Relationship direction in a pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// `-[:r]->`
    LtoR,
    /// `<-[:r]-`
    RtoL,
    /// `-[:r]-` (either orientation)
    Either,
}

/// A node pattern `(var:kind)` — both parts optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePat {
    pub var: Option<String>,
    pub kind: Option<String>,
}

/// A relationship pattern between two node patterns. `min`/`max` give the
/// variable-length bound (`-[:r*1..3]->`); both are 1 for a plain single step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelPat {
    pub rel: Option<String>,
    pub dir: Dir,
    pub min: u32,
    pub max: u32,
}

/// Upper bound for an unbounded `*` repetition, to keep BFS terminating.
pub const VARLEN_CAP: u32 = 8;

/// A path pattern: `nodes.len() == rels.len() + 1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    pub nodes: Vec<NodePat>,
    pub rels: Vec<RelPat>,
}

/// A queryable node property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Kind,
    Name,
    File,
    Lang,
    Visibility,
    Loc,
    FanIn,
    FanOut,
    Degree,
    Community,
    RlsEnabled,
    Dialect,
    Operation,
}

impl Field {
    /// Parse a field name (lowercase) — `None` if unknown.
    pub fn parse(s: &str) -> Option<Field> {
        Some(match s.to_ascii_lowercase().as_str() {
            "kind" => Field::Kind,
            "name" => Field::Name,
            "file" => Field::File,
            "lang" => Field::Lang,
            "visibility" => Field::Visibility,
            "loc" => Field::Loc,
            "fan_in" => Field::FanIn,
            "fan_out" => Field::FanOut,
            "degree" => Field::Degree,
            "community" => Field::Community,
            "rls_enabled" => Field::RlsEnabled,
            "dialect" => Field::Dialect,
            "operation" => Field::Operation,
            _ => return None,
        })
    }

    /// All valid field names, for error messages.
    pub fn valid_names() -> &'static str {
        "kind, name, file, lang, visibility, loc, fan_in, fan_out, degree, community, rls_enabled, dialect, operation"
    }
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Regex,
}

/// A literal value on the right side of a comparison.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Num(f64),
}

/// `var.field`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prop {
    pub var: String,
    pub field: Field,
}

/// A WHERE expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Cmp(Prop, Op, Value),
    /// `has(var, "modifier")` — reserved; evaluates false until modifiers exist.
    Has(String, String),
}

/// One RETURN item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetItem {
    /// `RETURN c` — a bound node variable.
    Var(String),
    /// `RETURN c.community` — a property, used as a group key under aggregation.
    Prop(Prop),
    /// `count(c)` / `count(*)` — an aggregate over the matched rows.
    Count(Option<String>),
}

impl RetItem {
    /// Column header for this item.
    pub fn header(&self) -> String {
        match self {
            RetItem::Var(v) => v.clone(),
            RetItem::Prop(p) => format!("{}.{}", p.var, field_name(p.field)),
            RetItem::Count(_) => "count".to_string(),
        }
    }

    /// The bound variable this item references, if any (for validation).
    pub fn var(&self) -> Option<&str> {
        match self {
            RetItem::Var(v) => Some(v),
            RetItem::Prop(p) => Some(&p.var),
            RetItem::Count(v) => v.as_deref(),
        }
    }
}

/// Lowercase field name (inverse of [`Field::parse`]).
pub fn field_name(f: Field) -> &'static str {
    match f {
        Field::Kind => "kind",
        Field::Name => "name",
        Field::File => "file",
        Field::Lang => "lang",
        Field::Visibility => "visibility",
        Field::Loc => "loc",
        Field::FanIn => "fan_in",
        Field::FanOut => "fan_out",
        Field::Degree => "degree",
        Field::Community => "community",
        Field::RlsEnabled => "rls_enabled",
        Field::Dialect => "dialect",
        Field::Operation => "operation",
    }
}

/// A full query.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub pattern: Pattern,
    pub where_: Option<Expr>,
    pub ret: Vec<RetItem>,
    pub limit: Option<usize>,
}

impl Query {
    /// True when any RETURN item is an aggregate (`count`).
    pub fn is_aggregate(&self) -> bool {
        self.ret.iter().any(|r| matches!(r, RetItem::Count(_)))
    }
}
