//! Groovy extractor — Bucket A. Groovy's tree-sitter grammar uses the same node
//! vocabulary as Java (`class_declaration`, `method_declaration`,
//! `method_invocation`, `superclass`/`super_interfaces`, `import_declaration`),
//! so it reuses the Java `ImportStyle`/`HeritageStyle`/`TypeRefStyle` machinery
//! directly — only the grammar differs.

#[cfg(feature = "lang-groovy")]
use crate::config::{HeritageStyle, ImportStyle, LanguageConfig, TypeRefStyle};
#[cfg(feature = "lang-groovy")]
use crate::result::ExtractionResult;
#[cfg(feature = "lang-groovy")]
use crate::walker::extract_with_config;

/// The Groovy `LanguageConfig` (Java-shaped grammar).
#[cfg(feature = "lang-groovy")]
pub fn groovy_config() -> LanguageConfig {
    LanguageConfig {
        language: || tree_sitter_groovy::LANGUAGE.into(),
        class_types: &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
        ],
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

/// Extract a Groovy source file already in memory.
#[cfg(feature = "lang-groovy")]
pub fn extract_groovy_source(path: &str, source: &[u8]) -> ExtractionResult {
    extract_with_config(path, source, &groovy_config())
}

/// Read and extract a Groovy file from disk.
#[cfg(feature = "lang-groovy")]
pub fn extract_groovy_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_groovy_source(&path_str, &source))
}

#[cfg(all(test, feature = "lang-groovy"))]
mod tests {
    use super::extract_groovy_source;
    use crate::result::ExtractionResult;

    const SAMPLE: &[u8] = b"package p\nimport a.B\n\nclass Dog extends Animal implements Greeter {\n  String bark() { return sound() }\n  String sound() { return 'woof' }\n}\n";

    fn extract() -> ExtractionResult {
        extract_groovy_source("src/Dog.groovy", SAMPLE)
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
    fn import_extends_implements() {
        let r = extract();
        assert!(rels(&r, "imports").iter().any(|(_, t)| t == "B"));
        assert!(rels(&r, "inherits").contains(&("Dog".to_string(), "Animal".to_string())));
        assert!(rels(&r, "implements").contains(&("Dog".to_string(), "Greeter".to_string())));
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
