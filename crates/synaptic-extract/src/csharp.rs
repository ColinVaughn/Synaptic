//! C# extractor — Bucket A (declarative `LanguageConfig` + interface pre-scan).
//! Covers the C# config, imports, base-type classification, and type-reference
//! collection.
//!
//! Classes/interfaces/structs/records → bare name; methods/constructors →
//! `.name()`; `using A.B.C` → `imports` edge to the tail; `base_list` bases are
//! classified `inherits` vs `implements` (pre-scanned interface names + the
//! `I`-prefix convention); parameter/return types → `references`.

#[cfg(feature = "lang-csharp")]
use crate::config::{HeritageStyle, ImportStyle, LanguageConfig, TypeRefStyle};
#[cfg(feature = "lang-csharp")]
use crate::result::ExtractionResult;
#[cfg(feature = "lang-csharp")]
use crate::walker::extract_with_config;

/// The C# `LanguageConfig`. Calls are `invocation_expression` whose `function`
/// is either an `identifier` or a `member_access_expression` (its `name` field
/// is the method) — handled by the generic accessor path.
#[cfg(feature = "lang-csharp")]
pub fn csharp_config() -> LanguageConfig {
    LanguageConfig {
        language: || tree_sitter_c_sharp::LANGUAGE.into(),
        class_types: &[
            "class_declaration",
            "interface_declaration",
            "struct_declaration",
            "record_declaration",
        ],
        function_types: &["method_declaration", "constructor_declaration"],
        call_types: &["invocation_expression"],
        name_field: "name",
        body_field: "body",
        call_function_field: "function",
        call_accessor_node_types: &["member_access_expression"],
        call_accessor_field: "name",
        function_boundary_types: &["method_declaration", "constructor_declaration"],
        superclasses_field: None,
        decorated_types: &[],
        builtins: &[],
        import_types: &["using_directive"],
        import_style: Some(ImportStyle::CSharp),
        type_ref_style: Some(TypeRefStyle::CSharp),
        heritage_style: Some(HeritageStyle::CSharp),
        constructor_call_type: None,
        body_kinds: &[],
    }
}

/// Extract a C# source file already in memory.
#[cfg(feature = "lang-csharp")]
pub fn extract_csharp_source(path: &str, source: &[u8]) -> ExtractionResult {
    extract_with_config(path, source, &csharp_config())
}

/// Read and extract a C# file from disk.
#[cfg(feature = "lang-csharp")]
pub fn extract_csharp_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_csharp_source(&path_str, &source))
}

#[cfg(all(test, feature = "lang-csharp"))]
mod tests {
    use super::extract_csharp_source;
    use crate::result::ExtractionResult;
    use synaptic_core::Confidence;

    const SAMPLE: &[u8] = br#"
using System;
using System.Collections.Generic;

namespace Demo {
    interface IGreeter {
        string Greet();
    }

    class Animal {
        public void Breathe() {}
    }

    class Dog : Animal, IGreeter {
        public string Greet() {
            return MakeSound();
        }

        string MakeSound() {
            return "woof";
        }

        void Feed(Food food) {
            Breathe();
        }
    }
}
"#;

    fn extract() -> ExtractionResult {
        extract_csharp_source("src/Demo/Sample.cs", SAMPLE)
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
        assert!(ls.contains(&"IGreeter".to_string()));
    }

    #[test]
    fn methods_scoped_under_class() {
        let r = extract();
        let ls = labels(&r);
        assert!(ls.contains(&".MakeSound()".to_string()), "{ls:?}");
        assert!(ls.contains(&".Breathe()".to_string()));
        assert!(ls.contains(&".Greet()".to_string()));
    }

    #[test]
    fn using_imports_emit_tail() {
        let r = extract();
        let imps = rels(&r, "imports");
        assert!(
            imps.iter().any(|(_, t)| t == "Generic"),
            "expected import tail Generic; got {imps:?}"
        );
        assert!(imps.iter().any(|(_, t)| t == "System"));
    }

    #[test]
    fn base_list_classified_inherits_vs_implements() {
        let r = extract();
        let inh = rels(&r, "inherits");
        let imp = rels(&r, "implements");
        // Dog : Animal, IGreeter -> Animal is a class (inherits), IGreeter is an
        // interface (pre-scan + I-prefix) (implements).
        assert!(
            inh.contains(&("Dog".to_string(), "Animal".to_string())),
            "inherits: {inh:?}"
        );
        assert!(
            imp.contains(&("Dog".to_string(), "IGreeter".to_string())),
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
        // Greet()/MakeSound() return string (predefined_type) -> NOT referenced.
        assert!(!refs.iter().any(|(t, _)| t == "string"));
    }

    #[test]
    fn method_attributes_referenced_as_attributes() {
        // `[TestMethod]` on a method gives a references edge (context "attribute").
        let r = extract_csharp_source(
            "T.cs",
            b"class T {\n  [TestMethod]\n  public void M() {}\n}\n",
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
        assert!(
            refs.contains(&("TestMethod".to_string(), "attribute".to_string())),
            "refs: {refs:?}"
        );
    }

    #[test]
    fn field_and_property_types_referenced() {
        let r = extract_csharp_source(
            "F.cs",
            b"class C {\n  private Leash leash;\n  public Tail Tail { get; set; }\n}\n",
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
        assert!(
            refs.contains(&("Leash".to_string(), "field".to_string())),
            "{refs:?}"
        );
        assert!(refs.contains(&("Tail".to_string(), "field".to_string())));
    }

    #[test]
    fn intra_file_calls_resolve() {
        let r = extract();
        let calls = rels(&r, "calls");
        assert!(
            calls.contains(&(".Greet()".to_string(), ".MakeSound()".to_string())),
            "calls: {calls:?}"
        );
        assert!(calls.contains(&(".Feed()".to_string(), ".Breathe()".to_string())));
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
