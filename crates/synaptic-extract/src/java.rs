//! Java extractor — Bucket A (declarative `LanguageConfig`). Covers the Java
//! config, imports, and type-reference collection.
//!
//! Classes/interfaces → bare name; methods/constructors → `.name()` under the
//! class; `import a.b.C` → `imports` edge to the tail (`C`); `extends`/
//! `implements` → `inherits`/`implements`; parameter/return types → `references`.

#[cfg(feature = "lang-java")]
use crate::config::{HeritageStyle, ImportStyle, LanguageConfig, TypeRefStyle};
#[cfg(feature = "lang-java")]
use crate::result::ExtractionResult;
#[cfg(feature = "lang-java")]
use crate::walker::extract_with_config;

/// The Java `LanguageConfig`. Calls are `method_invocation` nodes whose `name`
/// field is the callee method (resolved intra-file by label, like the other
/// languages). Java has no decorated-wrapper node and no callable builtins to
/// filter (annotations are modifiers, not call targets).
#[cfg(feature = "lang-java")]
pub fn java_config() -> LanguageConfig {
    LanguageConfig {
        language: || tree_sitter_java::LANGUAGE.into(),
        class_types: &["class_declaration", "interface_declaration"],
        function_types: &["method_declaration", "constructor_declaration"],
        call_types: &["method_invocation"],
        name_field: "name",
        body_field: "body",
        call_function_field: "name",
        call_accessor_node_types: &[],
        call_accessor_field: "",
        function_boundary_types: &["method_declaration", "constructor_declaration"],
        superclasses_field: None,
        decorated_types: &[],
        builtins: &[],
        import_types: &["import_declaration"],
        import_style: Some(ImportStyle::Java),
        type_ref_style: Some(TypeRefStyle::Java),
        heritage_style: Some(HeritageStyle::Java),
        constructor_call_type: None,
        body_kinds: &[],
    }
}

/// Extract a Java source file already in memory.
#[cfg(feature = "lang-java")]
pub fn extract_java_source(path: &str, source: &[u8]) -> ExtractionResult {
    extract_with_config(path, source, &java_config())
}

/// Read and extract a Java file from disk.
#[cfg(feature = "lang-java")]
pub fn extract_java_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_java_source(&path_str, &source))
}

#[cfg(all(test, feature = "lang-java"))]
mod tests {
    use super::extract_java_source;
    use crate::result::ExtractionResult;
    use synaptic_core::Confidence;

    const SAMPLE: &[u8] = br#"
package com.example;

import java.util.List;
import java.io.IOException;

interface Greeter {
    String greet();
}

class Animal {
    void breathe() {}
}

class Dog extends Animal implements Greeter {
    public String greet() {
        return makeSound();
    }

    String makeSound() {
        return "woof";
    }

    void feed(Food food) {
        breathe();
    }
}
"#;

    fn extract() -> ExtractionResult {
        extract_java_source("src/com/example/Sample.java", SAMPLE)
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
        assert!(ls.contains(&"Sample.java".to_string())); // file node
    }

    #[test]
    fn methods_scoped_under_class() {
        let r = extract();
        let ls = labels(&r);
        assert!(ls.contains(&".makeSound()".to_string()), "{ls:?}");
        assert!(ls.contains(&".breathe()".to_string()));
        assert!(ls.contains(&".greet()".to_string()));
        // `method` edges: Animal->.breathe(), Dog->.greet(), Dog->.makeSound(),
        // Dog->.feed(), Greeter->.greet() = 5
        assert_eq!(rels(&r, "method").len(), 5, "method edges");
    }

    #[test]
    fn imports_emit_dotted_name_tail() {
        let r = extract();
        let imps = rels(&r, "imports");
        assert!(
            imps.iter().any(|(_, t)| t == "List"),
            "expected import tail List; got {imps:?}"
        );
        assert!(imps.iter().any(|(_, t)| t == "IOException"));
    }

    #[test]
    fn inherits_and_implements() {
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
        // feed(Food food) -> Food parameter_type; greet()/makeSound() -> String return_type
        assert!(
            refs.contains(&("Food".to_string(), "parameter_type".to_string())),
            "refs: {refs:?}"
        );
        assert!(refs.contains(&("String".to_string(), "return_type".to_string())));
    }

    #[test]
    fn method_annotations_referenced_as_attributes() {
        // `@Override` / `@Test` on a method gives a references edge (context "attribute").
        let r = extract_java_source(
            "T.java",
            b"class T {\n  @Override\n  public String toString() { return \"\"; }\n}\n",
        );
        assert!(
            refs_with_ctx(&r).contains(&("Override".to_string(), "attribute".to_string())),
            "refs: {:?}",
            refs_with_ctx(&r)
        );
    }

    fn refs_with_ctx(r: &ExtractionResult) -> Vec<(String, String)> {
        r.edges
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
            .collect()
    }

    #[test]
    fn field_types_referenced() {
        let r = extract_java_source(
            "F.java",
            b"class C {\n  private Leash leash;\n  java.util.List<Toy> toys;\n}\n",
        );
        let refs = refs_with_ctx(&r);
        assert!(
            refs.contains(&("Leash".to_string(), "field".to_string())),
            "{refs:?}"
        );
        assert!(refs.contains(&("Toy".to_string(), "generic_arg".to_string())));
    }

    #[test]
    fn intra_file_calls_resolve_by_method_name() {
        let r = extract();
        let calls = rels(&r, "calls");
        // greet() calls makeSound(); feed() calls breathe()
        assert!(
            calls.contains(&(".greet()".to_string(), ".makeSound()".to_string())),
            "calls: {calls:?}"
        );
        assert!(calls.contains(&(".feed()".to_string(), ".breathe()".to_string())));
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

    #[test]
    fn no_dangling_edge_sources() {
        let r = extract();
        let ids: std::collections::HashSet<_> = r.nodes.iter().map(|n| n.id.clone()).collect();
        for e in &r.edges {
            assert!(ids.contains(&e.source), "dangling source: {}", e.source);
        }
    }
}
