//! C extractor — Bucket B (declarative `LanguageConfig` + the walker's
//! declarator-unwrap function-name fallback). Covers the C config, imports, and
//! type-reference collection.
//!
//! Functions → `name()` (name unwrapped from the declarator chain); `#include`
//! → `imports_from` edge to the header base name; parameter/return types →
//! `references`. C has no classes/heritage.

#[cfg(feature = "lang-c")]
use crate::config::{ImportStyle, LanguageConfig, TypeRefStyle};
#[cfg(feature = "lang-c")]
use crate::result::ExtractionResult;
#[cfg(feature = "lang-c")]
use crate::walker::{extract_with_config, normalize_c_family_source};

/// The C `LanguageConfig`. `function_definition` has no `name` field — the
/// walker's `function_name_node` fallback unwraps the declarator chain. Calls
/// are `call_expression` whose `function` field names the callee.
#[cfg(feature = "lang-c")]
pub fn c_config() -> LanguageConfig {
    LanguageConfig {
        language: || tree_sitter_c::LANGUAGE.into(),
        class_types: &[],
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
        heritage_style: None,
        constructor_call_type: None,
        body_kinds: &[],
    }
}

/// Extract a C source file already in memory.
#[cfg(feature = "lang-c")]
pub fn extract_c_source(path: &str, source: &[u8]) -> ExtractionResult {
    let source = normalize_c_family_source(source, true);
    extract_with_config(path, &source, &c_config())
}

/// Read and extract a C file from disk.
#[cfg(feature = "lang-c")]
pub fn extract_c_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_c_source(&path_str, &source))
}

#[cfg(all(test, feature = "lang-c"))]
mod tests {
    use super::extract_c_source;
    use crate::result::ExtractionResult;
    use synaptic_core::Confidence;

    const SAMPLE: &[u8] = br#"
#include <stdio.h>
#include "util.h"

int helper(int x) { return x; }

int run(struct Config *cfg) {
    return helper(cfg->n);
}
"#;

    fn extract() -> ExtractionResult {
        extract_c_source("src/app.c", SAMPLE)
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
    fn functions_via_declarator_unwrap() {
        let r = extract();
        let ls = labels(&r);
        assert!(ls.contains(&"helper()".to_string()), "{ls:?}");
        assert!(ls.contains(&"run()".to_string()));
    }

    #[test]
    fn includes_emit_header_base_name() {
        let r = extract();
        let imps = rels(&r, "imports_from");
        assert!(
            imps.iter().any(|(_, t)| t == "stdio"),
            "expected header base stdio; got {imps:?}"
        );
        assert!(imps.iter().any(|(_, t)| t == "util"));
    }

    #[test]
    fn struct_parameter_type_referenced() {
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
        // run(struct Config *cfg) -> Config parameter_type; primitive `int` skipped.
        assert!(
            refs.contains(&("Config".to_string(), "parameter_type".to_string())),
            "refs: {refs:?}"
        );
        assert!(!refs.iter().any(|(t, _)| t == "int"));
    }

    #[test]
    fn intra_file_call_resolves() {
        let r = extract();
        let calls = rels(&r, "calls");
        assert!(
            calls.contains(&("run()".to_string(), "helper()".to_string())),
            "calls: {calls:?}"
        );
    }

    #[test]
    fn structural_edges_extracted_confidence() {
        let r = extract();
        for e in &r.edges {
            if matches!(e.relation.as_str(), "contains" | "calls") {
                assert_eq!(e.confidence, Confidence::Extracted, "edge {e:?}");
            }
        }
    }

    #[test]
    fn declaration_macros_do_not_hide_functions() {
        let r = extract_c_source(
            "zlib.c",
            br#"
int ZEXPORT inflate(void FAR *stream, int flush) { return flush; }
const char * ZEXPORT zlibVersion(void) { return "1.0"; }
void ZLIB_INTERNAL helper(void) {}
"#,
        );
        let ls = labels(&r);
        assert!(ls.contains(&"inflate()".to_string()), "{ls:?}");
        assert!(ls.contains(&"zlibVersion()".to_string()), "{ls:?}");
        assert!(ls.contains(&"helper()".to_string()), "{ls:?}");
        assert_eq!(
            r.nodes
                .iter()
                .find(|node| node.label == "inflate()")
                .unwrap()
                .signature()
                .unwrap()
                .params
                .len(),
            2
        );
    }

    #[test]
    fn trailing_underscore_functions_do_not_collide() {
        let r = extract_c_source(
            "api.c",
            b"int get(void) { return 1; }\nint get_(void) { return get(); }\n",
        );
        let functions: Vec<_> = r
            .nodes
            .iter()
            .filter(|node| matches!(node.label.as_str(), "get()" | "get_()"))
            .collect();
        assert_eq!(functions.len(), 2);
        assert_ne!(functions[0].id, functions[1].id);
    }

    #[test]
    fn conditionals_inside_function_bodies_do_not_hide_definition() {
        let r = extract_c_source(
            "deflate.c",
            br#"
int ZEXPORT reset(void *stream) {
  int state;
  state =
#ifdef FEATURE
      stream ? 1 :
#endif
      0;
  return state;
}
#if NEVER
Call UPDATE_HASH()
#endif
"#,
        );
        let ls = labels(&r);
        assert!(ls.contains(&"reset()".to_string()));
        assert!(!ls.contains(&"UPDATE_HASH()".to_string()));
    }
}
