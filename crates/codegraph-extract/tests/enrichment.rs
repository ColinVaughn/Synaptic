//! Metadata enrichment: kind / visibility / span on code nodes.
//!
//! Each test is gated on the `lang-*` feature it exercises so the suite is green
//! under any single-language build (the `extract-langs` CI matrix), not just
//! `--all-features`. An ungated test would panic when its grammar is absent.

// A single-language build with no enrichment test for that language (e.g. json)
// compiles this file with every test gated out, leaving these imports unused.
#![allow(unused_imports)]

use codegraph_core::{NodeKind, Visibility};
use codegraph_extract::extract_source;

#[cfg(feature = "lang-python")]
#[test]
fn python_kind_visibility_span() {
    let src = b"class Foo:\n    def _bar(self):\n        return 1\n\ndef top():\n    return 2\n";
    let r = extract_source("m.py", src).expect("python extracts");

    let foo = r
        .nodes
        .iter()
        .find(|n| n.label == "Foo")
        .expect("class node");
    assert_eq!(foo.kind(), Some(NodeKind::Class));
    let span = foo.span().expect("class has a span");
    assert_eq!(span.start_line, 1);
    assert!(span.end_line >= 3, "multi-line class span: {span:?}");

    let bar = r
        .nodes
        .iter()
        .find(|n| n.label == "._bar()")
        .expect("method node");
    assert_eq!(bar.kind(), Some(NodeKind::Method));
    assert_eq!(bar.visibility(), Some(Visibility::Private)); // _name convention

    let top = r
        .nodes
        .iter()
        .find(|n| n.label == "top()")
        .expect("function node");
    assert_eq!(top.kind(), Some(NodeKind::Function));
}

#[cfg(feature = "lang-rust")]
#[test]
fn rust_kind_visibility_span() {
    let src = b"pub struct S {\n    x: i32,\n}\nimpl S {\n    pub fn go(&self) {}\n    fn hidden(&self) {}\n}\nfn free() {}\n";
    let r = extract_source("m.rs", src).expect("rust extracts");

    let s = r
        .nodes
        .iter()
        .find(|n| n.label == "S")
        .expect("struct node");
    assert_eq!(s.kind(), Some(NodeKind::Struct));
    assert_eq!(s.visibility(), Some(Visibility::Public));
    assert!(s.span().map(|sp| sp.end_line >= 3).unwrap_or(false));

    let go = r
        .nodes
        .iter()
        .find(|n| n.label == ".go()")
        .expect("method node");
    assert_eq!(go.kind(), Some(NodeKind::Method));
    assert_eq!(go.visibility(), Some(Visibility::Public));

    let hidden = r
        .nodes
        .iter()
        .find(|n| n.label == ".hidden()")
        .expect("private method");
    assert_eq!(hidden.visibility(), Some(Visibility::Private));

    let free = r
        .nodes
        .iter()
        .find(|n| n.label == "free()")
        .expect("free function");
    assert_eq!(free.kind(), Some(NodeKind::Function));
    assert_eq!(free.visibility(), Some(Visibility::Private)); // no `pub`
}

#[cfg(feature = "lang-go")]
#[test]
fn go_kind_visibility() {
    let src = b"package p\n\nfunc Exported() {}\n\nfunc internal() {}\n\ntype T struct{}\n";
    let r = extract_source("m.go", src).expect("go extracts");

    let exported = r
        .nodes
        .iter()
        .find(|n| n.label == "Exported()")
        .expect("func");
    assert_eq!(exported.kind(), Some(NodeKind::Function));
    assert_eq!(exported.visibility(), Some(Visibility::Public)); // uppercase initial

    let internal = r
        .nodes
        .iter()
        .find(|n| n.label == "internal()")
        .expect("func");
    assert_eq!(internal.visibility(), Some(Visibility::Private));

    let t = r.nodes.iter().find(|n| n.label == "T").expect("type");
    assert_eq!(t.kind(), Some(NodeKind::Struct));
    assert_eq!(t.visibility(), Some(Visibility::Public));
}

#[cfg(feature = "lang-java")]
#[test]
fn java_visibility_ignores_annotation_names() {
    // `@PublicApi private` must resolve to Private, not Public (annotation name
    // contains the substring "public").
    let src =
        b"public class Foo {\n  @PublicApi\n  private void bar() {}\n  public void baz() {}\n}\n";
    let r = extract_source("Foo.java", src).expect("java extracts");
    let foo = r.nodes.iter().find(|n| n.label == "Foo").expect("class");
    assert_eq!(foo.kind(), Some(NodeKind::Class));
    assert_eq!(foo.visibility(), Some(Visibility::Public));
    let bar = r
        .nodes
        .iter()
        .find(|n| n.label == ".bar()")
        .expect("private method");
    assert_eq!(bar.visibility(), Some(Visibility::Private));
    let baz = r
        .nodes
        .iter()
        .find(|n| n.label == ".baz()")
        .expect("public method");
    assert_eq!(baz.visibility(), Some(Visibility::Public));
}

#[cfg(feature = "lang-go")]
#[test]
fn go_type_enriched_even_when_method_precedes_declaration() {
    // Method appears before the type decl: the type node must still be enriched.
    let src = b"package p\n\nfunc (t *T) M() {}\n\ntype T struct{}\n";
    let r = extract_source("m.go", src).expect("go extracts");
    let t = r.nodes.iter().find(|n| n.label == "T").expect("type node");
    assert_eq!(t.kind(), Some(NodeKind::Struct), "stub upgraded in place");
}

#[cfg(feature = "lang-python")]
#[test]
fn raw_call_span_captured_by_generic_walker() {
    // The generic walker (Python here) records a column-accurate call span.
    let src = b"def a():\n    return helper()\n";
    let r = extract_source("m.py", src).expect("python extracts");
    let call = r
        .raw_calls
        .iter()
        .find(|c| c.callee == "helper")
        .expect("unresolved call recorded");
    assert!(call.span.is_some(), "generic walker captures call span");
}
