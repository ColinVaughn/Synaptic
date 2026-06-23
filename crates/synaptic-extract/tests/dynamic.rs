//! Reflection / dynamic-dispatch SITE detection (gated on `cross-language`). These
//! record `DynamicSite`s on the enclosing node and emit no edges; the query layer
//! turns them into the "0 dependents is not proof of safety" caveat.

#![cfg(feature = "cross-language")]
#![allow(unused_imports)]

use synaptic_core::{DynamicKind, DynamicSite};
use synaptic_extract::extract_source;

/// All dynamic sites recorded anywhere in the extraction result.
#[allow(dead_code)]
fn sites(path: &str, src: &[u8]) -> Vec<DynamicSite> {
    let r = extract_source(path, src).expect("extracts");
    r.nodes.iter().flat_map(|n| n.dynamic_sites()).collect()
}

#[cfg(feature = "lang-typescript")]
#[test]
fn js_literal_computed_call_yields_keyed_reflection_site() {
    let s = sites("a.ts", b"function f(){ return handlers['doThing'](x); }\n");
    assert!(
        s.iter()
            .any(|s| s.kind == DynamicKind::Reflection && s.key.as_deref() == Some("doThing")),
        "{s:?}"
    );
}

#[cfg(feature = "lang-typescript")]
#[test]
fn js_variable_computed_call_is_opaque() {
    let s = sites("a.ts", b"function f(k){ return handlers[k](x); }\n");
    assert!(
        s.iter()
            .any(|s| s.kind == DynamicKind::Reflection && s.key.is_none()),
        "{s:?}"
    );
}

#[cfg(feature = "lang-typescript")]
#[test]
fn js_eval_and_dynamic_import() {
    let s = sites("a.ts", b"function f(p){ eval(p); return import(p); }\n");
    assert!(s.iter().any(|s| s.kind == DynamicKind::Eval), "{s:?}");
    assert!(
        s.iter().any(|s| s.kind == DynamicKind::DynamicImport),
        "{s:?}"
    );
}

#[cfg(feature = "lang-typescript")]
#[test]
fn js_literal_dynamic_import_is_not_flagged() {
    // A static `import('./x')` specifier is resolvable; only non-literal flagged.
    let s = sites("a.ts", b"function f(){ return import('./mod'); }\n");
    assert!(
        !s.iter().any(|s| s.kind == DynamicKind::DynamicImport),
        "literal specifier must not be a dynamic-import hazard: {s:?}"
    );
}

#[cfg(feature = "lang-typescript")]
#[test]
fn module_scope_eval_lands_on_a_file_node() {
    // No enclosing function -> the site attaches to the file node, so it is still
    // cataloged.
    let s = sites("top.ts", b"const x = eval(userInput);\n");
    assert!(s.iter().any(|s| s.kind == DynamicKind::Eval), "{s:?}");
}

#[cfg(feature = "lang-csharp")]
#[test]
fn dotnet_getmethod_literal_is_keyed() {
    let s = sites(
        "a.cs",
        b"class C { void f(){ t.GetMethod(\"Run\"); Activator.CreateInstance(ty); } }\n",
    );
    assert!(
        s.iter()
            .any(|s| s.kind == DynamicKind::Reflection && s.key.as_deref() == Some("Run")),
        "{s:?}"
    );
    assert!(
        s.iter()
            .any(|s| s.kind == DynamicKind::Reflection && s.key.is_none()),
        "Activator.CreateInstance is an opaque reflection site: {s:?}"
    );
}

#[cfg(feature = "lang-python")]
#[test]
fn python_getattr_literal_is_keyed() {
    let s = sites("a.py", b"def f(o):\n    return getattr(o, 'run')\n");
    assert!(
        s.iter()
            .any(|s| s.kind == DynamicKind::Reflection && s.key.as_deref() == Some("run")),
        "{s:?}"
    );
}

#[cfg(feature = "lang-python")]
#[test]
fn python_importlib_is_opaque_dynamic_import() {
    let s = sites(
        "a.py",
        b"def f(n):\n    return importlib.import_module(n)\n",
    );
    assert!(
        s.iter()
            .any(|s| s.kind == DynamicKind::DynamicImport && s.key.is_none()),
        "{s:?}"
    );
}

#[cfg(feature = "lang-java")]
#[test]
fn jvm_forname_and_getmethod_literals() {
    let s = sites(
        "A.java",
        b"class A { void f(){ Class.forName(\"com.x.Y\"); c.getMethod(\"run\"); } }\n",
    );
    assert!(
        s.iter()
            .any(|s| s.kind == DynamicKind::Reflection && s.key.as_deref() == Some("com.x.Y")),
        "{s:?}"
    );
    assert!(
        s.iter()
            .any(|s| s.kind == DynamicKind::Reflection && s.key.as_deref() == Some("run")),
        "{s:?}"
    );
}
