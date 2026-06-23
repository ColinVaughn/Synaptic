//! Dynamic-dispatch site metadata. Recorded by the extractor on the enclosing
//! node and consumed by the query layer's honesty caveat. See
//! `design/2026-06-22-dynamic-dispatch-awareness-design.md`.

use serde::{Deserialize, Serialize};

/// The flavor of a dynamic-dispatch site. These are the kinds the reflection
/// detectors actually emit as `dynamic_sites`; event buses are modeled as edges
/// (an `event #<name>` channel), not sites, so they have no kind here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DynamicKind {
    /// Reflective / computed dispatch by name: `obj[expr]()`, `Reflect.*`, a
    /// dispatch table, .NET `GetMethod`, Python `getattr`, JVM `Class.forName`.
    Reflection,
    /// Dynamic module import with a non-literal specifier (`import(expr)`,
    /// `importlib.import_module`).
    DynamicImport,
    /// `eval` / `new Function`.
    Eval,
}

impl DynamicKind {
    /// The snake_case wire string (matches the serde representation).
    pub fn as_str(&self) -> &'static str {
        match self {
            DynamicKind::Reflection => "reflection",
            DynamicKind::DynamicImport => "dynamic_import",
            DynamicKind::Eval => "eval",
        }
    }
}

/// One dynamic-dispatch site, recorded against the node that encloses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DynamicSite {
    pub kind: DynamicKind,
    /// 1-based source line.
    pub line: u32,
    /// The dispatched name when it is a string literal (evidence-link candidate);
    /// `None` when the name is computed/opaque (catalog-only).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub key: Option<String>,
    /// Trimmed source snippet for human display.
    pub snippet: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_roundtrips_and_omits_absent_key() {
        let site = DynamicSite {
            kind: DynamicKind::Reflection,
            line: 12,
            key: None,
            snippet: "getattr(o, name)".into(),
        };
        let v = serde_json::to_value(&site).unwrap();
        assert_eq!(v.get("kind").unwrap(), "reflection");
        assert!(v.get("key").is_none(), "absent key omitted");
        let back: DynamicSite = serde_json::from_value(v).unwrap();
        assert_eq!(back, site);
    }

    #[test]
    fn kind_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(DynamicKind::DynamicImport).unwrap(),
            serde_json::json!("dynamic_import")
        );
    }

    #[test]
    fn as_str_matches_serde() {
        for k in [
            DynamicKind::Reflection,
            DynamicKind::DynamicImport,
            DynamicKind::Eval,
        ] {
            assert_eq!(
                serde_json::to_value(k).unwrap(),
                serde_json::json!(k.as_str())
            );
        }
    }
}
