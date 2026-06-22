//! Call attribution through anonymous callbacks: a call inside an inline arrow /
//! function expression belongs to the enclosing named function, while a named
//! nested function keeps its own calls. Exercised via the real `extract_source`.
#![cfg(feature = "lang-javascript")]

use synaptic_extract::extract_source;

#[test]
fn calls_inside_anonymous_callbacks_attribute_to_the_enclosing_fn() {
    // `register()` passes an inline arrow to `.handle(...)` whose body calls
    // `doWork()`. The arrow is anonymous (no node of its own), so its call must be
    // attributed to `register` -- otherwise `doWork` looks like a 0-caller node
    // (the Electron `ipcMain.handle('ch', () => helper())` shape).
    let src = b"function doWork() { return 1; }\nfunction register(bus) {\n  bus.handle('evt', async () => { return doWork(); });\n}\n";
    let r = extract_source("h.js", src).unwrap();
    let register = r.nodes.iter().find(|n| n.label == "register()").unwrap();
    let dowork = r.nodes.iter().find(|n| n.label == "doWork()").unwrap();
    assert!(
        r.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == register.id && e.target == dowork.id),
        "the arrow's call to doWork must attribute to register"
    );
}

#[test]
fn calls_in_nested_named_functions_attribute_to_the_enclosing_named_fn() {
    // The generic walker does not create a node for a function nested inside a
    // function body, so its calls would otherwise be lost. They attribute to the
    // enclosing named fn instead (recovering an otherwise-0-caller helper). The
    // `owned_fn_nodes` guard means that if the nested function ever DID get its own
    // node, its calls would stay on it rather than double-attributing here.
    let src = b"function helper() { return 1; }\nfunction outer() {\n  function inner() { return helper(); }\n  return inner();\n}\n";
    let r = extract_source("n.js", src).unwrap();
    let outer = r.nodes.iter().find(|n| n.label == "outer()").unwrap();
    let helper = r.nodes.iter().find(|n| n.label == "helper()").unwrap();
    assert!(
        r.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == outer.id && e.target == helper.id),
        "a call in a nested (node-less) fn attributes to the enclosing named fn"
    );
}

#[test]
fn a_methods_calls_stay_on_the_method() {
    // Guard against over-attribution: a class method's own calls belong to the
    // method, and are not duplicated onto a sibling method.
    let src = b"class C {\n  a() { return helper(); }\n  b() { return 2; }\n}\nfunction helper() { return 1; }\n";
    let r = extract_source("c.js", src).unwrap();
    let a = r.nodes.iter().find(|n| n.label == ".a()").unwrap();
    let b = r.nodes.iter().find(|n| n.label == ".b()").unwrap();
    let helper = r.nodes.iter().find(|n| n.label == "helper()").unwrap();
    assert!(
        r.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == a.id && e.target == helper.id),
        "method a calls helper"
    );
    assert!(
        !r.edges
            .iter()
            .any(|e| e.relation == "calls" && e.source == b.id && e.target == helper.id),
        "method b does not call helper"
    );
}
