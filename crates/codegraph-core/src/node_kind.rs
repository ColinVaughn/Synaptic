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
