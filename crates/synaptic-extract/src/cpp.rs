//! C++ extractor — Bucket B (declarative `LanguageConfig` + the walker's
//! declarator-unwrap function-name fallback). Covers the C++ config, imports,
//! and type-reference collection.
//!
//! Classes/structs → bare name; inline-defined methods/free functions → `name()`
//! / `.name()` (name unwrapped from the declarator chain); `#include` →
//! `imports_from`; `base_class_clause` → `inherits`; parameter/return types →
//! `references`.

#[cfg(feature = "lang-cpp")]
use crate::config::{HeritageStyle, ImportStyle, LanguageConfig, TypeRefStyle};
#[cfg(feature = "lang-cpp")]
use crate::result::ExtractionResult;
#[cfg(feature = "lang-cpp")]
use crate::walker::extract_with_config;

/// The C++ `LanguageConfig`. `class_specifier`/`struct_specifier` carry `name`
/// and `body` fields; inline methods are `function_definition` (their name is
/// unwrapped from the declarator). Method prototypes (`field_declaration` with a
/// `function_declarator`) become method nodes and data members become `field`
/// type references — handled by the walker's `class_members` pass.
#[cfg(feature = "lang-cpp")]
pub fn cpp_config() -> LanguageConfig {
    LanguageConfig {
        language: || tree_sitter_cpp::LANGUAGE.into(),
        class_types: &["class_specifier", "struct_specifier"],
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
        import_types: &["preproc_include"],
        import_style: Some(ImportStyle::CInclude),
        type_ref_style: Some(TypeRefStyle::Cpp),
        heritage_style: Some(HeritageStyle::Cpp),
        constructor_call_type: None,
        body_kinds: &[],
    }
}

/// Extract a C++ source file already in memory.
#[cfg(feature = "lang-cpp")]
pub fn extract_cpp_source(path: &str, source: &[u8]) -> ExtractionResult {
    extract_with_config(path, source, &cpp_config())
}

/// Read and extract a C++ file from disk.
#[cfg(feature = "lang-cpp")]
pub fn extract_cpp_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_cpp_source(&path_str, &source))
}

#[cfg(all(test, feature = "lang-cpp"))]
mod tests {
    use super::extract_cpp_source;
    use crate::result::ExtractionResult;
    use synaptic_core::Confidence;

    const SAMPLE: &[u8] = br#"
#include <vector>

class Animal {
public:
    void breathe() { idle(); }
    void idle() {}
};

class Dog : public Animal {
public:
    Result greet(Food food) { return makeSound(); }
    Result makeSound() { return Result(); }
};
"#;

    fn extract() -> ExtractionResult {
        extract_cpp_source("src/app.cpp", SAMPLE)
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
        let r = extract();
        let ls = labels(&r);
        assert!(ls.contains(&"Animal".to_string()), "{ls:?}");
        assert!(ls.contains(&"Dog".to_string()));
        assert!(ls.contains(&".greet()".to_string()));
        assert!(ls.contains(&".makeSound()".to_string()));
    }

    #[test]
    fn include_emits_header_base() {
        let r = extract();
        let imps = rels(&r, "imports_from");
        assert!(imps.iter().any(|(_, t)| t == "vector"), "imports: {imps:?}");
    }

    #[test]
    fn base_class_clause_inherits() {
        let r = extract();
        let inh = rels(&r, "inherits");
        assert!(
            inh.contains(&("Dog".to_string(), "Animal".to_string())),
            "inherits: {inh:?}"
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
        assert!(refs.contains(&("Result".to_string(), "return_type".to_string())));
    }

    #[test]
    fn intra_class_call_resolves() {
        let r = extract();
        let calls = rels(&r, "calls");
        // greet() calls makeSound(); breathe() calls idle()
        assert!(
            calls.contains(&(".greet()".to_string(), ".makeSound()".to_string())),
            "calls: {calls:?}"
        );
        assert!(calls.contains(&(".breathe()".to_string(), ".idle()".to_string())));
    }

    #[test]
    fn method_prototypes_and_data_members() {
        let r = extract_cpp_source(
            "F.cpp",
            b"class C {\n  Leash leash;\n  void walk(Dog d);\n  Result fetch();\n};\n",
        );
        let labels: Vec<_> = r.nodes.iter().map(|n| n.label.clone()).collect();
        // prototypes become method nodes
        assert!(labels.contains(&".walk()".to_string()), "{labels:?}");
        assert!(labels.contains(&".fetch()".to_string()));
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
        // data member type + prototype param/return
        assert!(
            refs.contains(&("Leash".to_string(), "field".to_string())),
            "{refs:?}"
        );
        assert!(refs.contains(&("Dog".to_string(), "parameter_type".to_string())));
        assert!(refs.contains(&("Result".to_string(), "return_type".to_string())));
    }

    #[test]
    fn template_class_inheritance_and_members() {
        // A class template that inherits from a templated base, with members
        // whose types are the template parameter `T`.
        let r = extract_cpp_source(
            "tpl.cpp",
            br#"
template <typename T>
class Container {
public:
    T get() { return value; }
    void set(T v) { value = v; }
private:
    T value;
};

template <typename T>
class Stack : public Container<T> {
public:
    void push(T v) { this->set(v); }
};

class IntStack : public Stack<int> {
public:
    int top() { return this->get(); }
};
"#,
        );
        // The class template and its specializing subclass are real nodes.
        let ls = labels(&r);
        assert!(ls.contains(&"Container".to_string()), "{ls:?}");
        assert!(ls.contains(&"Stack".to_string()), "{ls:?}");
        assert!(ls.contains(&"IntStack".to_string()), "{ls:?}");

        // Inheritance follows through the templated base.
        let inh = rels(&r, "inherits");
        assert!(
            inh.contains(&("Stack".to_string(), "Container".to_string())),
            "{inh:?}"
        );
        assert!(
            inh.contains(&("IntStack".to_string(), "Stack".to_string())),
            "{inh:?}"
        );

        // The template parameter `T` is a placeholder, not a type: it must not
        // become a node nor a `references`/`inherits` target.
        assert!(
            !ls.contains(&"T".to_string()),
            "spurious template-param node T: {ls:?}"
        );
        let refs_to_t = r.edges.iter().any(|e| {
            r.nodes
                .iter()
                .find(|n| n.id == e.target)
                .is_some_and(|n| n.label == "T")
        });
        assert!(!refs_to_t, "edges should not target template parameter T");
    }

    #[test]
    fn structural_edges_extracted_confidence() {
        let r = extract();
        for e in &r.edges {
            if matches!(e.relation.as_str(), "contains" | "method" | "inherits") {
                assert_eq!(e.confidence, Confidence::Extracted, "edge {e:?}");
            }
        }
    }
}
