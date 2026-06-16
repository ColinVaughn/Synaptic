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
