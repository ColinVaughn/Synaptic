//! JavaScript / TypeScript / TSX extraction. These languages fit the generic
//! tree-sitter walker directly via per-variant `LanguageConfig`s.
//!
//! Scope: file/class/function/method structure, intra-file calls (+ member
//! calls via `member_expression.property`), `extends`/`implements` heritage
//! (`inherits`/`implements`), `new` constructor calls, `import`/`export … from`
//! edges (+ named-import records for cross-file resolution), and TS parameter/
//! return type references. Relative imports are emitted as specifier stubs here
//! and then bound to their real file nodes by the cross-file
//! [`crate::resolve::resolve_relative_imports`] pass (which has the full file
//! set). The only piece still deferred is tsconfig path-alias resolution
//! (`@/x`), which needs reading `tsconfig.json` (project config).

use crate::config::{HeritageStyle, ImportStyle, LanguageConfig, TypeRefStyle};
use crate::result::ExtractionResult;
use crate::walker::extract_with_config;

/// Global callables skipped as call targets. Only *bare* globals are listed —
/// member-method names (`log`, `parse`, …) are intentionally excluded so a
/// user method of the same name is not suppressed.
pub const ECMASCRIPT_BUILTINS: &[&str] = &[
    "parseInt",
    "parseFloat",
    "isNaN",
    "isFinite",
    "require",
    "setTimeout",
    "setInterval",
    "clearTimeout",
    "clearInterval",
    "fetch",
    "encodeURIComponent",
    "decodeURIComponent",
    "encodeURI",
    "decodeURI",
    "btoa",
    "atob",
    "structuredClone",
    "queueMicrotask",
    "String",
    "Number",
    "Boolean",
    "Array",
    "Object",
    "Symbol",
    "BigInt",
];

/// Node types common to JS and TS: function declarations + class/object methods.
const FUNCTION_TYPES: &[&str] = &["function_declaration", "method_definition"];
const FUNCTION_BOUNDARY_TYPES: &[&str] = &[
    "function_declaration",
    "arrow_function",
    "method_definition",
];

/// The JavaScript `LanguageConfig`.
#[cfg(feature = "lang-javascript")]
pub fn js_config() -> LanguageConfig {
    LanguageConfig {
        language: || tree_sitter_javascript::LANGUAGE.into(),
        class_types: &["class_declaration"],
        function_types: FUNCTION_TYPES,
        call_types: &["call_expression"],
        name_field: "name",
        body_field: "body",
        call_function_field: "function",
        call_accessor_node_types: &["member_expression"],
        call_accessor_field: "property",
        function_boundary_types: FUNCTION_BOUNDARY_TYPES,
        superclasses_field: None,
        decorated_types: &[],
        builtins: ECMASCRIPT_BUILTINS,
        import_types: &["import_statement"],
        import_style: Some(ImportStyle::EcmaScript),
        type_ref_style: None,
        heritage_style: Some(HeritageStyle::EcmaScript),
        constructor_call_type: Some("new_expression"),
        body_kinds: &[],
    }
}

/// TS treats interfaces / enums / type-aliases / abstract classes as "classes"
/// (named container nodes).
#[cfg(feature = "lang-typescript")]
const TS_CLASS_TYPES: &[&str] = &[
    "class_declaration",
    "abstract_class_declaration",
    "interface_declaration",
    "enum_declaration",
    "type_alias_declaration",
];

/// The TypeScript `LanguageConfig`.
#[cfg(feature = "lang-typescript")]
pub fn ts_config() -> LanguageConfig {
    LanguageConfig {
        language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        class_types: TS_CLASS_TYPES,
        function_types: FUNCTION_TYPES,
        call_types: &["call_expression"],
        name_field: "name",
        body_field: "body",
        call_function_field: "function",
        call_accessor_node_types: &["member_expression"],
        call_accessor_field: "property",
        function_boundary_types: FUNCTION_BOUNDARY_TYPES,
        superclasses_field: None,
        decorated_types: &[],
        builtins: ECMASCRIPT_BUILTINS,
        import_types: &["import_statement"],
        import_style: Some(ImportStyle::EcmaScript),
        type_ref_style: Some(TypeRefStyle::EcmaScript),
        heritage_style: Some(HeritageStyle::EcmaScript),
        constructor_call_type: Some("new_expression"),
        body_kinds: &[],
    }
}

/// The TSX `LanguageConfig` — TS grammar variant that also parses JSX
/// (uses `language_tsx`, not `language_typescript`).
#[cfg(feature = "lang-typescript")]
pub fn tsx_config() -> LanguageConfig {
    LanguageConfig {
        language: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        ..ts_config()
    }
}

/// Extract in-memory JavaScript (`.js`/`.jsx`/`.mjs`).
#[cfg(feature = "lang-javascript")]
pub fn extract_js_source(path: &str, source: &[u8]) -> ExtractionResult {
    extract_with_config(path, source, &js_config())
}

/// Extract in-memory TypeScript (`.ts`).
#[cfg(feature = "lang-typescript")]
pub fn extract_ts_source(path: &str, source: &[u8]) -> ExtractionResult {
    extract_with_config(path, source, &ts_config())
}

/// Extract in-memory TSX (`.tsx`).
#[cfg(feature = "lang-typescript")]
pub fn extract_tsx_source(path: &str, source: &[u8]) -> ExtractionResult {
    extract_with_config(path, source, &tsx_config())
}

#[cfg(test)]
mod tests {
    use crate::result::ExtractionResult;

    fn labels(r: &ExtractionResult) -> Vec<String> {
        r.nodes.iter().map(|n| n.label.clone()).collect()
    }

    fn call_pairs(r: &ExtractionResult) -> std::collections::HashSet<(String, String)> {
        let label = |id: &codegraph_core::NodeId| {
            r.nodes
                .iter()
                .find(|n| &n.id == id)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| id.0.clone())
        };
        r.edges
            .iter()
            .filter(|e| e.relation == "calls")
            .map(|e| (label(&e.source), label(&e.target)))
            .collect()
    }

    /// Labels of nodes targeted by `imports_from` edges (import specifiers).
    fn import_targets(r: &ExtractionResult) -> std::collections::HashSet<String> {
        let label: std::collections::HashMap<&str, &str> = r
            .nodes
            .iter()
            .map(|n| (n.id.0.as_str(), n.label.as_str()))
            .collect();
        r.edges
            .iter()
            .filter(|e| e.relation == "imports_from")
            .filter_map(|e| label.get(e.target.0.as_str()).map(|s| s.to_string()))
            .collect()
    }

    #[cfg(feature = "lang-javascript")]
    #[test]
    fn js_captures_dynamic_import_forms() {
        let src = br#"
            const a = import('mod-a');
            const b = require('mod-b');
            registerApplication({ app: () => System.import('@scope/App') });
        "#;
        let r = super::extract_js_source("d.js", src);
        let t = import_targets(&r);
        assert!(t.contains("mod-a"), "native dynamic import: {t:?}");
        assert!(t.contains("mod-b"), "require: {t:?}");
        assert!(t.contains("@scope/App"), "System.import: {t:?}");
    }

    #[cfg(feature = "lang-javascript")]
    #[test]
    fn js_skips_computed_and_non_system_dynamic_imports() {
        let src = br#"
            const x = 'm';
            const a = import(x);
            const b = import(`a/${x}`);
            const c = obj.import('not-an-import');
        "#;
        let r = super::extract_js_source("d.js", src);
        let t = import_targets(&r);
        assert!(
            !t.iter()
                .any(|s| s == "m" || s.contains("${") || s == "not-an-import"),
            "{t:?}"
        );
    }

    #[cfg(feature = "lang-javascript")]
    const JS: &[u8] = b"class Widget {\n  render() {\n    return draw(this.size);\n  }\n}\n\nfunction draw(s) {\n  return s;\n}\n\nfunction main() {\n  const w = new Widget();\n  w.render();\n  draw(3);\n}\n";

    #[cfg(feature = "lang-javascript")]
    #[test]
    fn js_extracts_class_method_function_and_calls() {
        let r = super::extract_js_source("app.js", JS);
        let ls = labels(&r);
        assert!(ls.contains(&"app.js".to_string()));
        assert!(ls.contains(&"Widget".to_string()));
        assert!(ls.contains(&".render()".to_string()));
        assert!(ls.contains(&"draw()".to_string()));
        assert!(ls.contains(&"main()".to_string()));
        let pairs = call_pairs(&r);
        // render() calls module fn draw(); main() calls draw() and the method render().
        assert!(
            pairs.contains(&(".render()".into(), "draw()".into())),
            "{pairs:?}"
        );
        assert!(pairs.contains(&("main()".into(), "draw()".into())));
        assert!(pairs.contains(&("main()".into(), ".render()".into())));
    }

    fn rel_pairs(
        r: &ExtractionResult,
        relation: &str,
    ) -> std::collections::HashSet<(String, String)> {
        let label = |id: &codegraph_core::NodeId| {
            r.nodes
                .iter()
                .find(|n| &n.id == id)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| id.0.clone())
        };
        r.edges
            .iter()
            .filter(|e| e.relation == relation)
            .map(|e| (label(&e.source), label(&e.target)))
            .collect()
    }

    #[cfg(feature = "lang-javascript")]
    #[test]
    fn js_extends_creates_inherits_edge() {
        let r = super::extract_js_source("a.js", b"class A {}\nclass B extends A {}\n");
        assert!(
            rel_pairs(&r, "inherits").contains(&("B".into(), "A".into())),
            "edges: {:?}",
            r.edges
                .iter()
                .map(|e| (e.source.0.clone(), e.relation.clone(), e.target.0.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "lang-javascript")]
    #[test]
    fn js_structural_edges_are_extracted() {
        use codegraph_core::Confidence;
        let r = super::extract_js_source("app.js", JS);
        // file -> class contains, class -> method method.
        let rels: Vec<&str> = r.edges.iter().map(|e| e.relation.as_str()).collect();
        assert!(rels.contains(&"contains"));
        assert!(rels.contains(&"method"));
        assert!(r
            .edges
            .iter()
            .filter(|e| matches!(e.relation.as_str(), "contains" | "method"))
            .all(|e| e.confidence == Confidence::Extracted));
    }

    #[cfg(feature = "lang-typescript")]
    const TS: &[u8] = b"interface Shape {\n  area(): number;\n}\n\nclass Circle implements Shape {\n  area(): number {\n    return compute();\n  }\n}\n\nfunction compute(): number {\n  return 1;\n}\n";

    #[cfg(feature = "lang-javascript")]
    #[test]
    fn js_new_creates_constructor_call() {
        let r = super::extract_js_source(
            "a.js",
            b"class Widget {}\nfunction main() {\n  const w = new Widget();\n}\n",
        );
        assert!(
            call_pairs(&r).contains(&("main()".into(), "Widget".into())),
            "calls: {:?}",
            call_pairs(&r)
        );
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn ts_implements_creates_implements_edge() {
        let r = super::extract_ts_source(
            "a.ts",
            b"interface I {}\nclass C implements I {\n  m(): void {}\n}\n",
        );
        assert!(
            rel_pairs(&r, "implements").contains(&("C".into(), "I".into())),
            "edges: {:?}",
            r.edges
                .iter()
                .map(|e| (e.source.0.clone(), e.relation.clone(), e.target.0.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn ts_interface_extends_creates_inherits_edge() {
        let r = super::extract_ts_source("a.ts", b"interface Y {}\ninterface X extends Y {}\n");
        assert!(
            rel_pairs(&r, "inherits").contains(&("X".into(), "Y".into())),
            "edges: {:?}",
            r.edges
                .iter()
                .map(|e| (e.source.0.clone(), e.relation.clone(), e.target.0.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn ts_interface_extends_generic_base_creates_inherits_edge() {
        // `extends Base<T>`: the base is a `generic_type`; its head must still inherit.
        let r = super::extract_ts_source(
            "a.ts",
            b"interface Y<T> {}\ninterface X extends Y<string> {}\n",
        );
        assert!(
            rel_pairs(&r, "inherits").contains(&("X".into(), "Y".into())),
            "edges: {:?}",
            r.edges
                .iter()
                .map(|e| (e.source.0.clone(), e.relation.clone(), e.target.0.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "lang-javascript")]
    #[test]
    fn js_named_import_creates_edge_and_records() {
        let r = super::extract_js_source("a.js", b"import { foo, bar as baz } from './util';\n");
        assert!(
            r.edges.iter().any(|e| e.relation == "imports_from"),
            "edges: {:?}",
            r.edges
                .iter()
                .map(|e| (e.source.0.clone(), e.relation.clone(), e.target.0.clone()))
                .collect::<Vec<_>>()
        );
        assert!(
            r.imports.iter().any(|i| i.imported_name == "foo"
                && i.local_name == "foo"
                && i.module_stem == "util"),
            "records: {:?}",
            r.imports
        );
        assert!(r
            .imports
            .iter()
            .any(|i| i.imported_name == "bar" && i.local_name == "baz" && i.module_stem == "util"));
        // The imports_from edge is tagged with the imported symbol names (original
        // names, not aliases) so forecast-time impact can resolve module importers.
        let edge = r
            .edges
            .iter()
            .find(|e| e.relation == "imports_from")
            .expect("imports_from edge");
        let names: Vec<&str> = edge
            .extra
            .get("imported")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        assert!(
            names.contains(&"foo") && names.contains(&"bar"),
            "edge imported tag: {names:?}"
        );
    }

    #[cfg(feature = "lang-javascript")]
    #[test]
    fn js_bare_import_creates_module_stub_edge() {
        let r = super::extract_js_source("a.js", b"import React from 'react';\n");
        assert!(r.edges.iter().any(|e| e.relation == "imports_from"
            && r.nodes
                .iter()
                .any(|n| n.id == e.target && n.label == "react")));
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn ts_reexport_creates_re_exports_edge_and_keeps_decl() {
        let r =
            super::extract_ts_source("idx.ts", b"export { A } from './a';\nexport class B {}\n");
        assert!(
            r.edges.iter().any(|e| e.relation == "re_exports"),
            "edges: {:?}",
            r.edges
                .iter()
                .map(|e| (e.source.0.clone(), e.relation.clone(), e.target.0.clone()))
                .collect::<Vec<_>>()
        );
        // The exported declaration is still extracted by the structural pass.
        assert!(
            labels(&r).contains(&"B".to_string()),
            "labels: {:?}",
            labels(&r)
        );
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn ts_treats_interface_as_class_and_links_calls() {
        let r = super::extract_ts_source("shapes.ts", TS);
        let ls = labels(&r);
        assert!(
            ls.contains(&"Shape".to_string()),
            "interface node; got {ls:?}"
        );
        assert!(ls.contains(&"Circle".to_string()));
        assert!(ls.contains(&".area()".to_string()));
        assert!(ls.contains(&"compute()".to_string()));
        assert!(call_pairs(&r).contains(&(".area()".into(), "compute()".into())));
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn ts_param_and_return_types_create_references() {
        let r = super::extract_ts_source(
            "a.ts",
            b"class Foo {}\nclass Bar {}\nfunction f(x: Foo): Bar {\n  return new Bar();\n}\n",
        );
        let refs = rel_pairs(&r, "references");
        assert!(
            refs.contains(&("f()".into(), "Foo".into())),
            "refs: {refs:?}"
        );
        assert!(
            refs.contains(&("f()".into(), "Bar".into())),
            "refs: {refs:?}"
        );
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn ts_primitive_types_are_not_referenced() {
        let r =
            super::extract_ts_source("a.ts", b"function g(s: string): number {\n  return 0;\n}\n");
        assert!(
            rel_pairs(&r, "references").is_empty(),
            "refs: {:?}",
            rel_pairs(&r, "references")
        );
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn tsx_parses_jsx_returning_components() {
        // A component returning JSX must parse (language_tsx, not language_typescript).
        let src =
            b"function App() {\n  return helper();\n}\n\nfunction helper() {\n  return 1;\n}\n";
        let r = super::extract_tsx_source("App.tsx", src);
        let ls = labels(&r);
        assert!(ls.contains(&"App()".to_string()));
        assert!(ls.contains(&"helper()".to_string()));
        assert!(call_pairs(&r).contains(&("App()".into(), "helper()".into())));
    }
}
