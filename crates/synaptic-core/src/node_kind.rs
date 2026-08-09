use serde::{Deserialize, Serialize};

/// What a code node represents. `Other` is the safe fallback for declarations a
/// language extractor cannot classify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Module,
    Namespace,
    Package,
    Class,
    Interface,
    Trait,
    Struct,
    Enum,
    Protocol,
    Object,
    Function,
    Method,
    Constructor,
    Property,
    Field,
    Constant,
    Variable,
    TypeAlias,
    Macro,
    Table,
    View,
    Column,
    Index,
    Trigger,
    Procedure,
    Policy,
    Role,
    Other,
}

impl NodeKind {
    /// The snake_case wire string (matches the serde representation).
    pub fn as_str(&self) -> &'static str {
        use NodeKind::*;
        match self {
            Module => "module",
            Namespace => "namespace",
            Package => "package",
            Class => "class",
            Interface => "interface",
            Trait => "trait",
            Struct => "struct",
            Enum => "enum",
            Protocol => "protocol",
            Object => "object",
            Function => "function",
            Method => "method",
            Constructor => "constructor",
            Property => "property",
            Field => "field",
            Constant => "constant",
            Variable => "variable",
            TypeAlias => "type_alias",
            Macro => "macro",
            Table => "table",
            View => "view",
            Column => "column",
            Index => "index",
            Trigger => "trigger",
            Procedure => "procedure",
            Policy => "policy",
            Role => "role",
            Other => "other",
        }
    }
}

/// Declared visibility. A node with no visibility set (the `Node::visibility`
/// accessor returning `None`) means unknown / not applicable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Public,
    Protected,
    Private,
    Internal,
}

impl Visibility {
    /// The snake_case wire string (matches the serde representation).
    pub fn as_str(&self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Protected => "protected",
            Visibility::Private => "private",
            Visibility::Internal => "internal",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_kinds_have_snake_case_wire_strings() {
        assert_eq!(NodeKind::Table.as_str(), "table");
        assert_eq!(NodeKind::View.as_str(), "view");
        assert_eq!(NodeKind::Column.as_str(), "column");
        assert_eq!(NodeKind::Index.as_str(), "index");
        assert_eq!(NodeKind::Trigger.as_str(), "trigger");
        assert_eq!(NodeKind::Procedure.as_str(), "procedure");
        assert_eq!(NodeKind::Policy.as_str(), "policy");
        assert_eq!(NodeKind::Role.as_str(), "role");
    }

    #[test]
    fn sql_kinds_roundtrip_through_serde() {
        for k in [
            NodeKind::Table,
            NodeKind::View,
            NodeKind::Column,
            NodeKind::Index,
            NodeKind::Trigger,
            NodeKind::Procedure,
            NodeKind::Policy,
            NodeKind::Role,
        ] {
            let json = serde_json::to_string(&k).unwrap();
            let back: NodeKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, k, "roundtrip {json}");
        }
    }
}

/// A node's `kind` exactly as `graph.json` carries it.
///
/// Code symbols use the [`NodeKind`] vocabulary, but the key is shared: the API
/// coverage layer serializes an `ExternalSurfaceKind` (`sdk`, `http`,
/// `dynamic_dispatch`, …) onto its observation nodes under the same key.
/// Unknown values are therefore preserved verbatim so a load/save round-trip
/// stays byte-identical, and [`crate::Node::kind`] reports `None` for them —
/// exactly the lenient behaviour the old `extra`-backed accessor had, which
/// silently dropped any value that failed to parse as a `NodeKind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KindValue {
    /// One of the known code-symbol kinds.
    Known(NodeKind),
    /// A value from another layer's vocabulary, kept as written.
    Other(String),
}

#[cfg(test)]
mod kind_value_tests {
    use super::*;

    #[test]
    fn known_kinds_parse_into_the_typed_variant() {
        let v: KindValue = serde_json::from_str("\"class\"").unwrap();
        assert_eq!(v, KindValue::Known(NodeKind::Class));
        assert_eq!(serde_json::to_string(&v).unwrap(), "\"class\"");
    }

    #[test]
    fn foreign_vocabularies_round_trip_verbatim() {
        // `sdk` / `http` / `dynamic_dispatch` come from ExternalSurfaceKind.
        for raw in ["\"sdk\"", "\"http\"", "\"dynamic_dispatch\""] {
            let v: KindValue = serde_json::from_str(raw).unwrap();
            assert!(matches!(v, KindValue::Other(_)), "{raw} is not a NodeKind");
            assert_eq!(
                serde_json::to_string(&v).unwrap(),
                raw,
                "{raw} survives a round trip byte-for-byte"
            );
        }
    }
}

/// The known values of a node's `_origin` tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OriginKind {
    /// Extracted from a parsed syntax tree — the overwhelming majority.
    Ast,
    /// Emitted by the resource indexer for a data/asset file.
    Resource,
    /// Emitted by the T-SQL semantic layer.
    Tsql,
    /// Emitted by the LLM semantic pass (`--semantic`).
    Semantic,
}

impl OriginKind {
    /// The tag exactly as `graph.json` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            OriginKind::Ast => "ast",
            OriginKind::Resource => "resource",
            OriginKind::Tsql => "tsql",
            OriginKind::Semantic => "semantic",
        }
    }
}

/// A node's `_origin` tag: which layer produced it.
///
/// `ast` covers ~74% of nodes, so the known values are held inline with no heap
/// allocation at all. Anything else is preserved verbatim, so a load/save round
/// trip stays byte-identical and a new extractor can introduce an origin without
/// a change here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Origin {
    Known(OriginKind),
    Other(Box<str>),
}

impl Origin {
    /// The tag exactly as `graph.json` spells it.
    pub fn as_str(&self) -> &str {
        match self {
            Origin::Known(k) => k.as_str(),
            Origin::Other(s) => s,
        }
    }
}

impl From<&str> for Origin {
    fn from(s: &str) -> Self {
        match s {
            "ast" => Origin::Known(OriginKind::Ast),
            "resource" => Origin::Known(OriginKind::Resource),
            "tsql" => Origin::Known(OriginKind::Tsql),
            "semantic" => Origin::Known(OriginKind::Semantic),
            other => Origin::Other(other.into()),
        }
    }
}

#[cfg(test)]
mod origin_tests {
    use super::*;

    #[test]
    fn known_origins_parse_into_the_typed_variant() {
        for (raw, want) in [
            ("\"ast\"", OriginKind::Ast),
            ("\"resource\"", OriginKind::Resource),
            ("\"tsql\"", OriginKind::Tsql),
            ("\"semantic\"", OriginKind::Semantic),
        ] {
            let o: Origin = serde_json::from_str(raw).unwrap();
            assert_eq!(o, Origin::Known(want));
            assert_eq!(serde_json::to_string(&o).unwrap(), raw);
            assert_eq!(o.as_str(), raw.trim_matches('"'));
            assert_eq!(Origin::from(raw.trim_matches('"')), o, "From<&str> agrees");
        }
    }

    #[test]
    fn unknown_origins_round_trip_verbatim() {
        let o: Origin = serde_json::from_str("\"handwritten\"").unwrap();
        assert!(matches!(o, Origin::Other(_)), "not a known origin");
        assert_eq!(serde_json::to_string(&o).unwrap(), "\"handwritten\"");
        assert_eq!(o.as_str(), "handwritten");
    }
}
