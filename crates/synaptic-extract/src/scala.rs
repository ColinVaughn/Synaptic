//! Scala extractor — Bucket A (declarative `LanguageConfig`).
//!
//! `class`/`object`/`trait` → bare name; `def` → `.name()` / `name()`;
//! `import a.b.C` → `imports` to the last path id; `extends A with B` →
//! `inherits` (first) / `mixes_in` (rest); parameter/return types → `references`;
//! `f(x)` / `obj.m()` → `calls`.

#[cfg(feature = "lang-scala")]
use crate::config::{HeritageStyle, ImportStyle, LanguageConfig, TypeRefStyle};
#[cfg(feature = "lang-scala")]
use crate::result::ExtractionResult;
#[cfg(feature = "lang-scala")]
use crate::walker::extract_with_config;

/// The Scala `LanguageConfig`. Member calls (`obj.m()`) come through a
/// `field_expression` accessor; plain calls use the `function` field.
#[cfg(feature = "lang-scala")]
pub fn scala_config() -> LanguageConfig {
    LanguageConfig {
        language: || tree_sitter_scala::LANGUAGE.into(),
        class_types: &["class_definition", "object_definition", "trait_definition"],
        function_types: &["function_definition"],
        call_types: &["call_expression"],
        name_field: "name",
        body_field: "body",
        call_function_field: "function",
        call_accessor_node_types: &["field_expression"],
        call_accessor_field: "field",
        function_boundary_types: &["function_definition"],
        superclasses_field: None,
        decorated_types: &[],
        builtins: &[],
        import_types: &["import_declaration"],
        import_style: Some(ImportStyle::Scala),
        type_ref_style: Some(TypeRefStyle::Scala),
        heritage_style: Some(HeritageStyle::Scala),
        constructor_call_type: None,
        body_kinds: &[],
    }
}

/// Extract a Scala source file already in memory.
#[cfg(feature = "lang-scala")]
pub fn extract_scala_source(path: &str, source: &[u8]) -> ExtractionResult {
    extract_with_config(path, source, &scala_config())
}

/// Read and extract a Scala file from disk.
#[cfg(feature = "lang-scala")]
pub fn extract_scala_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_scala_source(&path_str, &source))
}

#[cfg(all(test, feature = "lang-scala"))]
mod tests {
    use super::extract_scala_source;
    use crate::result::ExtractionResult;

    const SAMPLE: &[u8] = b"package p\nimport a.B\n\nclass Dog extends Animal with Walkable {\n  def bark(food: Food): String = sound(food)\n  def sound(f: Food): String = \"woof\"\n}\n";

    fn extract() -> ExtractionResult {
        extract_scala_source("src/Dog.scala", SAMPLE)
    }

    fn labels(r: &ExtractionResult) -> Vec<String> {
        r.nodes.iter().map(|n| n.label.clone()).collect()
    }

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

    #[test]
    fn class_and_method_nodes() {
        let ls = labels(&extract());
        assert!(ls.contains(&"Dog".to_string()), "{ls:?}");
        assert!(ls.contains(&".bark()".to_string()));
        assert!(ls.contains(&".sound()".to_string()));
    }

    #[test]
    fn import_tail() {
        assert!(rels(&extract(), "imports").iter().any(|(_, t)| t == "B"));
    }

    #[test]
    fn extends_with_inherits_and_mixes_in() {
        let r = extract();
        assert!(rels(&r, "inherits").contains(&("Dog".to_string(), "Animal".to_string())));
        assert!(rels(&r, "mixes_in").contains(&("Dog".to_string(), "Walkable".to_string())));
    }

    #[test]
    fn param_and_return_type_references() {
        let refs: Vec<(String, String)> = extract()
            .edges
            .iter()
            .filter(|e| e.relation == "references")
            .map(|e| {
                let r = extract();
                let tgt = r
                    .nodes
                    .iter()
                    .find(|n| n.id == e.target)
                    .map(|n| n.label.clone())
                    .unwrap_or_else(|| e.target.0.clone());
                (tgt, e.context.clone().unwrap_or_default())
            })
            .collect();
        assert!(
            refs.contains(&("Food".to_string(), "parameter_type".to_string())),
            "{refs:?}"
        );
        assert!(refs.contains(&("String".to_string(), "return_type".to_string())));
    }

    #[test]
    fn calls_resolve() {
        assert!(
            rels(&extract(), "calls").contains(&(".bark()".to_string(), ".sound()".to_string())),
            "{:?}",
            rels(&extract(), "calls")
        );
    }
}
