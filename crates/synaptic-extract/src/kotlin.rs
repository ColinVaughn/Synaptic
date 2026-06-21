//! Kotlin extractor — Bucket A (declarative `LanguageConfig` + positional-body /
//! positional-callee walker fallbacks). Covers the Kotlin config, imports, and
//! type-reference collection.
//!
//! Classes/objects → bare name; functions → `name()` / `.name()`; `import a.b.C`
//! → `imports` edge to the tail; `delegation_specifiers` give `inherits` (a
//! `constructor_invocation` base) / `implements` (a bare `user_type` base);
//! parameter/return types → `references`.

#[cfg(feature = "lang-kotlin")]
use crate::config::{HeritageStyle, ImportStyle, LanguageConfig, TypeRefStyle};
#[cfg(feature = "lang-kotlin")]
use crate::result::ExtractionResult;
#[cfg(feature = "lang-kotlin")]
use crate::walker::extract_with_config;

/// The Kotlin `LanguageConfig`. The grammar attaches `class_body`/`function_body`
/// positionally (no `body` field) and names the callee of a `call_expression`
/// positionally, so this relies on the walker's `body_kinds` and empty-
/// `call_function_field` fallbacks.
#[cfg(feature = "lang-kotlin")]
pub fn kotlin_config() -> LanguageConfig {
    LanguageConfig {
        language: || tree_sitter_kotlin_ng::LANGUAGE.into(),
        class_types: &["class_declaration", "object_declaration"],
        function_types: &["function_declaration"],
        call_types: &["call_expression"],
        name_field: "name",
        body_field: "body",
        call_function_field: "",
        call_accessor_node_types: &[],
        call_accessor_field: "",
        function_boundary_types: &["function_declaration"],
        superclasses_field: None,
        decorated_types: &[],
        builtins: &[],
        import_types: &["import"],
        import_style: Some(ImportStyle::Kotlin),
        type_ref_style: Some(TypeRefStyle::Kotlin),
        heritage_style: Some(HeritageStyle::Kotlin),
        constructor_call_type: None,
        body_kinds: &["class_body", "function_body", "enum_class_body"],
    }
}

/// Extract a Kotlin source file already in memory.
#[cfg(feature = "lang-kotlin")]
pub fn extract_kotlin_source(path: &str, source: &[u8]) -> ExtractionResult {
    extract_with_config(path, source, &kotlin_config())
}

/// Read and extract a Kotlin file from disk.
#[cfg(feature = "lang-kotlin")]
pub fn extract_kotlin_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_kotlin_source(&path_str, &source))
}

#[cfg(all(test, feature = "lang-kotlin"))]
mod tests {
    use super::extract_kotlin_source;
    use crate::result::ExtractionResult;
    use synaptic_core::Confidence;

    const SAMPLE: &[u8] = br#"
package demo

import kotlin.io.File

interface Greeter {
    fun greet(): String
}

open class Animal {
    fun breathe() {}
}

class Dog(name: String) : Animal(), Greeter {
    override fun greet(): String = makeSound()
    fun makeSound(): String = "woof"
    fun feed(food: Food) { breathe() }
}
"#;

    fn extract() -> ExtractionResult {
        extract_kotlin_source("src/demo/Sample.kt", SAMPLE)
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
    fn class_and_interface_nodes() {
        let r = extract();
        let ls = labels(&r);
        assert!(ls.contains(&"Dog".to_string()), "{ls:?}");
        assert!(ls.contains(&"Animal".to_string()));
        assert!(ls.contains(&"Greeter".to_string()));
    }

    #[test]
    fn methods_scoped_under_class() {
        let r = extract();
        let ls = labels(&r);
        assert!(ls.contains(&".makeSound()".to_string()), "{ls:?}");
        assert!(ls.contains(&".breathe()".to_string()));
        assert!(ls.contains(&".greet()".to_string()));
        assert!(ls.contains(&".feed()".to_string()));
    }

    #[test]
    fn import_emits_tail() {
        let r = extract();
        let imps = rels(&r, "imports");
        assert!(
            imps.iter().any(|(_, t)| t == "File"),
            "expected import tail File; got {imps:?}"
        );
    }

    #[test]
    fn delegation_specifiers_inherits_and_implements() {
        let r = extract();
        let inh = rels(&r, "inherits");
        let imp = rels(&r, "implements");
        assert!(
            inh.contains(&("Dog".to_string(), "Animal".to_string())),
            "inherits: {inh:?}"
        );
        assert!(
            imp.contains(&("Dog".to_string(), "Greeter".to_string())),
            "implements: {imp:?}"
        );
    }

    #[test]
    fn parameter_and_return_type_references() {
        let r = extract();
        let refs: Vec<(String, String)> = r
            .edges
            .iter()
            .filter(|e| e.relation == "references")
            .map(|e| {
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
            "refs: {refs:?}"
        );
        assert!(refs.contains(&("String".to_string(), "return_type".to_string())));
    }

    #[test]
    fn intra_file_calls_resolve() {
        let r = extract();
        let calls = rels(&r, "calls");
        assert!(
            calls.contains(&(".greet()".to_string(), ".makeSound()".to_string())),
            "calls: {calls:?}"
        );
        assert!(calls.contains(&(".feed()".to_string(), ".breathe()".to_string())));
    }

    #[test]
    fn properties_and_member_calls() {
        let r = extract_kotlin_source(
            "F.kt",
            b"class C(val tag: Tag) {\n  val leash: Leash = Leash()\n  fun helper(): Int = 1\n  fun go(c: C) { c.helper() }\n}\n",
        );
        let refs: Vec<(String, String)> = r
            .edges
            .iter()
            .filter(|e| e.relation == "references")
            .map(|e| {
                let tgt = r
                    .nodes
                    .iter()
                    .find(|n| n.id == e.target)
                    .map(|n| n.label.clone())
                    .unwrap_or_else(|| e.target.0.clone());
                (tgt, e.context.clone().unwrap_or_default())
            })
            .collect();
        // primary-constructor property `val tag: Tag` and body property `leash: Leash`
        assert!(
            refs.contains(&("Tag".to_string(), "field".to_string())),
            "{refs:?}"
        );
        assert!(refs.contains(&("Leash".to_string(), "field".to_string())));
        // member call `c.helper()` resolves to the in-file method by name.
        let calls = rels(&r, "calls");
        assert!(
            calls.contains(&(".go()".to_string(), ".helper()".to_string())),
            "calls: {calls:?}"
        );
    }

    #[test]
    fn structural_edges_are_extracted_confidence() {
        let r = extract();
        for e in &r.edges {
            if matches!(e.relation.as_str(), "contains" | "method" | "inherits") {
                assert_eq!(e.confidence, Confidence::Extracted, "edge {e:?}");
            }
        }
    }
}
