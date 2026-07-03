//! Cross-language linkage edges (FFI / subprocess / HTTP), gated on the
//! `cross-language` feature. Each test exercises a detector via the real
//! `extract_source` dispatch, which runs the post-passes.

#![cfg(feature = "cross-language")]
#![allow(unused_imports)]

use synaptic_core::Confidence;
use synaptic_extract::extract_source;

#[cfg(feature = "lang-python")]
#[test]
fn python_ctypes_emits_binds_native() {
    let src = b"import ctypes\n\ndef compute():\n    lib = ctypes.CDLL(\"./libmath.so\")\n    return lib.add(1, 2)\n";
    let r = extract_source("m.py", src).expect("python extracts");

    let lib = r
        .nodes
        .iter()
        .find(|n| n.label == "libmath")
        .expect("native library target node");
    assert!(
        lib.source_file.is_empty(),
        "native target is an external stub"
    );

    let compute = r.nodes.iter().find(|n| n.label == "compute()").unwrap();
    let edge = r
        .edges
        .iter()
        .find(|e| e.relation == "binds_native" && e.context.as_deref() == Some("ctypes"))
        .expect("a binds_native edge from the ctypes load");
    assert_eq!(
        edge.source, compute.id,
        "attributed to the enclosing function"
    );
    assert_eq!(edge.target, lib.id);
    assert_eq!(edge.confidence, Confidence::Inferred);
}

/// Labels of the targets of `invokes` edges in a result.
#[allow(dead_code)]
fn invoked(r: &synaptic_extract::ExtractionResult) -> Vec<String> {
    r.edges
        .iter()
        .filter(|e| e.relation == "invokes")
        .filter_map(|e| {
            r.nodes
                .iter()
                .find(|n| n.id == e.target)
                .map(|n| n.label.clone())
        })
        .collect()
}

#[cfg(feature = "lang-python")]
#[test]
fn python_subprocess_emits_invokes() {
    let src = b"import subprocess\nimport os\n\ndef deploy():\n    subprocess.run([\"mybinary\", \"--flag\"])\n    subprocess.Popen(\"otherbin\")\n    os.system(\"thirdbin -x\")\n";
    let r = extract_source("m.py", src).unwrap();
    let deploy = r.nodes.iter().find(|n| n.label == "deploy()").unwrap();

    let targets = invoked(&r);
    assert!(targets.contains(&"mybinary".to_string()), "{targets:?}");
    assert!(targets.contains(&"otherbin".to_string()), "{targets:?}");
    assert!(
        targets.contains(&"thirdbin".to_string()),
        "os.system first token: {targets:?}"
    );

    let edge = r
        .edges
        .iter()
        .find(|e| e.relation == "invokes")
        .expect("an invokes edge");
    assert_eq!(edge.source, deploy.id);
    assert_eq!(edge.confidence, Confidence::Inferred);
}

#[cfg(feature = "lang-go")]
#[test]
fn go_exec_command_emits_invokes() {
    let src = b"package main\n\nimport \"os/exec\"\n\nfunc Run() {\n\texec.Command(\"ffmpeg\", \"-i\", \"x\").Run()\n}\n";
    let r = extract_source("m.go", src).unwrap();
    assert!(
        invoked(&r).contains(&"ffmpeg".to_string()),
        "{:?}",
        invoked(&r)
    );
}

#[cfg(feature = "lang-rust")]
#[test]
fn rust_command_new_emits_invokes() {
    let src = b"use std::process::Command;\nfn run() {\n    Command::new(\"git\").arg(\"status\").output().unwrap();\n}\n";
    let r = extract_source("m.rs", src).unwrap();
    assert!(
        invoked(&r).contains(&"git".to_string()),
        "{:?}",
        invoked(&r)
    );
}

#[cfg(feature = "lang-typescript")]
#[test]
fn js_child_process_emits_invokes() {
    let src = b"import { execSync } from 'child_process';\nfunction build() {\n  execSync('webpack --mode production');\n}\n";
    let r = extract_source("m.ts", src).unwrap();
    assert!(
        invoked(&r).contains(&"webpack".to_string()),
        "{:?}",
        invoked(&r)
    );
}

/// The target label of the first `binds_native` edge with the given context.
#[allow(dead_code)]
fn native_target(r: &synaptic_extract::ExtractionResult, ctx: &str) -> Option<String> {
    let e = r
        .edges
        .iter()
        .find(|e| e.relation == "binds_native" && e.context.as_deref() == Some(ctx))?;
    r.nodes
        .iter()
        .find(|n| n.id == e.target)
        .map(|n| n.label.clone())
}

/// The `_pyo3_registers` list on a pyo3 module boundary node, as strings.
#[allow(dead_code)]
fn pyo3_registers(r: &synaptic_extract::ExtractionResult, module: &str) -> Vec<String> {
    r.nodes
        .iter()
        .find(|n| n.label == module)
        .and_then(|n| n.extra.get("_pyo3_registers"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(feature = "lang-rust")]
#[test]
fn rust_pyo3_function_module_marks_exports_and_registers() {
    // A function-style `#[pymodule] fn mathmod` records the names it registers
    // (`add`) in `_pyo3_registers`, and the `#[pyfunction] fn add` node is tagged
    // `_pyo3_export`. The boundary -> fn edge is stitched later by the graph pass.
    let src = b"use pyo3::prelude::*;\n\n#[pyfunction]\nfn add(a: i64, b: i64) -> i64 {\n    a + b\n}\n\n#[pymodule]\nfn mathmod(_py: Python<'_>, m: &PyModule) -> PyResult<()> {\n    m.add_function(wrap_pyfunction!(add, m)?)?;\n    Ok(())\n}\n";
    let r = extract_source("lib.rs", src).unwrap();
    assert!(
        pyo3_registers(&r, "pyo3:mathmod").contains(&"add".to_string()),
        "module records `add` as a registered export"
    );
    let add = r.nodes.iter().find(|n| n.label == "add()").unwrap();
    assert_eq!(
        add.extra.get("_pyo3_export").and_then(|v| v.as_str()),
        Some("add"),
        "the #[pyfunction] node is tagged as a pyo3 export"
    );
    // No edges are emitted per-file; the graph pass stitches them.
    assert!(
        edge_of(&r, "handled_by").is_none(),
        "scan_pyo3 is marker-only"
    );
}

#[cfg(feature = "lang-rust")]
#[test]
fn rust_pyo3_declarative_module_and_rename_and_class() {
    // Declarative module: a `#[pymodule_export] use` re-export, an inline
    // `#[pyfunction]` with a `#[pyo3(name)]` rename, and a `#[pyclass]`. The
    // boundary registers the Rust names; the renamed fn's node carries the Python
    // name in its tag.
    let src = b"use pyo3::prelude::*;\n\n#[pyfunction]\nfn double(x: i64) -> i64 { x * 2 }\n\n#[pyclass]\nstruct Widget;\n\n#[pymodule]\nmod gadget {\n    #[pymodule_export]\n    use super::double;\n\n    #[pyfunction]\n    #[pyo3(name = \"thrice\")]\n    fn triple(x: i64) -> i64 { x * 3 }\n\n    #[pyclass]\n    struct Inner;\n}\n";
    let r = extract_source("lib.rs", src).unwrap();
    let regs = pyo3_registers(&r, "pyo3:gadget");
    for want in ["double", "triple", "Inner"] {
        assert!(
            regs.contains(&want.to_string()),
            "registers {want}: {regs:?}"
        );
    }
    // The renamed fn keeps its Rust label but carries the Python name in the tag.
    let triple = r.nodes.iter().find(|n| n.label == "triple()").unwrap();
    assert_eq!(
        triple.extra.get("_pyo3_export").and_then(|v| v.as_str()),
        Some("thrice"),
        "#[pyo3(name)] override recorded"
    );
}

#[cfg(feature = "lang-typescript")]
#[test]
fn js_node_gyp_binds_native_addon() {
    let src =
        b"const addon = require('bindings')('myaddon');\nexport function run() { return addon.go(); }\n";
    let r = extract_source("m.ts", src).unwrap();
    assert_eq!(
        native_target(&r, "node-gyp").as_deref(),
        Some("myaddon"),
        "links to the native addon"
    );
}

#[cfg(feature = "lang-java")]
#[test]
fn java_native_method_binds_native() {
    let src = b"public class Foo {\n  public native int compute(int x);\n}\n";
    let r = extract_source("Foo.java", src).unwrap();
    assert_eq!(native_target(&r, "jni").as_deref(), Some("jni:compute"));
}

#[cfg(feature = "lang-c")]
#[test]
fn c_jni_impl_shares_target_with_java() {
    // The C implementation links to the SAME jni:compute target the Java native
    // method does, so a graph holding both files connects the two sides.
    let src =
        b"int Java_com_example_Foo_compute(void* env, void* obj, int x) {\n  return x * 2;\n}\n";
    let r = extract_source("foo.c", src).unwrap();
    assert_eq!(native_target(&r, "jni").as_deref(), Some("jni:compute"));
}

// --- HTTP/RPC service boundaries ---

/// Find an edge by relation; return (source-label, target-label, context).
#[allow(dead_code)]
fn edge_of(
    r: &synaptic_extract::ExtractionResult,
    rel: &str,
) -> Option<(String, String, Option<String>)> {
    let e = r.edges.iter().find(|e| e.relation == rel)?;
    let lbl = |id: &synaptic_core::NodeId| {
        r.nodes
            .iter()
            .find(|n| &n.id == id)
            .map(|n| n.label.clone())
            .unwrap_or_default()
    };
    Some((lbl(&e.source), lbl(&e.target), e.context.clone()))
}

#[cfg(feature = "lang-python")]
#[test]
fn python_route_links_handler() {
    let src = b"from flask import Flask\napp = Flask(__name__)\n\n@app.get(\"/api/users\")\ndef list_users():\n    return []\n";
    let r = extract_source("server.py", src).unwrap();
    assert!(
        r.nodes.iter().any(|n| n.label == "/api/users"),
        "route node by path"
    );
    // route -> handler (handled_by), so reverse-impact from the handler reaches
    // the route and its clients.
    let (src_lbl, tgt_lbl, ctx) = edge_of(&r, "handled_by").expect("handled_by edge");
    assert_eq!(src_lbl, "/api/users", "route is the source");
    assert_eq!(tgt_lbl, "list_users()", "handler is the decorated fn");
    assert_eq!(ctx.as_deref(), Some("GET"));
}

#[cfg(feature = "lang-python")]
#[test]
fn python_requests_client_links_route_by_path() {
    // The client URL normalizes to a path, so it shares the route node a server
    // handler would create for "/api/users".
    let src =
        b"import requests\n\ndef fetch():\n    return requests.get(\"http://svc/api/users\").json()\n";
    let r = extract_source("client.py", src).unwrap();
    let (src_lbl, tgt_lbl, ctx) = edge_of(&r, "calls_service").expect("calls_service edge");
    assert_eq!(src_lbl, "fetch()");
    assert_eq!(tgt_lbl, "/api/users", "URL normalized to a path");
    assert_eq!(
        ctx.as_deref(),
        Some("GET svc"),
        "authority rides as context"
    );
}

#[cfg(feature = "lang-typescript")]
#[test]
fn js_express_route_and_axios_client() {
    let server = extract_source(
        "server.ts",
        b"function setup(app) {\n  app.post('/api/login', (req, res) => res.end());\n}\n",
    )
    .unwrap();
    let (src, _, ctx) = edge_of(&server, "handled_by").expect("express handled_by edge");
    assert_eq!(src, "/api/login", "route is the source");
    assert_eq!(ctx.as_deref(), Some("POST"));

    let client = extract_source(
        "client.ts",
        b"function login() {\n  return axios.post('/api/login', {});\n}\n",
    )
    .unwrap();
    let (_, ctgt, cctx) = edge_of(&client, "calls_service").expect("axios calls_service edge");
    assert_eq!(ctgt, "/api/login");
    assert_eq!(cctx.as_deref(), Some("POST"));
}

#[cfg(feature = "lang-python")]
#[test]
fn route_handler_skips_commented_def() {
    // A commented-out `def` between the decorator and the real handler must not
    // be picked up as the handler.
    let src = b"@app.get(\"/d\")\n# def fake():\ndef real():\n    return 1\n";
    let r = extract_source("server.py", src).unwrap();
    let (_, tgt, _) = edge_of(&r, "handled_by").expect("handled_by edge");
    assert_eq!(
        tgt, "real()",
        "handler is the real def, not the commented one"
    );
}

#[cfg(feature = "lang-typescript")]
#[test]
fn non_http_fetch_is_not_a_route() {
    // `.fetch('SELECT ...')` (ORM/cache) must not create a route node.
    let src = b"function load() {\n  return db.fetch('SELECT * FROM users');\n}\n";
    let r = extract_source("m.ts", src).unwrap();
    assert!(
        edge_of(&r, "calls_service").is_none(),
        "non-path fetch arg should not produce a calls_service edge"
    );
    // A real path-shaped fetch still works.
    let r2 = extract_source("m2.ts", b"function go() { return fetch('/api/x'); }\n").unwrap();
    assert!(edge_of(&r2, "calls_service").is_some());
}

#[cfg(feature = "lang-go")]
#[test]
fn go_handlefunc_route_and_http_get_client() {
    let server = extract_source(
        "server.go",
        b"package main\nimport \"net/http\"\nfunc setup() {\n\thttp.HandleFunc(\"/healthz\", handler)\n}\n",
    )
    .unwrap();
    let (src, _, _) = edge_of(&server, "handled_by").expect("go handled_by edge");
    assert_eq!(src, "/healthz", "route is the source");

    let client = extract_source(
        "client.go",
        b"package main\nimport \"net/http\"\nfunc ping() {\n\thttp.Get(\"http://svc/healthz\")\n}\n",
    )
    .unwrap();
    let (_, ctgt, _) = edge_of(&client, "calls_service").expect("go calls_service edge");
    assert_eq!(ctgt, "/healthz");
}

#[cfg(feature = "lang-go")]
#[test]
fn go_122_method_pattern_route_splits_method_and_path() {
    // Go 1.22 ServeMux: `mux.HandleFunc("GET /healthz", h)` -- the leading method
    // must be split out of the path (else the route is `/GET /healthz` with method
    // ANY, and a client call to `/healthz` won't match it).
    let src = b"package main\nimport \"net/http\"\nfunc setup(mux *http.ServeMux) {\n\tmux.HandleFunc(\"GET /healthz\", handler)\n\tmux.HandleFunc(\"POST /v1/users/{id}\", create)\n}\n";
    let r = extract_source("s.go", src).unwrap();
    let routes: Vec<(String, Option<String>)> = r
        .edges
        .iter()
        .filter(|e| e.relation == "handled_by")
        .map(|e| {
            let lbl = r
                .nodes
                .iter()
                .find(|n| n.id == e.source)
                .map(|n| n.label.clone())
                .unwrap_or_default();
            (lbl, e.context.clone())
        })
        .collect();
    assert!(
        routes
            .iter()
            .any(|(p, m)| p == "/healthz" && m.as_deref() == Some("GET")),
        "{routes:?}"
    );
    assert!(
        routes
            .iter()
            .any(|(p, m)| p == "/v1/users/{id}" && m.as_deref() == Some("POST")),
        "{routes:?}"
    );
    assert!(
        !r.nodes
            .iter()
            .any(|n| n.label.starts_with("/GET") || n.label.starts_with("/POST")),
        "no HTTP method left in any route path"
    );
}

// --- Rust HTTP routes (axum / actix) + reqwest client ---

#[cfg(feature = "lang-rust")]
#[test]
fn rust_axum_route_links_handler() {
    // axum: `.route("/path", get(handler))` -- the handler is a named fn ref, not
    // the enclosing function, so route -> handler resolves by handler name.
    let src = b"async fn list_users() -> String { String::new() }\n\nfn app() -> Router {\n    Router::new().route(\"/api/users\", get(list_users))\n}\n";
    let r = extract_source("server.rs", src).unwrap();
    assert!(
        r.nodes.iter().any(|n| n.label == "/api/users"),
        "route node by path"
    );
    let (src_lbl, tgt_lbl, ctx) = edge_of(&r, "handled_by").expect("axum handled_by edge");
    assert_eq!(src_lbl, "/api/users", "route is the source");
    assert_eq!(
        tgt_lbl, "list_users()",
        "handler is the named fn, not app()"
    );
    assert_eq!(ctx.as_deref(), Some("GET"));
}

#[cfg(feature = "lang-rust")]
#[test]
fn rust_axum_external_handler_records_marker() {
    // When the handler is defined in another file, the route node records the
    // handler name (and method) for the graph-level cross-file resolver; no
    // (misleading) file-node fallback edge is emitted.
    let src = b"use axum::routing::get;\nfn app() -> Router {\n    Router::new().route(\"/api/x\", get(handlers::serve))\n}\n";
    let r = extract_source("app.rs", src).unwrap();
    let route = r
        .nodes
        .iter()
        .find(|n| n.label == "/api/x")
        .expect("route node");
    assert_eq!(
        route.extra.get("_route_handler").and_then(|v| v.as_str()),
        Some("serve"),
        "qualified handler keyed by last segment"
    );
    assert_eq!(
        route.extra.get("_route_method").and_then(|v| v.as_str()),
        Some("GET")
    );
    assert!(
        edge_of(&r, "handled_by").is_none(),
        "no file-node fallback for an external handler"
    );
}

#[cfg(feature = "lang-rust")]
#[test]
fn rust_actix_attr_route_links_handler() {
    // actix-web: `#[post("/path")]` attribute macro over the handler fn (the
    // decorator-style case, like Python/Express).
    let src = b"#[post(\"/api/login\")]\nasync fn login() -> String {\n    String::new()\n}\n";
    let r = extract_source("server.rs", src).unwrap();
    let (src_lbl, tgt_lbl, ctx) = edge_of(&r, "handled_by").expect("actix handled_by edge");
    assert_eq!(src_lbl, "/api/login", "route is the source");
    assert_eq!(tgt_lbl, "login()", "handler is the decorated fn");
    assert_eq!(ctx.as_deref(), Some("POST"));
}

#[cfg(feature = "lang-rust")]
#[test]
fn rust_reqwest_get_client_links_route() {
    let src =
        b"async fn fetch_users() {\n    let _ = reqwest::get(\"http://svc/api/users\").await;\n}\n";
    let r = extract_source("client.rs", src).unwrap();
    let (src_lbl, tgt_lbl, ctx) = edge_of(&r, "calls_service").expect("reqwest calls_service edge");
    assert_eq!(src_lbl, "fetch_users()");
    assert_eq!(tgt_lbl, "/api/users", "URL normalized to a path");
    assert_eq!(
        ctx.as_deref(),
        Some("GET svc"),
        "authority rides as context"
    );
}

#[cfg(feature = "lang-rust")]
#[test]
fn rust_reqwest_method_client_links_route() {
    // `client.post("https://...")` -- the absolute URL is the signal it's an HTTP
    // client call, not some other `.post(...)`.
    let src = b"async fn login(client: reqwest::Client) {\n    let _ = client.post(\"https://svc/api/login\").send().await;\n}\n";
    let r = extract_source("client.rs", src).unwrap();
    let (_, tgt_lbl, ctx) =
        edge_of(&r, "calls_service").expect("reqwest method calls_service edge");
    assert_eq!(tgt_lbl, "/api/login");
    assert_eq!(
        ctx.as_deref(),
        Some("POST svc"),
        "authority rides as context"
    );
}

#[cfg(feature = "lang-rust")]
#[test]
fn rust_map_get_is_not_a_service_call() {
    // `.get("/local")` on a non-absolute path must not be treated as a client
    // call (could be a map/router lookup). Only absolute http(s) URLs qualify for
    // the generic method-call form.
    let src = b"fn load(m: std::collections::HashMap<String, u32>) -> Option<u32> {\n    m.get(\"/local/key\").copied()\n}\n";
    let r = extract_source("m.rs", src).unwrap();
    assert!(
        edge_of(&r, "calls_service").is_none(),
        "a non-URL .get arg should not produce a calls_service edge"
    );
}

// --- gRPC service boundaries (tonic / Python stubs) ---
//
// Keyed by the (lowercased) service name, so a tonic server impl, a tonic client,
// and a cross-language client all land on one `grpc:<service>` node -- sidestepping
// the proto PascalCase vs Rust snake_case rpc-method naming mismatch.

#[cfg(feature = "lang-rust")]
#[test]
fn rust_tonic_server_links_service_methods() {
    let src = b"use tonic::async_trait;\n\n#[tonic::async_trait]\nimpl Greeter for MyGreeter {\n    async fn say_hello(&self) -> Result<(), ()> {\n        Ok(())\n    }\n}\n";
    let r = extract_source("server.rs", src).unwrap();
    assert!(
        r.nodes.iter().any(|n| n.label == "grpc:greeter"),
        "grpc service node"
    );
    let (src_lbl, tgt_lbl, ctx) = edge_of(&r, "handled_by").expect("grpc handled_by edge");
    assert_eq!(src_lbl, "grpc:greeter", "service node is the source");
    // The Rust extractor labels methods `.<name>()`; the edge targets that node.
    assert_eq!(tgt_lbl, ".say_hello()", "rpc method impl is the handler");
    assert_eq!(ctx.as_deref(), Some("gRPC"));
}

#[cfg(feature = "lang-rust")]
#[test]
fn rust_tonic_server_brace_in_literal_links_all_methods() {
    // A `}` inside a string or char literal must not truncate the impl body's
    // brace-matched span, so every rpc method still links to the service.
    let src = b"use tonic::async_trait;\n#[tonic::async_trait]\nimpl Greeter for S {\n    async fn first(&self) {\n        let s = \"}\";\n        let c = '}';\n    }\n    async fn second(&self) {}\n}\n";
    let r = extract_source("s.rs", src).unwrap();
    let handlers = r
        .edges
        .iter()
        .filter(|e| e.relation == "handled_by" && e.context.as_deref() == Some("gRPC"))
        .count();
    assert_eq!(
        handlers, 2,
        "both rpc methods linked despite brace-in-literal"
    );
}

#[cfg(feature = "lang-rust")]
#[test]
fn rust_tonic_server_brace_in_raw_string_links_all_methods() {
    // Braces (and quotes) inside a Rust raw string must not truncate the impl
    // body's brace-matched span -- the masker blanks raw strings, so both rpc
    // methods still link.
    let src = b"use tonic::async_trait;\n#[tonic::async_trait]\nimpl Greeter for S {\n    async fn first(&self) {\n        let _j = r#\"{\"k\": \"v\"} } {{ \"#;\n    }\n    async fn second(&self) {}\n}\n";
    let r = extract_source("s.rs", src).unwrap();
    let handlers = r
        .edges
        .iter()
        .filter(|e| e.relation == "handled_by" && e.context.as_deref() == Some("gRPC"))
        .count();
    assert_eq!(
        handlers, 2,
        "both methods linked despite braces in a raw string"
    );
}

#[cfg(feature = "lang-rust")]
#[test]
fn rust_tonic_client_links_service() {
    let src = b"use tonic::transport::Channel;\nasync fn greet() {\n    let mut client = GreeterClient::connect(\"http://[::1]:50051\").await.unwrap();\n    let _ = client.say_hello(()).await;\n}\n";
    let r = extract_source("client.rs", src).unwrap();
    let (src_lbl, tgt_lbl, ctx) = edge_of(&r, "calls_service").expect("grpc calls_service edge");
    assert_eq!(src_lbl, "greet()");
    assert_eq!(
        tgt_lbl, "grpc:greeter",
        "client targets the service node (Client suffix stripped)"
    );
    assert_eq!(ctx.as_deref(), Some("gRPC"));
}

#[cfg(feature = "lang-python")]
#[test]
fn python_grpc_stub_links_service() {
    let src = b"import greeter_pb2_grpc\n\ndef greet(channel):\n    stub = greeter_pb2_grpc.GreeterStub(channel)\n    return stub.SayHello(req)\n";
    let r = extract_source("client.py", src).unwrap();
    let (src_lbl, tgt_lbl, ctx) = edge_of(&r, "calls_service").expect("python grpc calls_service");
    assert_eq!(src_lbl, "greet()");
    assert_eq!(tgt_lbl, "grpc:greeter");
    assert_eq!(ctx.as_deref(), Some("gRPC"));
}

#[cfg(feature = "lang-rust")]
#[test]
fn non_tonic_client_suffix_is_not_grpc() {
    // `HttpClient::new(...)` in a file with no `tonic` mention must NOT be read as
    // a gRPC client -- the `<Name>Client` shape alone is too common.
    let src = b"fn setup() {\n    let _ = HttpClient::new(\"https://api\");\n}\n";
    let r = extract_source("m.rs", src).unwrap();
    assert!(
        edge_of(&r, "calls_service").is_none(),
        "no tonic mention, so no grpc client edge"
    );
}

#[cfg(feature = "lang-rust")]
#[test]
fn grpc_same_named_methods_resolve_by_impl_scope() {
    // Two tonic services in one file each define `run`. Each service must link to
    // the `run` inside its OWN impl, not whichever appears first.
    let src = b"use tonic::async_trait;\n#[tonic::async_trait]\nimpl Greeter for A {\n    async fn run(&self) {}\n}\n#[tonic::async_trait]\nimpl Farewell for B {\n    async fn run(&self) {}\n}\n";
    let r = extract_source("s.rs", src).unwrap();
    let svc = |label: &str| {
        r.nodes
            .iter()
            .find(|n| n.label == label)
            .map(|n| n.id.clone())
    };
    let target_of = |svc_id: &synaptic_core::NodeId| {
        r.edges
            .iter()
            .find(|e| e.relation == "handled_by" && &e.source == svc_id)
            .map(|e| e.target.clone())
    };
    let g = target_of(&svc("grpc:greeter").unwrap()).unwrap();
    let f = target_of(&svc("grpc:farewell").unwrap()).unwrap();
    assert_ne!(
        g, f,
        "same-named methods resolve to distinct nodes by impl scope"
    );
}

#[cfg(feature = "lang-rust")]
#[test]
fn denylisted_client_in_tonic_file_is_not_grpc() {
    // Even in a tonic file, a well-known non-gRPC `<Name>Client` (HttpClient,
    // DbClient, ...) must not be read as a gRPC client; the real one still is.
    let src = b"use tonic::transport::Channel;\nasync fn go() {\n    let _ = HttpClient::new(\"x\");\n    let _ = DbClient::connect(\"y\").await;\n    let _ = GreeterClient::connect(\"http://svc\").await;\n}\n";
    let r = extract_source("c.rs", src).unwrap();
    let targets: Vec<String> = r
        .edges
        .iter()
        .filter(|e| e.relation == "calls_service")
        .filter_map(|e| {
            r.nodes
                .iter()
                .find(|n| n.id == e.target)
                .map(|n| n.label.clone())
        })
        .collect();
    assert_eq!(
        targets,
        vec!["grpc:greeter".to_string()],
        "only the real gRPC client: {targets:?}"
    );
}

#[cfg(feature = "lang-rust")]
#[test]
fn non_reqwest_method_call_is_not_a_service_call() {
    // `.post("https://...")` in a file with no `reqwest` must not be a client call
    // (the receiver is unknown; absolute URL alone is too weak a signal).
    let src = b"fn f(c: SomeClient) {\n    let _ = c.post(\"https://api/thing\");\n}\n";
    let r = extract_source("m.rs", src).unwrap();
    assert!(
        edge_of(&r, "calls_service").is_none(),
        "no reqwest in file, so the bare .post is not a service call"
    );
}

// --- comment / docstring masking: detectors must not fire inside comments ---

/// Labels of route nodes in a result.
#[allow(dead_code)]
fn route_labels(r: &synaptic_extract::ExtractionResult) -> Vec<String> {
    let mut v: Vec<String> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|x| x.as_str()) == Some("route"))
        .map(|n| n.label.clone())
        .collect();
    v.sort();
    v
}

#[cfg(feature = "lang-rust")]
#[test]
fn commented_rust_routes_are_ignored() {
    // A `//` line comment, a `/* */` block comment, and a `///` doc comment must
    // not produce routes; only the real registration does.
    let src = b"async fn h() {}\n/// docs: Router::new().route(\"/doc\", get(h))\nfn app() {\n    // Router::new().route(\"/line\", get(h))\n    /* .route(\"/block\", get(h)) */\n    Router::new().route(\"/real\", get(h));\n}\n";
    let r = extract_source("s.rs", src).unwrap();
    assert_eq!(
        route_labels(&r),
        vec!["/real".to_string()],
        "only the uncommented route survives"
    );
}

#[cfg(feature = "lang-python")]
#[test]
fn commented_and_docstring_python_routes_are_ignored() {
    // A `#` comment and a triple-quoted docstring example must not produce routes.
    let src = b"from flask import Flask\napp = Flask(__name__)\n\ndef helper():\n    \"\"\"Example usage:\n    @app.get('/docexample')\n    def x(): pass\n    \"\"\"\n    return 1\n\n# @app.get('/commented')\n# def ghost(): return 1\n\n@app.get('/real')\ndef real():\n    return []\n";
    let r = extract_source("s.py", src).unwrap();
    assert_eq!(route_labels(&r), vec!["/real".to_string()]);
}

#[cfg(feature = "lang-python")]
#[test]
fn commented_subprocess_is_ignored_but_real_kept() {
    let src = b"import subprocess\n\ndef f():\n    # subprocess.run([\"ghost\"])\n    subprocess.run([\"real\"])\n";
    let r = extract_source("s.py", src).unwrap();
    let invoked = invoked(&r);
    assert!(invoked.contains(&"real".to_string()), "{invoked:?}");
    assert!(
        !invoked.contains(&"ghost".to_string()),
        "commented: {invoked:?}"
    );
}

#[cfg(feature = "lang-rust")]
#[test]
fn url_inside_rust_string_is_not_masked() {
    // Masking must preserve string contents: a real reqwest URL still extracts.
    let src =
        b"async fn f() {\n    // not this one: reqwest::get(\"http://svc/ghost\")\n    let _ = reqwest::get(\"http://svc/real\").await;\n}\n";
    let r = extract_source("c.rs", src).unwrap();
    assert_eq!(route_labels(&r), vec!["/real".to_string()]);
}

// --- SQL string literal detection ---

#[cfg(feature = "lang-python")]
#[cfg(feature = "lang-sql")]
#[test]
fn scan_sql_links_python_query_to_table_stub() {
    let src = br#"
def list_orders(conn):
    cur = conn.cursor()
    cur.execute("SELECT id, total FROM orders WHERE tenant_id = %s", [t])
    return cur.fetchall()
"#;
    let r = extract_source("app.py", src).unwrap();
    let to_orders = r.edges.iter().any(|e| {
        (e.relation == "queries" || e.relation == "reads_from")
            && r.nodes
                .iter()
                .any(|n| n.id == e.target && n.label.eq_ignore_ascii_case("orders"))
    });
    assert!(
        to_orders,
        "expected a code->orders SQL edge; edges: {:?}",
        r.edges
    );
}

#[cfg(feature = "lang-javascript")]
#[cfg(feature = "lang-sql")]
#[test]
fn scan_sql_classifies_write_as_writes_to() {
    let src = br#"const sql = "UPDATE accounts SET balance = balance - 1 WHERE id = $1";"#;
    let r = extract_source("app.js", src).unwrap();
    let writes = r.edges.iter().any(|e| e.relation == "writes_to");
    assert!(
        writes,
        "UPDATE should map to writes_to; edges: {:?}",
        r.edges
    );
}

#[cfg(feature = "lang-javascript")]
#[cfg(feature = "lang-sql")]
#[test]
fn scan_sql_ignores_prose_string_with_leading_verb() {
    // Real a11ycore false positive: a UI string starting with "Update" but with no
    // SET clause must NOT become a SQL write edge.
    let src = br#"function label() { return 'Update password'; }"#;
    let r = extract_source("Auth.tsx", src).unwrap();
    let sql_edge = r
        .edges
        .iter()
        .any(|e| matches!(e.relation.as_str(), "queries" | "writes_to" | "calls_proc"));
    assert!(
        !sql_edge,
        "prose 'Update password' must not be treated as SQL; edges: {:?}",
        r.edges
    );
}

#[cfg(feature = "lang-javascript")]
#[cfg(feature = "lang-sql")]
#[test]
fn scan_sql_ignores_delete_prose_without_from() {
    let src = br#"const msg = 'Delete account permanently';"#;
    let r = extract_source("app.tsx", src).unwrap();
    let sql_edge = r
        .edges
        .iter()
        .any(|e| matches!(e.relation.as_str(), "queries" | "writes_to" | "calls_proc"));
    assert!(
        !sql_edge,
        "prose 'Delete account...' (no FROM) must not be SQL; edges: {:?}",
        r.edges
    );
}

#[cfg(feature = "lang-javascript")]
#[cfg(feature = "lang-sql")]
#[test]
fn scan_sql_still_detects_real_delete_with_from() {
    let src = br#"const sql = "DELETE FROM sessions WHERE expired = true";"#;
    let r = extract_source("app.js", src).unwrap();
    let writes = r.edges.iter().any(|e| e.relation == "writes_to");
    assert!(
        writes,
        "real DELETE FROM should still map to writes_to; edges: {:?}",
        r.edges
    );
}

#[cfg(feature = "lang-python")]
#[cfg(feature = "lang-sql")]
#[test]
fn scan_sql_stores_snippet_on_edge() {
    let src = br#"q = "SELECT * FROM orders WHERE id = 1""#;
    let r = extract_source("app.py", src).unwrap();
    let e = r
        .edges
        .iter()
        .find(|e| e.relation == "queries")
        .expect("queries edge");
    let snip = e.extra.get("sql").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        snip.to_ascii_uppercase().starts_with("SELECT"),
        "snippet: {snip}"
    );
}

// --- WebSocket coupling ---

/// The `calls_service`/`handled_by` edge for a `wsmsg`/`wsendpoint` boundary node
/// whose label matches `label`, if any.
#[allow(dead_code)]
fn ws_edge<'a>(
    r: &'a synaptic_extract::ExtractionResult,
    node_type: &str,
    label: &str,
) -> Option<&'a synaptic_core::Edge> {
    let node = r.nodes.iter().find(|n| {
        n.extra.get("_node_type").and_then(|v| v.as_str()) == Some(node_type) && n.label == label
    })?;
    r.edges
        .iter()
        .find(|e| e.source == node.id || e.target == node.id)
}

#[cfg(feature = "lang-javascript")]
#[test]
fn js_ws_command_send_is_a_client_message() {
    // `client.send({ cmd: 'subscribe' })` -> the enclosing fn calls_service a
    // `wsmsg:subscribe` node. The file must look WebSocket-ish (2026-07 audit:
    // ungated `.send({type})` matched HTTP response bodies like
    // `res.send({ type: 'success' })`).
    let src = b"import { client } from './websocket.js';\nasync function subscribe(topic) {\n  await client.send({ cmd: 'subscribe', value: topic });\n}\n";
    let r = extract_source("client.js", src).unwrap();
    let node = r
        .nodes
        .iter()
        .find(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_message"))
        .expect("a ws_message node");
    assert_eq!(node.label, "ws #subscribe");
    let f_subscribe = r.nodes.iter().find(|n| n.label == "subscribe()").unwrap();
    let edge = r
        .edges
        .iter()
        .find(|e| e.relation == "calls_service" && e.target == node.id)
        .expect("calls_service edge to the ws message");
    assert_eq!(edge.source, f_subscribe.id, "attributed to the sender fn");
    assert_eq!(edge.confidence, Confidence::Inferred);
    // `.request({ cmd: 'fetch' })` counts too.
    let src2 = b"import { client } from './websocket.js';\nasync function poll() {\n  return client.request({ cmd: \"fetch\" });\n}\n";
    let r2 = extract_source("c.js", src2).unwrap();
    assert!(r2.nodes.iter().any(|n| n.label == "ws #fetch"
        && n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_message")));
    // A plain `res.send(body)` (no command object) must NOT create a ws message.
    let plain = b"function h(req, res) {\n  res.send({ ok: true });\n}\n";
    let rp = extract_source("h.js", plain).unwrap();
    assert!(
        !rp.nodes
            .iter()
            .any(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_message")),
        "no command key -> no ws message node"
    );
}

#[cfg(feature = "lang-javascript")]
#[test]
fn js_electron_ipc_links_invoke_to_handle_via_channel_node() {
    // Electron IPC: a renderer/preload `ipcRenderer.invoke('app:getStatus')` and a
    // main `ipcMain.handle('app:getStatus', ...)` attach to one channel-keyed
    // `ipc #app:getStatus` boundary node -- the invoke site calls_service it, the
    // handler is handled_by it -- so a handler reached only across the IPC boundary
    // is no longer a 0-caller node.
    let preload = b"const { ipcRenderer } = require('electron');\nfunction getStatus() {\n  return ipcRenderer.invoke('app:getStatus');\n}\n";
    let r = extract_source("bridge.js", preload).unwrap();
    let node = r
        .nodes
        .iter()
        .find(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ipc_channel"))
        .expect("an ipc_channel node");
    assert_eq!(node.label, "ipc #app:getStatus");
    assert!(node.source_file.is_empty(), "channel is an external stub");
    let f = r.nodes.iter().find(|n| n.label == "getStatus()").unwrap();
    let edge = r
        .edges
        .iter()
        .find(|e| e.relation == "calls_service" && e.target == node.id)
        .expect("calls_service edge to the ipc channel");
    assert_eq!(edge.source, f.id, "attributed to the invoking fn");
    assert_eq!(edge.context.as_deref(), Some("ipc"));
    assert_eq!(edge.confidence, Confidence::Inferred);

    // Main side: the ipcMain.handle handler is reached from the channel via
    // handled_by (same channel id as the invoke side, so they meet in the graph).
    let main = b"import { ipcMain } from 'electron';\nfunction registerHandlers() {\n  ipcMain.handle('app:getStatus', async () => readStatus());\n}\n";
    let rm = extract_source("main.js", main).unwrap();
    let node2 = rm
        .nodes
        .iter()
        .find(|n| n.label == "ipc #app:getStatus")
        .expect("ipc node on the main side");
    assert_eq!(
        node2.id, node.id,
        "invoke and handle meet on one channel id"
    );
    let reg = rm
        .nodes
        .iter()
        .find(|n| n.label == "registerHandlers()")
        .unwrap();
    let hb = rm
        .edges
        .iter()
        .find(|e| e.relation == "handled_by" && e.source == node2.id)
        .expect("handled_by edge from the channel");
    assert_eq!(hb.target, reg.id, "handler attributed to the enclosing fn");
    assert_eq!(hb.context.as_deref(), Some("ipc"));

    // A plain `.handle(...)` with no electron IPC API in the file must NOT create a
    // channel node (avoids false positives on ordinary `.handle`/`.on` calls).
    let plain = b"function f() {\n  emitter.handle('x', g);\n}\n";
    let rp = extract_source("p.js", plain).unwrap();
    assert!(
        !rp.nodes
            .iter()
            .any(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ipc_channel")),
        "no electron ipc API -> no channel node"
    );
}

#[cfg(feature = "lang-javascript")]
#[test]
fn js_electron_ipc_main_to_renderer_push() {
    // Main pushes via `webContents.send('evt')` (client) and the renderer listens
    // via `ipcRenderer.on('evt', fn)` (handler); both meet on `ipc #evt`.
    let mainp = b"const { ipcRenderer } = require('electron');\nfunction listen() {\n  ipcRenderer.on('app:progress', (e, p) => render(p));\n}\n";
    let r = extract_source("renderer.js", mainp).unwrap();
    let node = r
        .nodes
        .iter()
        .find(|n| n.label == "ipc #app:progress")
        .expect("ipc node from ipcRenderer.on");
    let hb = r
        .edges
        .iter()
        .find(|e| e.relation == "handled_by" && e.source == node.id)
        .expect("renderer .on handler is handled_by the channel");
    assert_eq!(hb.context.as_deref(), Some("ipc"));

    let send = b"function push(win, p) {\n  win.webContents.send('app:progress', p);\n}\n";
    let rs = extract_source("push.js", send).unwrap();
    let n2 = rs
        .nodes
        .iter()
        .find(|n| n.label == "ipc #app:progress")
        .expect("ipc node from webContents.send");
    assert_eq!(n2.id, node.id);
    assert!(rs.edges.iter().any(|e| e.relation == "calls_service"
        && e.target == n2.id
        && e.context.as_deref() == Some("ipc")));
}

// --- Event-bus coupling ---

#[cfg(feature = "lang-typescript")]
#[test]
fn event_emitter_links_publisher_and_subscriber_through_channel() {
    // A Node EventEmitter: `bus.on('e', h)` subscribes (handled_by the channel),
    // `bus.emit('e', x)` publishes (calls_service it). Gated on an `EventEmitter`
    // token in the file so ordinary `.on(`/`.emit(` (jQuery, RxJS) do not fire.
    let src = b"import { EventEmitter } from 'events';\nconst bus = new EventEmitter();\nfunction wire() { bus.on('user:login', handleLogin); }\nfunction fire() { bus.emit('user:login', u); }\n";
    let r = extract_source("app/bus.ts", src).unwrap();
    let chan = r
        .nodes
        .iter()
        .find(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("event_channel"))
        .expect("an event_channel node");
    assert_eq!(chan.label, "event #user:login");
    assert!(chan.source_file.is_empty(), "channel is an external stub");
    assert!(
        r.edges
            .iter()
            .any(|e| e.target == chan.id && e.relation == "calls_service"),
        "publisher calls_service the channel"
    );
    assert!(
        r.edges
            .iter()
            .any(|e| e.source == chan.id && e.relation == "handled_by"),
        "subscriber handled_by the channel"
    );
}

#[cfg(feature = "lang-typescript")]
#[test]
fn custom_event_links_but_standard_dom_event_is_ignored() {
    // `dispatchEvent(new CustomEvent('e'))` publishes; `addEventListener('e')`
    // subscribes -- but only for non-standard (app) event names. A standard DOM
    // event (`click`) must not mint a channel.
    let src = b"function a() { el.dispatchEvent(new CustomEvent('app:refresh')); }\nfunction b() { el.addEventListener('app:refresh', onRefresh); }\nfunction c() { el.addEventListener('click', onClick); }\n";
    let r = extract_source("app/ui.ts", src).unwrap();
    let chan = r
        .nodes
        .iter()
        .find(|n| n.label == "event #app:refresh")
        .expect("custom event channel");
    assert!(r
        .edges
        .iter()
        .any(|e| e.target == chan.id && e.relation == "calls_service"));
    assert!(r
        .edges
        .iter()
        .any(|e| e.source == chan.id && e.relation == "handled_by"));
    assert!(
        !r.nodes.iter().any(|n| n.label == "event #click"),
        "standard DOM event must not mint a channel"
    );
}

#[cfg(feature = "lang-typescript")]
#[test]
fn bare_emit_on_without_emitter_token_does_not_fire() {
    // No EventEmitter signal -> ordinary `.on(`/`.emit(` must not mint a channel.
    let src = b"function wire() { $el.on('click', go); socket.emit('ping', x); }\n";
    let r = extract_source("app/ui.ts", src).unwrap();
    assert!(
        !r.nodes
            .iter()
            .any(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("event_channel")),
        "no EventEmitter token -> no event channel"
    );
}

#[cfg(feature = "lang-csharp")]
#[test]
fn csharp_event_invoke_and_subscribe_link_through_channel() {
    // `StatusChanged?.Invoke(...)` raises (publisher); `x.StatusChanged += h`
    // subscribes (handler), but `+=` only counts for a known event name, so a
    // `total += amount` accumulator does not mint a spurious channel.
    let src = b"class S {\n  event System.EventHandler StatusChanged;\n  void Raise() { StatusChanged?.Invoke(this, e); }\n  void Wire() { svc.StatusChanged += OnStatus; }\n  void Acc() { int total = 0; total += amount; }\n}\n";
    let r = extract_source("svc/S.cs", src).unwrap();
    let chan = r
        .nodes
        .iter()
        .find(|n| n.label == "event #StatusChanged")
        .expect("event channel");
    assert!(r
        .edges
        .iter()
        .any(|e| e.target == chan.id && e.relation == "calls_service"));
    assert!(r
        .edges
        .iter()
        .any(|e| e.source == chan.id && e.relation == "handled_by"));
    assert!(
        !r.nodes.iter().any(|n| n.label == "event #total"),
        "arithmetic += must not mint a channel"
    );
}

#[cfg(feature = "lang-csharp")]
#[test]
fn csharp_ws_server_case_and_endpoint() {
    // WebSocketSharp server: an endpoint from AddWebSocketService and handlers from
    // the `case "..."` arms. Both meet the JS client's `wsendpoint`/`wsmsg` ids.
    let src = br#"
public class FeedServer {
  public void Start() {
    Server = new WebSocketServer("ws://127.0.0.1:45000");
    Server.AddWebSocketService<Feed>("/feed");
  }
  public void OnMessage(string text) {
    switch (cmd) {
      case "subscribe": DoSubscribe(); break;
      case "fetch": DoFetch(); break;
    }
  }
}
"#;
    let r = extract_source("feedServer.cs", src).unwrap();
    // Endpoint node, handled_by the registering method.
    let ep = r
        .nodes
        .iter()
        .find(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_endpoint"))
        .expect("a ws_endpoint node");
    assert_eq!(ep.label, "ws /feed");
    assert!(r
        .edges
        .iter()
        .any(|e| e.relation == "handled_by" && e.source == ep.id));
    // Per-command handler nodes for subscribe + fetch.
    let msgs: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_message"))
        .map(|n| n.label.as_str())
        .collect();
    assert!(msgs.contains(&"ws #subscribe"), "{msgs:?}");
    assert!(msgs.contains(&"ws #fetch"), "{msgs:?}");
    let subscribe = r.nodes.iter().find(|n| n.label == "ws #subscribe").unwrap();
    assert!(
        r.edges
            .iter()
            .any(|e| e.relation == "handled_by" && e.source == subscribe.id),
        "server case is a handled_by"
    );
}

#[cfg(feature = "lang-csharp")]
#[test]
fn csharp_non_ws_switch_is_ignored() {
    // A `case "..."` outside any WebSocket context must not mint ws message nodes.
    let src = br#"
public class Parser {
  public void Handle(string cmd) {
    switch (cmd) { case "add": break; case "sub": break; }
  }
}
"#;
    let r = extract_source("Parser.cs", src).unwrap();
    assert!(
        !r.nodes
            .iter()
            .any(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_message")),
        "non-websocket switch must not produce ws messages"
    );
}

#[cfg(feature = "lang-javascript")]
#[test]
fn js_ws_endpoint_from_url_and_id_matches_csharp() {
    // The client URL path keys the same endpoint node id as the C# server's
    // AddWebSocketService path (make_id is case-insensitive on the path).
    let src = b"function openSocket(port) {\n  const url = `ws://127.0.0.1:${port}/feed`;\n  client = new WebSocket(url);\n}\n";
    let r = extract_source("socketClient.js", src).unwrap();
    let ep = r
        .nodes
        .iter()
        .find(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_endpoint"))
        .expect("a ws_endpoint node");
    assert_eq!(ep.label, "ws /feed");
    assert!(r
        .edges
        .iter()
        .any(|e| e.relation == "calls_service" && e.target == ep.id));
    // Same boundary id as the C# server (case-insensitive path via make_id).
    let cs = br#"class S { void Start() { Server.AddWebSocketService<Feed>("/feed"); } }"#;
    let rc = extract_source("S.cs", cs).unwrap();
    let cs_ep = rc
        .nodes
        .iter()
        .find(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_endpoint"))
        .unwrap();
    assert_eq!(
        ep.id, cs_ep.id,
        "client and server meet at one endpoint node"
    );
}

#[cfg(feature = "lang-javascript")]
#[test]
fn js_socketio_emit_and_on() {
    // socket.io custom events: emit -> client message, on -> server handler.
    // Reserved lifecycle events (connection/disconnect) are skipped.
    let src = b"import io from 'socket.io-client';\nfunction wire(socket) {\n  socket.on('price_update', updatePrice);\n  socket.emit('start_job', {});\n  socket.on('connect', noop);\n}\n";
    let r = extract_source("rt.js", src).unwrap();
    let labels: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_message"))
        .map(|n| n.label.as_str())
        .collect();
    assert!(labels.contains(&"ws #price_update"), "{labels:?}");
    assert!(labels.contains(&"ws #start_job"), "{labels:?}");
    assert!(
        !labels.contains(&"ws #connect"),
        "reserved socket.io event excluded: {labels:?}"
    );
}

#[cfg(feature = "lang-python")]
#[test]
fn python_socketio_handler_and_emit() {
    let src = b"import socketio\nsio = socketio.Server()\n\n@sio.on('join_room')\ndef on_join(sid, data):\n    sio.emit('room_joined', data)\n";
    let r = extract_source("rt.py", src).unwrap();
    let labels: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_message"))
        .map(|n| n.label.as_str())
        .collect();
    assert!(labels.contains(&"ws #join_room"), "{labels:?}");
    assert!(labels.contains(&"ws #room_joined"), "{labels:?}");
    // Direction (2026-07 audit F4): @sio.on is the SERVER side (handled_by),
    // sio.emit the CLIENT side (calls_service) -- a role swap must fail here.
    let node_id = |lbl: &str| {
        r.nodes
            .iter()
            .find(|n| n.label == lbl)
            .map(|n| n.id.clone())
            .unwrap()
    };
    let join = node_id("ws #join_room");
    let joined = node_id("ws #room_joined");
    assert!(
        r.edges
            .iter()
            .any(|e| e.relation == "handled_by" && e.source == join),
        "@sio.on('join_room') is a handler"
    );
    assert!(
        r.edges
            .iter()
            .any(|e| e.relation == "calls_service" && e.target == joined),
        "sio.emit('room_joined') is a sender"
    );
}

// ---------------------------------------------------------------------------
// 2026-07 audit fixes (docs/superpowers/plans/2026-07-02-crosslang-fixes.md)
// ---------------------------------------------------------------------------

/// A1: an HTTP response body like `res.send({ type: 'success' })` in a file
/// with no WebSocket usage must not mint a ws_message boundary.
#[cfg(feature = "lang-typescript")]
#[test]
fn http_res_send_type_is_not_a_ws_message() {
    let src = b"const express = require('express');\nconst app = express();\napp.post('/api/ping', (req, res) => {\n  res.send({ type: 'success' });\n});\n";
    let r = extract_source("api_server.js", src).unwrap();
    let ws_nodes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_message"))
        .map(|n| n.label.as_str())
        .collect();
    assert!(
        ws_nodes.is_empty(),
        "no ws_message node from an HTTP response body: {ws_nodes:?}"
    );
}

/// A1 positive control: the same send shape in a real WebSocket file is kept.
#[cfg(feature = "lang-typescript")]
#[test]
fn ws_send_in_ws_file_still_detected() {
    let src = b"const socket = new WebSocket('ws://host/feed');\nfunction subscribe() {\n  socket.send({ type: 'subscribe' });\n}\n";
    let r = extract_source("client.js", src).unwrap();
    let ws_nodes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_message"))
        .map(|n| n.label.as_str())
        .collect();
    assert!(
        ws_nodes.contains(&"ws #subscribe"),
        "ws send in a ws file still detected: {ws_nodes:?}"
    );
}

/// A2: clap's `Command::new("myapp")` CLI-builder idiom is not a subprocess.
#[cfg(feature = "lang-rust")]
#[test]
fn clap_command_builder_is_not_subprocess() {
    let src = b"use clap::{Arg, Command};\n\npub fn build_cli() -> Command {\n    Command::new(\"myapp\").arg(Arg::new(\"verbose\"))\n}\n";
    let r = extract_source("cli.rs", src).unwrap();
    assert!(
        invoked(&r).is_empty(),
        "clap builder must not mint an invokes edge: {:?}",
        invoked(&r)
    );
}

/// A2 positive controls: real process spawns still detected.
#[cfg(feature = "lang-rust")]
#[test]
fn std_and_tokio_process_command_detected() {
    let src = b"use std::process::Command;\n\nfn run() {\n    Command::new(\"mytool\").status().unwrap();\n}\n";
    let r = extract_source("run.rs", src).unwrap();
    assert!(
        invoked(&r).contains(&"mytool".to_string()),
        "{:?}",
        invoked(&r)
    );

    let src2 = b"use tokio::process::Command;\n\nasync fn run2() {\n    Command::new(\"mytool2\").status().await.unwrap();\n}\n";
    let r2 = extract_source("run2.rs", src2).unwrap();
    assert!(
        invoked(&r2).contains(&"mytool2".to_string()),
        "{:?}",
        invoked(&r2)
    );
}

/// A3: Express's 1-arg settings getter `app.get('port')` is not a route.
#[cfg(feature = "lang-typescript")]
#[test]
fn express_settings_getter_is_not_a_route() {
    let src = b"const express = require('express');\nconst app = express();\napp.set('port', 3000);\nconst port = app.get('port');\napp.listen(port);\n";
    let r = extract_source("settings.js", src).unwrap();
    let routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    assert!(
        routes.is_empty(),
        "settings getter minted a route: {routes:?}"
    );
}

/// A4: a Java native method with an underscore in its name joins the C export
/// that mangles that underscore as `_1` -- both sides key to `jni:do_work`.
#[cfg(all(feature = "lang-java", feature = "lang-c"))]
#[test]
fn jni_underscored_method_joins_both_sides() {
    let java = b"package pkg;\n\npublic class NativeLib {\n    public native void do_work();\n}\n";
    let rj = extract_source("NativeLib.java", java).unwrap();
    let jni_java: Vec<&str> = rj
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("jni_symbol"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(jni_java, vec!["jni:do_work"], "java side keys the raw name");

    let c = b"#include <jni.h>\n\nJNIEXPORT void JNICALL Java_pkg_NativeLib_do_1work(JNIEnv *env, jobject obj) {\n}\n";
    let rc = extract_source("jni_impl.c", c).unwrap();
    let jni_c: Vec<&str> = rc
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("jni_symbol"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(
        jni_c,
        vec!["jni:do_work"],
        "C side demangles _1 back to an underscore"
    );
}

/// A4: an overloaded C export (`__` + signature suffix) still keys the method.
#[cfg(feature = "lang-c")]
#[test]
fn jni_overloaded_export_demangles_method() {
    let c = b"#include <jni.h>\n\nJNIEXPORT void JNICALL Java_pkg_Cls_send__Ljava_lang_String_2(JNIEnv *e, jobject o, jstring s) {\n}\n";
    let rc = extract_source("jni_ov.c", c).unwrap();
    let jni_c: Vec<&str> = rc
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("jni_symbol"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(jni_c, vec!["jni:send"], "signature suffix dropped");
}

/// A5: a C# event subscription in a DIFFERENT file from the `event` declaration
/// still mints the channel handler side (`pub.Changed += OnChanged`).
#[cfg(feature = "lang-csharp")]
#[test]
fn cs_event_subscribe_cross_file() {
    let src = b"using System;\n\npublic class SubscriberOther {\n    public void Wire(Publisher pub) {\n        pub.Changed += OnChanged;\n    }\n    private void OnChanged(object s, EventArgs e) {}\n}\n";
    let r = extract_source("SubscriberOther.cs", src).unwrap();
    let ev = r
        .nodes
        .iter()
        .find(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("event_channel"))
        .expect("event channel node from a cross-file subscribe");
    assert_eq!(ev.label, "event #Changed");
    assert!(
        r.edges
            .iter()
            .any(|e| e.relation == "handled_by" && e.source == ev.id),
        "subscriber side is handled_by"
    );
}

/// A5 guards: arithmetic `+=` shapes never mint event channels.
#[cfg(feature = "lang-csharp")]
#[test]
fn cs_arithmetic_plus_equals_not_an_event() {
    let src = b"public class Basket {\n    private int total;\n    public void Add(Item item, int n) {\n        total += item.Price;\n        item.Count += n;\n        this.total += n;\n    }\n}\n";
    let r = extract_source("Basket.cs", src).unwrap();
    let evs: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("event_channel"))
        .map(|n| n.label.as_str())
        .collect();
    assert!(
        evs.is_empty(),
        "arithmetic += minted event channels: {evs:?}"
    );
}

/// A6: `<Name>Stub(` inside gRPC codegen files is not a client call site.
#[cfg(feature = "lang-python")]
#[test]
fn grpc_stub_in_pb2_codegen_not_client() {
    let src = b"import grpc\n\nclass GreeterStub(object):\n    def __init__(self, channel):\n        self.SayHello = channel.unary_unary('/Greeter/SayHello')\n\ndef make():\n    return GreeterStub(None)\n";
    let r = extract_source("greeter_pb2_grpc.py", src).unwrap();
    assert!(
        !r.edges.iter().any(|e| e.relation == "calls_service"),
        "codegen file must not mint gRPC client edges"
    );
}

/// A7: relative `./`/`../` client paths normalize to the joinable rooted path.
#[cfg(feature = "lang-typescript")]
#[test]
fn relative_fetch_path_normalized() {
    let src = b"export function rel() {\n  return fetch('./api/x');\n}\nexport function rel2() {\n  return fetch('../api/x');\n}\n";
    let r = extract_source("relative.js", src).unwrap();
    let routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(
        routes,
        vec!["/api/x"],
        "dot segments stripped, both calls key one route"
    );
}

/// A8: a URL built by string concatenation keys a `{param}` template, not a
/// truncated (wrong) collection path.
#[cfg(feature = "lang-typescript")]
#[test]
fn concat_url_becomes_template() {
    let src = b"import axios from 'axios';\nexport function getUser(id) {\n  return axios.get('/users/' + id);\n}\nexport function getItem(itemId) {\n  return fetch('/api/items/' + itemId);\n}\n";
    let r = extract_source("concat.js", src).unwrap();
    let mut routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    routes.sort();
    assert_eq!(routes, vec!["/api/items/{param}", "/users/{param}"]);
}

/// A9: `fetch(url, { method: 'POST' })` records POST, not GET.
#[cfg(feature = "lang-typescript")]
#[test]
fn fetch_options_method_respected() {
    let src = b"export function createThing(body) {\n  return fetch('/api/ping', { method: 'POST', body: JSON.stringify(body) });\n}\n";
    let r = extract_source("postfetch.js", src).unwrap();
    let e = r
        .edges
        .iter()
        .find(|e| e.relation == "calls_service")
        .expect("client edge");
    assert_eq!(e.context.as_deref(), Some("POST"));
}

/// A9: every entry of a Flask `methods=[...]` list is recorded.
#[cfg(feature = "lang-python")]
#[test]
fn flask_methods_list_all_recorded() {
    let src = b"from flask import Flask\napp = Flask(__name__)\n\n@app.route(\"/thing\", methods=[\"PUT\", \"POST\"])\ndef update_thing():\n    return \"\"\n";
    let r = extract_source("app.py", src).unwrap();
    let e = r
        .edges
        .iter()
        .find(|e| e.relation == "handled_by")
        .expect("handler edge");
    assert_eq!(e.context.as_deref(), Some("PUT,POST"));
}

/// A9: both handlers of a chained axum route method get handled_by edges.
#[cfg(feature = "lang-rust")]
#[test]
fn axum_chained_second_method_handler_linked() {
    let src = b"use axum::{routing::get, Router};\n\nasync fn list() -> String { String::new() }\nasync fn create() -> String { String::new() }\n\nfn app() -> Router {\n    Router::new()\n        .route(\"/things\", get(list).post(create))\n        .route(\"/other\", get(list))\n}\n";
    let r = extract_source("main.rs", src).unwrap();
    let handled: Vec<(String, String)> = r
        .edges
        .iter()
        .filter(|e| e.relation == "handled_by")
        .map(|e| {
            let tgt = r.nodes.iter().find(|n| n.id == e.target).unwrap();
            (tgt.label.clone(), e.context.clone().unwrap_or_default())
        })
        .collect();
    assert!(
        handled.contains(&("list()".to_string(), "GET".to_string())),
        "{handled:?}"
    );
    assert!(
        handled.contains(&("create()".to_string(), "POST".to_string())),
        "chained .post(create) handler linked: {handled:?}"
    );
    assert_eq!(
        handled.len(),
        3,
        "no cross-route bleed from the chain scan: {handled:?}"
    );
}

/// A9: Go's PostForm/Head helpers record proper HTTP methods.
#[cfg(feature = "lang-go")]
#[test]
fn go_postform_method_is_post() {
    let src = b"package main\n\nimport \"net/http\"\n\nfunc submit() {\n\thttp.PostForm(\"http://svc/api/form\", nil)\n\thttp.Head(\"http://svc/api/thing\")\n}\n";
    let r = extract_source("m.go", src).unwrap();
    let ctxs: Vec<&str> = r
        .edges
        .iter()
        .filter(|e| e.relation == "calls_service")
        .filter_map(|e| e.context.as_deref())
        .collect();
    assert!(ctxs.contains(&"POST svc"), "{ctxs:?}");
    assert!(ctxs.contains(&"HEAD svc"), "{ctxs:?}");
    assert!(!ctxs.iter().any(|c| c.starts_with("POSTFORM")), "{ctxs:?}");
}

/// B1: route identity distinguishes what `make_id` folding used to merge...
#[cfg(feature = "lang-typescript")]
#[test]
fn route_id_distinguishes_literal_from_template() {
    let src = b"const express = require('express');\nconst app = express();\napp.get('/users/id', (req, res) => res.json({}));\napp.get('/users/:id', (req, res) => res.json({}));\napp.get('/a-b', (req, res) => res.json({}));\napp.get('/a/b', (req, res) => res.json({}));\n";
    let r = extract_source("server.js", src).unwrap();
    let mut routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    routes.sort();
    assert_eq!(
        routes,
        vec!["/a-b", "/a/b", "/users/:id", "/users/id"],
        "four distinct routes stay four nodes"
    );
}

/// B1: ...while equivalent templates across frameworks share ONE node.
#[cfg(all(
    feature = "lang-typescript",
    feature = "lang-rust",
    feature = "lang-python"
))]
#[test]
fn route_id_merges_equivalent_templates() {
    let js =
        b"const express = require('express');\nconst app = express();\napp.get('/users/:id', h);\n";
    let rs = b"use axum::{routing::get, Router};\nasync fn get_user() -> String { String::new() }\nfn app() -> Router { Router::new().route(\"/users/{id}\", get(get_user)) }\n";
    let py = b"from flask import Flask\napp = Flask(__name__)\n\n@app.route(\"/users/<int:id>\")\ndef get_user():\n    return \"\"\n";
    let rj = extract_source("s.js", js).unwrap();
    let rr = extract_source("s.rs", rs).unwrap();
    let rp = extract_source("s.py", py).unwrap();
    let id_of = |r: &synaptic_extract::ExtractionResult| {
        r.nodes
            .iter()
            .find(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
            .map(|n| n.id.clone())
            .expect("route node")
    };
    let (a, b, c) = (id_of(&rj), id_of(&rr), id_of(&rp));
    assert_eq!(a, b, "express :id and axum {{id}} share a node id");
    assert_eq!(b, c, "flask <int:id> shares it too");
}

/// B1: a catch-all template is NOT the same endpoint as a single-segment one.
#[cfg(feature = "lang-rust")]
#[test]
fn route_id_distinguishes_catchall_from_param() {
    let src = b"use axum::{routing::get, Router};\nasync fn by_name() -> String { String::new() }\nasync fn by_path() -> String { String::new() }\nfn app() -> Router {\n    Router::new()\n        .route(\"/files/{name}\", get(by_name))\n        .route(\"/files/{*path}\", get(by_path))\n}\n";
    let r = extract_source("files.rs", src).unwrap();
    let routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(
        routes.len(),
        2,
        "param and catch-all stay distinct: {routes:?}"
    );
}

/// B2: template-literal URLs produce parameterized client routes.
#[cfg(feature = "lang-typescript")]
#[test]
fn fetch_template_literal_with_param() {
    let src = "export function loadUser(id) {\n  return fetch(`/api/users/${id}`);\n}\nexport function close(id) {\n  return axios.post(`/api/orders/${id}/close`);\n}\nimport axios from 'axios';\n".as_bytes();
    let r = extract_source("client.js", src).unwrap();
    let mut routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    routes.sort();
    assert_eq!(
        routes,
        vec!["/api/orders/{param}/close", "/api/users/{param}"]
    );
    let post = r
        .edges
        .iter()
        .find(|e| e.relation == "calls_service" && e.context.as_deref() == Some("POST"))
        .expect("axios.post template kept its method");
    assert!(post.source_file.contains("client.js"));
}

/// B2: a leading `${BASE}` hole is dropped; the literal path remains.
#[cfg(feature = "lang-typescript")]
#[test]
fn fetch_template_leading_base() {
    let src = "const API_BASE = '/api';\nexport function orders(id) {\n  return fetch(`${API_BASE}/orders/${id}`);\n}\nexport function opaque(url) {\n  return fetch(`${url}`);\n}\n".as_bytes();
    let r = extract_source("client2.js", src).unwrap();
    let routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(
        routes,
        vec!["/orders/{param}"],
        "leading base hole dropped; all-hole template skipped"
    );
}

/// B2: Python f-string clients key the path with holes as params.
#[cfg(feature = "lang-python")]
#[test]
fn py_fstring_client() {
    let src = b"import requests\n\ndef fetch_user(host, uid):\n    return requests.get(f\"http://{host}/api/users/{uid}\")\n";
    let r = extract_source("c.py", src).unwrap();
    let routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(
        routes,
        vec!["/api/users/{param}"],
        "authority hole stripped, path hole is a param"
    );
}

/// B3: a client call through a same-file string constant resolves one hop.
#[cfg(feature = "lang-typescript")]
#[test]
fn js_const_url_resolved() {
    let src = b"const USERS_URL = '/api/users';\nexport function loadUsers() {\n  return fetch(USERS_URL);\n}\nexport function createUser(u) {\n  return axios.post(USERS_URL, u);\n}\nimport axios from 'axios';\n";
    let r = extract_source("consts.js", src).unwrap();
    let routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(routes, vec!["/api/users"], "one route via the const");
    let methods: Vec<&str> = r
        .edges
        .iter()
        .filter(|e| e.relation == "calls_service")
        .filter_map(|e| e.context.as_deref())
        .collect();
    assert!(
        methods.contains(&"GET") && methods.contains(&"POST"),
        "{methods:?}"
    );
}

/// B3: same for Python module-level constants.
#[cfg(feature = "lang-python")]
#[test]
fn py_const_url_resolved() {
    let src = b"import requests\n\nAPI_URL = \"http://svc/api/x\"\n\ndef poll():\n    return requests.get(API_URL)\n";
    let r = extract_source("cc.py", src).unwrap();
    let routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(routes, vec!["/api/x"]);
}

/// B3 guard: a rebound identifier is ambiguous and resolves to nothing.
#[cfg(feature = "lang-typescript")]
#[test]
fn js_rebound_const_not_resolved() {
    let src = b"let url = '/api/a';\nurl = '/api/b';\nexport function load() {\n  return fetch(url);\n}\n";
    let r = extract_source("rebound.js", src).unwrap();
    let routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    assert!(
        routes.is_empty(),
        "rebound name must not resolve: {routes:?}"
    );
}

/// B4: FastAPI APIRouter prefixes compose into the route key.
#[cfg(feature = "lang-python")]
#[test]
fn fastapi_router_prefix_composed() {
    let src = b"from fastapi import FastAPI, APIRouter\napp = FastAPI()\nrouter = APIRouter(prefix=\"/api2\")\n\n@router.get(\"/users\")\ndef list_users2():\n    return []\n\napp.include_router(router)\n";
    let r = extract_source("app.py", src).unwrap();
    let routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(routes, vec!["/api2/users"], "constructor prefix applied");
}

/// B4: include_router's own prefix composes ahead of the constructor's.
#[cfg(feature = "lang-python")]
#[test]
fn fastapi_include_router_prefix() {
    let src = b"from fastapi import FastAPI, APIRouter\napp = FastAPI()\nr = APIRouter(prefix=\"/items\")\n\n@r.get(\"/all\")\ndef list_all():\n    return []\n\napp.include_router(r, prefix=\"/v1\")\n";
    let r = extract_source("app2.py", src).unwrap();
    let routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(routes, vec!["/v1/items/all"]);
}

/// B4: Flask Blueprint url_prefix composes.
#[cfg(feature = "lang-python")]
#[test]
fn flask_blueprint_url_prefix() {
    let src = b"from flask import Blueprint\nbp = Blueprint('admin', __name__, url_prefix='/adm')\n\n@bp.route(\"/health\")\ndef health():\n    return \"ok\"\n";
    let r = extract_source("admin.py", src).unwrap();
    let routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(routes, vec!["/adm/health"]);
}

/// B4: Express `app.use('/api4', router)` mounts compose (same file).
#[cfg(feature = "lang-typescript")]
#[test]
fn express_use_mount_composed() {
    let src = b"const express = require('express');\nconst app = express();\nconst router = express.Router();\n\nrouter.get('/users', (req, res) => res.json([]));\napp.use('/api4', router);\napp.get('/health', (req, res) => res.send('ok'));\n";
    let r = extract_source("server.js", src).unwrap();
    let mut routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    routes.sort();
    assert_eq!(
        routes,
        vec!["/api4/users", "/health"],
        "mounted router route gets the mount prefix; direct app route does not"
    );
}

/// B4: axum `.nest("/api", sub)` composes onto the nested router's routes.
#[cfg(feature = "lang-rust")]
#[test]
fn axum_nest_composed() {
    let src = b"use axum::{routing::get, Router};\n\nasync fn list_x() -> String { String::new() }\n\nfn app() -> Router {\n    let sub = Router::new().route(\"/x\", get(list_x));\n    Router::new().nest(\"/api\", sub)\n}\n";
    let r = extract_source("nest.rs", src).unwrap();
    let routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(routes, vec!["/api/x"]);
}

/// B5: a bare authority/root URL is too generic to key a route node.
#[cfg(all(feature = "lang-python", feature = "lang-typescript"))]
#[test]
fn bare_root_url_not_a_route() {
    let py = b"import requests\n\ndef ping():\n    return requests.get(\"http://host:8080\")\n";
    let rp = extract_source("ping.py", py).unwrap();
    let js = b"export function root() {\n  return fetch('/');\n}\n";
    let rj = extract_source("root.js", js).unwrap();
    for (r, what) in [(&rp, "authority-only URL"), (&rj, "bare / path")] {
        let routes: Vec<&str> = r
            .nodes
            .iter()
            .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
            .map(|n| n.label.as_str())
            .collect();
        assert!(routes.is_empty(), "{what} minted a route: {routes:?}");
    }
}

/// B5: the URL's authority is preserved as edge context next to the method.
#[cfg(feature = "lang-python")]
#[test]
fn authority_recorded_in_context() {
    let src = b"import requests\n\ndef repos():\n    return requests.get(\"https://api.github.com/repos\")\n";
    let r = extract_source("gh.py", src).unwrap();
    let e = r
        .edges
        .iter()
        .find(|e| e.relation == "calls_service")
        .expect("client edge");
    assert_eq!(e.context.as_deref(), Some("GET api.github.com"));
}

/// B6: axios instances with a baseURL compose it into every call.
#[cfg(feature = "lang-typescript")]
#[test]
fn axios_create_baseurl_composed() {
    let src = b"import axios from 'axios';\nconst api = axios.create({ baseURL: '/api' });\nexport function loadUsers() {\n  return api.get('/users');\n}\n";
    let r = extract_source("inst.js", src).unwrap();
    let routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(routes, vec!["/api/users"]);
}

/// B6: httpx.Client(base_url=...) and requests.Session() instances count.
#[cfg(feature = "lang-python")]
#[test]
fn py_instance_clients_detected() {
    let src = b"import httpx\nimport requests\n\nc = httpx.Client(base_url=\"https://svc/api\")\ns = requests.Session()\n\ndef via_httpx():\n    return c.get(\"/x\")\n\ndef via_session():\n    return s.get(\"http://svc/api/y\")\n";
    let r = extract_source("inst.py", src).unwrap();
    let mut routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    routes.sort();
    assert_eq!(routes, vec!["/api/x", "/api/y"]);
}

/// C1: ASP.NET Core minimal-API routes are detected with their methods.
#[cfg(feature = "lang-csharp")]
#[test]
fn csharp_minimal_api_routes() {
    let src = b"var builder = WebApplication.CreateBuilder(args);\nvar app = builder.Build();\n\napp.MapGet(\"/api5/users\", () => new string[] {});\napp.MapPost(\"/api5/users\", (User u) => u);\n\napp.Run();\n";
    let r = extract_source("Program.cs", src).unwrap();
    let mut got: Vec<(String, String)> = r
        .edges
        .iter()
        .filter(|e| e.relation == "handled_by")
        .map(|e| {
            let src_n = r.nodes.iter().find(|n| n.id == e.source).unwrap();
            (src_n.label.clone(), e.context.clone().unwrap_or_default())
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            ("/api5/users".to_string(), "GET".to_string()),
            ("/api5/users".to_string(), "POST".to_string())
        ]
    );
}

/// C1: attribute-routed controllers compose the class [Route] prefix, with
/// [controller] substituted from the class name.
#[cfg(feature = "lang-csharp")]
#[test]
fn csharp_controller_attribute_routes() {
    let src = b"using Microsoft.AspNetCore.Mvc;\n\n[Route(\"api/[controller]\")]\npublic class UsersController : ControllerBase {\n    [HttpGet]\n    public IEnumerable<string> List() { return new string[] {}; }\n\n    [HttpPost(\"bulk\")]\n    public IActionResult Bulk() { return Ok(); }\n}\n";
    let r = extract_source("UsersController.cs", src).unwrap();
    let mut routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    routes.sort();
    assert_eq!(routes, vec!["/api/users", "/api/users/bulk"]);
}

/// C1: HttpClient verb calls are client edges (gated on HttpClient in file).
#[cfg(feature = "lang-csharp")]
#[test]
fn csharp_httpclient_calls() {
    let src = b"using System.Net.Http;\n\npublic class UsersClient {\n    private readonly HttpClient http = new HttpClient();\n    public async Task<string> GetUsers() {\n        return await http.GetStringAsync(\"http://svc/api/users\");\n    }\n    public async Task Create(User u) {\n        await http.PostAsync(\"http://svc/api/users\", null);\n    }\n}\n";
    let r = extract_source("UsersClient.cs", src).unwrap();
    let ctxs: Vec<&str> = r
        .edges
        .iter()
        .filter(|e| e.relation == "calls_service")
        .filter_map(|e| e.context.as_deref())
        .collect();
    assert!(ctxs.contains(&"GET svc"), "{ctxs:?}");
    assert!(ctxs.contains(&"POST svc"), "{ctxs:?}");
}

/// C1: P/Invoke imports bind to the native library.
#[cfg(feature = "lang-csharp")]
#[test]
fn csharp_dllimport_binds_native() {
    let src = b"using System.Runtime.InteropServices;\n\npublic static class Native {\n    [DllImport(\"mylib\")]\n    public static extern int add(int a, int b);\n\n    [LibraryImport(\"otherlib\")]\n    public static partial int sub(int a, int b);\n}\n";
    let r = extract_source("Interop.cs", src).unwrap();
    let libs: Vec<&str> = r
        .edges
        .iter()
        .filter(|e| e.relation == "binds_native")
        .filter_map(|e| r.nodes.iter().find(|n| n.id == e.target))
        .map(|n| n.label.as_str())
        .collect();
    assert!(libs.contains(&"mylib"), "{libs:?}");
    assert!(libs.contains(&"otherlib"), "{libs:?}");
}

/// C1: Process.Start / ProcessStartInfo are subprocess invocations.
#[cfg(feature = "lang-csharp")]
#[test]
fn csharp_process_start_invokes() {
    let src = b"using System.Diagnostics;\n\npublic class Runner {\n    public void Go() {\n        Process.Start(\"mytool\");\n        var psi = new ProcessStartInfo(\"othertool\", \"-x\");\n        Process.Start(psi);\n    }\n}\n";
    let r = extract_source("Runner.cs", src).unwrap();
    let t = invoked(&r);
    assert!(t.contains(&"mytool".to_string()), "{t:?}");
    assert!(t.contains(&"othertool".to_string()), "{t:?}");
}

/// C2: Spring mapping annotations are routes; class-level @RequestMapping
/// composes as a prefix.
#[cfg(feature = "lang-java")]
#[test]
fn spring_mapping_routes() {
    let src = b"import org.springframework.web.bind.annotation.*;\n\n@RestController\n@RequestMapping(\"/api6\")\npublic class UserApi {\n    @GetMapping(\"/users\")\n    public String users() { return \"[]\"; }\n\n    @PostMapping(\"/users\")\n    public String create() { return \"{}\"; }\n\n    @RequestMapping(value = \"/legacy\", method = RequestMethod.POST)\n    public String legacy() { return \"\"; }\n}\n";
    let r = extract_source("UserApi.java", src).unwrap();
    let mut got: Vec<(String, String)> = r
        .edges
        .iter()
        .filter(|e| e.relation == "handled_by")
        .map(|e| {
            let src_n = r.nodes.iter().find(|n| n.id == e.source).unwrap();
            (src_n.label.clone(), e.context.clone().unwrap_or_default())
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            ("/api6/legacy".to_string(), "POST".to_string()),
            ("/api6/users".to_string(), "GET".to_string()),
            ("/api6/users".to_string(), "POST".to_string()),
        ]
    );
}

/// C2: JAX-RS @GET + @Path routes.
#[cfg(feature = "lang-java")]
#[test]
fn jaxrs_routes() {
    let src = b"import javax.ws.rs.*;\n\n@Path(\"/things\")\npublic class ThingResource {\n    @GET\n    @Path(\"/all\")\n    public String all() { return \"[]\"; }\n}\n";
    let r = extract_source("ThingResource.java", src).unwrap();
    let routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(routes, vec!["/things/all"]);
}

/// C2: Java HTTP clients (RestTemplate, java.net.http, OkHttp, Retrofit).
#[cfg(feature = "lang-java")]
#[test]
fn java_http_clients() {
    let src = b"import org.springframework.web.client.RestTemplate;\nimport java.net.http.HttpRequest;\nimport okhttp3.Request;\n\npublic class Clients {\n    public void go(RestTemplate restTemplate) {\n        restTemplate.getForObject(\"http://svc/api/users\", String.class);\n        HttpRequest.newBuilder().uri(URI.create(\"https://svc/api/things\")).build();\n        new Request.Builder().url(\"http://svc/api/ok\").build();\n    }\n}\npublic interface UserService {\n    @retrofit2.http.GET(\"/api/retro\")\n    Call<User> getUser();\n}\n";
    let r = extract_source("Clients.java", src).unwrap();
    let mut routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    routes.sort();
    assert_eq!(
        routes,
        vec!["/api/ok", "/api/retro", "/api/things", "/api/users"]
    );
}

/// C2: ProcessBuilder / Runtime.exec are subprocess invocations.
#[cfg(feature = "lang-java")]
#[test]
fn java_processbuilder_invokes() {
    let src = b"public class Runner {\n    public void go() throws Exception {\n        new ProcessBuilder(\"mytool\", \"-x\").start();\n        Runtime.getRuntime().exec(\"othertool -y\");\n    }\n}\n";
    let r = extract_source("Runner.java", src).unwrap();
    let t = invoked(&r);
    assert!(t.contains(&"mytool".to_string()), "{t:?}");
    assert!(t.contains(&"othertool".to_string()), "{t:?}");
}

/// C3: Laravel Route:: registrations and PHP HTTP clients.
#[cfg(feature = "lang-php")]
#[test]
fn laravel_routes_and_php_clients() {
    let src = b"<?php
// routing via Laravel Route facade; HTTP via GuzzleHttp + Http facade

Route::get('/api8/users', [UserController::class, 'index']);
Route::post('/api8/users', [UserController::class, 'store']);

function callOther() {
    $client = new Client();
    $client->get('http://svc/api/users');
    Http::post('http://svc/api/things');
}
";
    let r = extract_source("routes.php", src).unwrap();
    let mut handled: Vec<(String, String)> = r
        .edges
        .iter()
        .filter(|e| e.relation == "handled_by")
        .map(|e| {
            let s = r.nodes.iter().find(|n| n.id == e.source).unwrap();
            (s.label.clone(), e.context.clone().unwrap_or_default())
        })
        .collect();
    handled.sort();
    assert_eq!(
        handled,
        vec![
            ("/api8/users".to_string(), "GET".to_string()),
            ("/api8/users".to_string(), "POST".to_string()),
        ]
    );
    let called: Vec<&str> = r
        .edges
        .iter()
        .filter(|e| e.relation == "calls_service")
        .filter_map(|e| r.nodes.iter().find(|n| n.id == e.target))
        .map(|n| n.label.as_str())
        .collect();
    assert!(called.contains(&"/api/users"), "{called:?}");
    assert!(called.contains(&"/api/things"), "{called:?}");
}

/// C3: Sinatra/Rails route declarations and Ruby HTTP clients.
#[cfg(feature = "lang-ruby")]
#[test]
fn ruby_routes_and_clients() {
    let sinatra = b"require 'sinatra'\n\nget '/api9/items' do\n  '[]'\nend\n\npost '/api9/items' do\n  '{}'\nend\n";
    let rs = extract_source("app.rb", sinatra).unwrap();
    let mut handled: Vec<(String, String)> = rs
        .edges
        .iter()
        .filter(|e| e.relation == "handled_by")
        .map(|e| {
            let s = rs.nodes.iter().find(|n| n.id == e.source).unwrap();
            (s.label.clone(), e.context.clone().unwrap_or_default())
        })
        .collect();
    handled.sort();
    assert_eq!(
        handled,
        vec![
            ("/api9/items".to_string(), "GET".to_string()),
            ("/api9/items".to_string(), "POST".to_string()),
        ]
    );

    let clients = b"require 'net/http'\nrequire 'faraday'\nrequire 'httparty'\n\ndef fetch_all\n  Net::HTTP.get(URI(\"http://svc/api/a\"))\n  Faraday.get(\"http://svc/api/b\")\n  HTTParty.post(\"http://svc/api/c\")\nend\n";
    let rc = extract_source("client.rb", clients).unwrap();
    let mut called: Vec<&str> = rc
        .edges
        .iter()
        .filter(|e| e.relation == "calls_service")
        .filter_map(|e| rc.nodes.iter().find(|n| n.id == e.target))
        .map(|n| n.label.as_str())
        .collect();
    called.sort();
    assert_eq!(called, vec!["/api/a", "/api/b", "/api/c"]);
}

/// C4: Go framework routes (gin/echo-style VERB methods, chi's Get, gorilla's
/// .Methods suffix) with named-handler linking.
#[cfg(feature = "lang-go")]
#[test]
fn go_framework_routes() {
    let src = b"package main\n\nimport (\n\t\"github.com/gin-gonic/gin\"\n\t\"github.com/gorilla/mux\"\n)\n\nfunc listUsers(c *gin.Context) {}\nfunc submit(w http.ResponseWriter, r *http.Request) {}\n\nfunc main() {\n\tr := gin.Default()\n\tr.GET(\"/api7/users\", listUsers)\n\n\tm := mux.NewRouter()\n\tm.HandleFunc(\"/api7/forms\", submit).Methods(\"POST\")\n}\n";
    let r = extract_source("main.go", src).unwrap();
    let mut got: Vec<(String, String, String)> = r
        .edges
        .iter()
        .filter(|e| e.relation == "handled_by")
        .map(|e| {
            let s = r.nodes.iter().find(|n| n.id == e.source).unwrap();
            let t = r.nodes.iter().find(|n| n.id == e.target).unwrap();
            (
                s.label.clone(),
                t.label.clone(),
                e.context.clone().unwrap_or_default(),
            )
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            (
                "/api7/forms".to_string(),
                "submit()".to_string(),
                "POST".to_string()
            ),
            (
                "/api7/users".to_string(),
                "listUsers()".to_string(),
                "GET".to_string()
            ),
        ]
    );
}

/// C5: Express-style routes on ANY receiver (with the handler-arg guard), and
/// NestJS controller decorators with prefix composition.
#[cfg(feature = "lang-typescript")]
#[test]
fn node_framework_routes() {
    let express = b"const express = require('express');\nconst api = express.Router();\napi.get('/users', (req, res) => res.json([]));\nconst fastify = require('fastify')();\nfastify.post('/things', async () => ({}));\nconst metrics = new Map();\nconst v = metrics.get('name');\n";
    let re = extract_source("anyrecv.js", express).unwrap();
    let mut routes: Vec<&str> = re
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    routes.sort();
    assert_eq!(
        routes,
        vec!["/things", "/users"],
        "any receiver with a handler arg; Map.get excluded"
    );

    let nest = b"import { Controller, Get, Post } from '@nestjs/common';\n\n@Controller('users')\nexport class UsersController {\n  @Get(':id')\n  findOne(@Param('id') id: string) { return {}; }\n\n  @Post()\n  create(@Body() dto: CreateUserDto) { return {}; }\n}\n";
    let rn = extract_source("users.controller.ts", nest).unwrap();
    let mut nroutes: Vec<(String, String)> = rn
        .edges
        .iter()
        .filter(|e| e.relation == "handled_by")
        .map(|e| {
            let s = rn.nodes.iter().find(|n| n.id == e.source).unwrap();
            (s.label.clone(), e.context.clone().unwrap_or_default())
        })
        .collect();
    nroutes.sort();
    assert_eq!(
        nroutes,
        vec![
            ("/users".to_string(), "POST".to_string()),
            ("/users/:id".to_string(), "GET".to_string()),
        ]
    );
}

/// C6: C/C++ system()/popen() are subprocess invocations.
#[cfg(feature = "lang-c")]
#[test]
fn c_system_popen_invokes() {
    let src = b"#include <stdlib.h>\n#include <stdio.h>\n\nvoid run(void) {\n    system(\"mytool -x\");\n    FILE *p = popen(\"othertool\", \"r\");\n}\n";
    let r = extract_source("run.c", src).unwrap();
    let t = invoked(&r);
    assert!(t.contains(&"mytool".to_string()), "{t:?}");
    assert!(t.contains(&"othertool".to_string()), "{t:?}");
}

/// C6: cffi's dlopen binds like ctypes' CDLL.
#[cfg(feature = "lang-python")]
#[test]
fn python_cffi_dlopen_binds_native() {
    let src =
        b"from cffi import FFI\nffi = FFI()\n\ndef load():\n    return ffi.dlopen(\"libfoo.so\")\n";
    let r = extract_source("c.py", src).unwrap();
    assert_eq!(
        native_target(&r, "cffi").as_deref(),
        Some("libfoo"),
        "cffi dlopen binds the native lib"
    );
}

/// C6: a Rust `#[no_mangle] extern "C"` export and a ctypes call-site on the
/// loaded lib meet at a shared `c_symbol:` sink.
#[cfg(all(feature = "lang-rust", feature = "lang-python"))]
#[test]
fn rust_extern_export_meets_ctypes_call() {
    let rs = b"#[no_mangle]\npub extern \"C\" fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
    let rr = extract_source("ffi.rs", rs).unwrap();
    let rust_sym: Vec<&str> = rr
        .edges
        .iter()
        .filter(|e| e.relation == "binds_native")
        .filter_map(|e| rr.nodes.iter().find(|n| n.id == e.target))
        .map(|n| n.label.as_str())
        .collect();
    assert!(rust_sym.contains(&"c_symbol:add"), "{rust_sym:?}");

    let py = b"import ctypes\n\nlib = ctypes.CDLL(\"./libmath.so\")\n\ndef compute():\n    return lib.add(1, 2)\n";
    let rp = extract_source("m.py", py).unwrap();
    let py_sym: Vec<&str> = rp
        .edges
        .iter()
        .filter(|e| e.relation == "binds_native")
        .filter_map(|e| rp.nodes.iter().find(|n| n.id == e.target))
        .map(|n| n.label.as_str())
        .collect();
    assert!(
        py_sym.contains(&"c_symbol:add"),
        "ctypes call-site links the symbol sink too: {py_sym:?}"
    );
}

/// C7: Django URLconf, aiohttp server routes, and aiohttp/urllib clients.
#[cfg(feature = "lang-python")]
#[test]
fn django_aiohttp_tornado_routes_and_clients() {
    let dj = b"from django.urls import path, re_path\nfrom . import views\n\nurlpatterns = [\n    path(\"users/\", views.list_users),\n    re_path(r\"^items/$\", views.list_items),\n]\n";
    let rd = extract_source("urls.py", dj).unwrap();
    let mut droutes: Vec<&str> = rd
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    droutes.sort();
    assert_eq!(droutes, vec!["/items", "/users"], "django URLconf routes");

    let aio = b"from aiohttp import web\n\nasync def list_things(request):\n    return web.json_response([])\n\napp = web.Application()\napp.router.add_get(\"/things\", list_things)\n";
    let ra = extract_source("srv.py", aio).unwrap();
    let aroutes: Vec<&str> = ra
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(aroutes, vec!["/things"], "aiohttp add_get route");

    let cli = b"import aiohttp\nimport urllib.request\n\nasync def go(session):\n    await session.get(\"http://svc/api/d\")\n    urllib.request.urlopen(\"http://svc/api/e\")\n";
    let rc = extract_source("cli.py", cli).unwrap();
    let mut called: Vec<&str> = rc
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    called.sort();
    assert_eq!(
        called,
        vec!["/api/d", "/api/e"],
        "aiohttp session + urllib clients"
    );
}

/// C8: Python gRPC SERVERS (Servicer subclass / add_to_server) join the
/// grpc:<service> boundary that tonic/py clients already use.
#[cfg(feature = "lang-python")]
#[test]
fn py_grpc_server_detected() {
    let src = b"import grpc\nimport greeter_pb2_grpc\n\nclass Greeter(greeter_pb2_grpc.GreeterServicer):\n    def SayHello(self, request, context):\n        return None\n\ndef serve():\n    server = grpc.server(None)\n    greeter_pb2_grpc.add_GreeterServicer_to_server(Greeter(), server)\n";
    let r = extract_source("server.py", src).unwrap();
    let svc = r
        .nodes
        .iter()
        .find(|n| n.label == "grpc:greeter")
        .expect("grpc service boundary");
    assert!(
        r.edges
            .iter()
            .any(|e| e.relation == "handled_by" && e.source == svc.id),
        "server side attaches via handled_by"
    );
}

/// C8: Go gRPC server registration + client stub constructor.
#[cfg(feature = "lang-go")]
#[test]
fn go_grpc_both_sides() {
    let src = b"package main\n\nimport (\n\t\"google.golang.org/grpc\"\n\tpb \"example/greeter\"\n)\n\ntype server struct{}\n\nfunc main() {\n\ts := grpc.NewServer()\n\tpb.RegisterGreeterServer(s, &server{})\n\n\tconn, _ := grpc.Dial(\"localhost:50051\")\n\tclient := pb.NewGreeterClient(conn)\n\t_ = client\n}\n";
    let r = extract_source("main.go", src).unwrap();
    let svc = r
        .nodes
        .iter()
        .find(|n| n.label == "grpc:greeter")
        .expect("grpc service boundary");
    let rels: Vec<&str> = r
        .edges
        .iter()
        .filter(|e| e.source == svc.id || e.target == svc.id)
        .map(|e| e.relation.as_str())
        .collect();
    assert!(rels.contains(&"handled_by"), "{rels:?}");
    assert!(rels.contains(&"calls_service"), "{rels:?}");
}

/// C8: Java gRPC ImplBase server + blocking stub client.
#[cfg(feature = "lang-java")]
#[test]
fn java_grpc_both_sides() {
    let src = b"import io.grpc.stub.StreamObserver;\n\npublic class GreeterImpl extends GreeterGrpc.GreeterImplBase {\n    public void sayHello(HelloRequest req, StreamObserver<HelloReply> obs) {}\n}\n\nclass Client {\n    void call() {\n        var stub = GreeterGrpc.newBlockingStub(channel);\n    }\n}\n";
    let r = extract_source("Greeter.java", src).unwrap();
    let svc = r
        .nodes
        .iter()
        .find(|n| n.label == "grpc:greeter")
        .expect("grpc service boundary");
    let rels: Vec<&str> = r
        .edges
        .iter()
        .filter(|e| e.source == svc.id || e.target == svc.id)
        .map(|e| e.relation.as_str())
        .collect();
    assert!(rels.contains(&"handled_by"), "{rels:?}");
    assert!(rels.contains(&"calls_service"), "{rels:?}");
}

/// C8: C# Grpc.Net service base + client.
#[cfg(feature = "lang-csharp")]
#[test]
fn csharp_grpc_both_sides() {
    let src = b"using Grpc.Core;\n\npublic class GreeterService : Greeter.GreeterBase {\n    public override Task<HelloReply> SayHello(HelloRequest r, ServerCallContext c) { return null; }\n}\n\npublic class Caller {\n    public void Go(GrpcChannel channel) {\n        var client = new Greeter.GreeterClient(channel);\n    }\n}\n";
    let r = extract_source("Greeter.cs", src).unwrap();
    let svc = r
        .nodes
        .iter()
        .find(|n| n.label == "grpc:greeter")
        .expect("grpc service boundary");
    let rels: Vec<&str> = r
        .edges
        .iter()
        .filter(|e| e.source == svc.id || e.target == svc.id)
        .map(|e| e.relation.as_str())
        .collect();
    assert!(rels.contains(&"handled_by"), "{rels:?}");
    assert!(rels.contains(&"calls_service"), "{rels:?}");
}

/// C8: JS @grpc/grpc-js client constructor.
#[cfg(feature = "lang-typescript")]
#[test]
fn js_grpc_client_detected() {
    let src = b"const grpc = require('@grpc/grpc-js');\nconst { GreeterClient } = require('./greeter_grpc_pb');\n\nconst client = new GreeterClient('localhost:50051', grpc.credentials.createInsecure());\nfunction hello(cb) {\n  client.sayHello(request, cb);\n}\n";
    let r = extract_source("client.js", src).unwrap();
    let svc = r
        .nodes
        .iter()
        .find(|n| n.label == "grpc:greeter")
        .expect("grpc service boundary");
    assert!(
        r.edges
            .iter()
            .any(|e| e.relation == "calls_service" && e.target == svc.id),
        "JS client attaches via calls_service"
    );
}

/// C9: Kafka producers/consumers meet at a `queue #<topic>` boundary.
#[cfg(all(feature = "lang-python", feature = "lang-typescript"))]
#[test]
fn kafka_producer_consumer_pair() {
    let py = b"from kafka import KafkaProducer\n\nproducer = KafkaProducer()\n\ndef publish_order(order):\n    producer.send(\"orders\", order)\n";
    let rp = extract_source("producer.py", py).unwrap();
    let q = rp
        .nodes
        .iter()
        .find(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("queue_topic"))
        .expect("queue boundary from kafka producer");
    assert_eq!(q.label, "queue #orders");
    assert!(
        rp.edges
            .iter()
            .any(|e| e.relation == "calls_service" && e.target == q.id),
        "producer side is calls_service"
    );

    let js = b"const { Kafka } = require('kafkajs');\nconst kafka = new Kafka({ brokers: ['b:9092'] });\nconst consumer = kafka.consumer({ groupId: 'g' });\n\nasync function run() {\n  await consumer.subscribe({ topic: 'orders' });\n}\n";
    let rj = extract_source("consumer.js", js).unwrap();
    let qj = rj
        .nodes
        .iter()
        .find(|n| n.label == "queue #orders")
        .expect("kafkajs consumer joins the same-keyed boundary");
    assert!(
        rj.edges
            .iter()
            .any(|e| e.relation == "handled_by" && e.source == qj.id),
        "consumer side is handled_by"
    );
}

/// C9: RabbitMQ pika/amqplib publish+consume pairs.
#[cfg(all(feature = "lang-python", feature = "lang-typescript"))]
#[test]
fn rabbitmq_pika_amqplib_pair() {
    let py = b"import pika\n\ndef publish(channel, body):\n    channel.basic_publish(exchange='', routing_key='jobs', body=body)\n\ndef consume(channel):\n    channel.basic_consume(queue='jobs', on_message_callback=handle)\n";
    let rp = extract_source("rabbit.py", py).unwrap();
    let labels: Vec<&str> = rp
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("queue_topic"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(labels, vec!["queue #jobs"], "one boundary for both sides");
    let rels: Vec<&str> = rp
        .edges
        .iter()
        .filter(|e| e.context.as_deref() == Some("queue"))
        .map(|e| e.relation.as_str())
        .collect();
    assert!(
        rels.contains(&"calls_service") && rels.contains(&"handled_by"),
        "{rels:?}"
    );

    let js = b"const amqp = require('amqplib');\n\nasync function send(ch, msg) {\n  ch.sendToQueue('jobs', Buffer.from(msg));\n}\nasync function recv(ch) {\n  await ch.consume('jobs', handle);\n}\n";
    let rj = extract_source("rabbit.js", js).unwrap();
    assert!(
        rj.nodes.iter().any(|n| n.label == "queue #jobs"),
        "amqplib joins the same key"
    );
}

/// C9: NATS and Redis pub/sub (token-gated so generic publish/subscribe stay out).
#[cfg(feature = "lang-python")]
#[test]
fn nats_redis_pubsub_gated() {
    let src = b"import nats\nimport redis\n\nasync def go(nc, r, pubsub):\n    await nc.publish(\"events.user\", b\"x\")\n    await nc.subscribe(\"events.user\", cb=handle)\n    r.publish(\"chan1\", \"m\")\n    pubsub.subscribe(\"chan1\")\n";
    let r = extract_source("bus.py", src).unwrap();
    let mut labels: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("queue_topic"))
        .map(|n| n.label.as_str())
        .collect();
    labels.sort();
    assert_eq!(labels, vec!["queue #chan1", "queue #events.user"]);

    // No library token -> no queue boundary from generic pub/sub names.
    let plain =
        b"def wire(bus):\n    bus.publish(\"whatever\", 1)\n    bus.subscribe(\"whatever\", cb)\n";
    let rp = extract_source("plainbus.py", plain).unwrap();
    assert!(
        !rp.nodes
            .iter()
            .any(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("queue_topic")),
        "ungated generic publish/subscribe must not mint queue nodes"
    );
}

/// C9: Celery task producers (send_task / .delay) meet the worker's @app.task.
#[cfg(feature = "lang-python")]
#[test]
fn celery_task_pair() {
    let src = b"from celery import Celery\napp = Celery('proj')\n\n@app.task\ndef refresh_cache(key):\n    return key\n\ndef kick():\n    app.send_task(\"tasks.refresh_cache\")\n    refresh_cache.delay(\"k\")\n";
    let r = extract_source("tasks.py", src).unwrap();
    let q = r
        .nodes
        .iter()
        .find(|n| n.label == "queue #task:refresh_cache")
        .expect("celery task boundary");
    let ins = r
        .edges
        .iter()
        .filter(|e| e.relation == "calls_service" && e.target == q.id)
        .count();
    let outs = r
        .edges
        .iter()
        .filter(|e| e.relation == "handled_by" && e.source == q.id)
        .count();
    assert!(ins >= 1, "producer side present");
    assert_eq!(outs, 1, "worker task registered once");
}

/// C9: Java Spring-Kafka listener + KafkaTemplate producer.
#[cfg(feature = "lang-java")]
#[test]
fn java_kafka_listener_and_template() {
    let src = b"import org.springframework.kafka.annotation.KafkaListener;\nimport org.springframework.kafka.core.KafkaTemplate;\n\npublic class OrderEvents {\n    private KafkaTemplate<String, String> kafkaTemplate;\n\n    @KafkaListener(topics = \"orders\")\n    public void onOrder(String msg) {}\n\n    public void emit(String o) {\n        kafkaTemplate.send(\"orders\", o);\n    }\n}\n";
    let r = extract_source("OrderEvents.java", src).unwrap();
    let q = r
        .nodes
        .iter()
        .find(|n| n.label == "queue #orders")
        .expect("kafka topic boundary");
    let rels: Vec<&str> = r
        .edges
        .iter()
        .filter(|e| e.source == q.id || e.target == q.id)
        .map(|e| e.relation.as_str())
        .collect();
    assert!(rels.contains(&"handled_by"), "{rels:?}");
    assert!(rels.contains(&"calls_service"), "{rels:?}");
}

/// C10: SFC script blocks participate in crosslang scanning.
#[cfg(all(feature = "lang-vue", feature = "lang-typescript"))]
#[test]
fn vue_sfc_fetch_detected() {
    let src = b"<script setup>\nasync function loadUsers() {\n  const r = await fetch('/api/users');\n  return r.json();\n}\n</script>\n<template><div @click=\"loadUsers\">users</div></template>\n";
    let r = extract_source("Users.vue", src).unwrap();
    let routes: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(
        routes,
        vec!["/api/users"],
        "fetch inside <script setup> detected"
    );
}

/// C10: Svelte and Astro too.
#[cfg(all(
    feature = "lang-svelte",
    feature = "lang-astro",
    feature = "lang-typescript"
))]
#[test]
fn svelte_astro_fetch_detected() {
    let sv = b"<script>\n  export async function load() {\n    return fetch('/api/sv');\n  }\n</script>\n<main>x</main>\n";
    let rs = extract_source("Load.svelte", sv).unwrap();
    assert!(
        rs.nodes.iter().any(|n| n.label == "/api/sv"),
        "svelte script fetch"
    );

    let astro = b"---\nconst data = await fetch('/api/astro');\n---\n<html><body>x</body></html>\n";
    let ra = extract_source("Page.astro", astro).unwrap();
    assert!(
        ra.nodes.iter().any(|n| n.label == "/api/astro"),
        "astro frontmatter fetch"
    );
}

/// C11: shell curl/wget are HTTP clients; runner lines invoke in-repo scripts.
#[cfg(feature = "lang-bash")]
#[test]
fn bash_curl_and_runners() {
    let src = b"#!/bin/bash\nset -e\n\ncurl -X POST http://localhost:8000/api/users\ncurl https://svc/api/things\nwget https://svc/api/blob\npython tools/mytool.py --flag\n./scripts/deploy.sh\n";
    let r = extract_source("run.sh", src).unwrap();
    let mut called: Vec<(String, String)> = r
        .edges
        .iter()
        .filter(|e| e.relation == "calls_service")
        .map(|e| {
            let t = r.nodes.iter().find(|n| n.id == e.target).unwrap();
            (t.label.clone(), e.context.clone().unwrap_or_default())
        })
        .collect();
    called.sort();
    assert_eq!(
        called,
        vec![
            ("/api/blob".to_string(), "GET svc".to_string()),
            ("/api/things".to_string(), "GET svc".to_string()),
            ("/api/users".to_string(), "POST localhost:8000".to_string()),
        ]
    );
    let t = invoked(&r);
    assert!(t.contains(&"mytool.py".to_string()), "{t:?}");
    assert!(t.contains(&"deploy.sh".to_string()), "{t:?}");
}

/// C12: Go gorilla Dial is a ws client; Java @ServerEndpoint is a ws server.
#[cfg(all(feature = "lang-go", feature = "lang-java"))]
#[test]
fn go_java_websocket_endpoints() {
    let go = b"package main\n\nimport \"github.com/gorilla/websocket\"\n\nfunc connect() {\n\tc, _, _ := websocket.DefaultDialer.Dial(\"ws://host/feed\", nil)\n\t_ = c\n}\n";
    let rg = extract_source("ws.go", go).unwrap();
    let ep = rg
        .nodes
        .iter()
        .find(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_endpoint"))
        .expect("go dial endpoint");
    assert_eq!(ep.label, "ws /feed");
    assert!(
        rg.edges
            .iter()
            .any(|e| e.relation == "calls_service" && e.target == ep.id),
        "dial is the client side"
    );

    let java = b"import jakarta.websocket.server.ServerEndpoint;\n\n@ServerEndpoint(\"/feed\")\npublic class FeedSocket {\n    public void onMessage(String m) {}\n}\n";
    let rj = extract_source("FeedSocket.java", java).unwrap();
    let epj = rj
        .nodes
        .iter()
        .find(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_endpoint"))
        .expect("java server endpoint");
    assert!(
        rj.edges
            .iter()
            .any(|e| e.relation == "handled_by" && e.source == epj.id),
        "@ServerEndpoint is the server side"
    );
}

/// C12: a Python file that both serves and connects gets per-site roles.
#[cfg(feature = "lang-python")]
#[test]
fn py_ws_mixed_role_per_site() {
    let src = b"import websockets\n\nasync def serve_feed(handler):\n    await websockets.serve(handler, \"0.0.0.0\", 8765)\n\nasync def relay():\n    async with websockets.connect(\"ws://up/stream\") as ws:\n        pass\n";
    let r = extract_source("relay.py", src).unwrap();
    let ep = r
        .nodes
        .iter()
        .find(|n| n.label == "ws /stream")
        .expect("connect endpoint");
    assert!(
        r.edges
            .iter()
            .any(|e| e.relation == "calls_service" && e.target == ep.id),
        "connect URL is CLIENT even though the file also serves"
    );
}

// --- F1 backfill: previously untested detectors (2026-07 audit) ---

#[cfg(feature = "lang-ruby")]
#[test]
fn ruby_subprocess_and_backticks() {
    let src = b"def deploy\n  system(\"mytool -x\")\n  out = `othertool --list`\n  Open3.capture2(\"third\")\nend\n";
    let r = extract_source("deploy.rb", src).unwrap();
    let t = invoked(&r);
    assert!(t.contains(&"mytool".to_string()), "{t:?}");
    assert!(
        t.contains(&"othertool".to_string()),
        "backtick command: {t:?}"
    );
    assert!(t.contains(&"third".to_string()), "Open3: {t:?}");
}

#[cfg(feature = "lang-php")]
#[test]
fn php_subprocess_detected() {
    let src =
        b"<?php\nfunction deploy() {\n    exec('mytool -x');\n    shell_exec(\"othertool\");\n}\n";
    let r = extract_source("deploy.php", src).unwrap();
    let t = invoked(&r);
    assert!(t.contains(&"mytool".to_string()), "{t:?}");
    assert!(t.contains(&"othertool".to_string()), "{t:?}");
}

#[cfg(feature = "lang-rust")]
#[test]
fn tungstenite_endpoint_roles() {
    let client = b"use tokio_tungstenite::connect_async;\n\nasync fn feed() {\n    let (ws, _) = connect_async(\"ws://host/feed\").await.unwrap();\n}\n";
    let rc = extract_source("wsc.rs", client).unwrap();
    let ep = rc
        .nodes
        .iter()
        .find(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_endpoint"))
        .expect("client endpoint");
    assert!(
        rc.edges
            .iter()
            .any(|e| e.relation == "calls_service" && e.target == ep.id),
        "connect_async URL is the client side"
    );
}

#[cfg(feature = "lang-csharp")]
#[test]
fn cs_clientwebsocket_is_client_role() {
    let src = b"using System.Net.WebSockets;\n\npublic class Feed {\n    public async Task Go(ClientWebSocket ws) {\n        await ws.ConnectAsync(new Uri(\"ws://host/feed\"), CancellationToken.None);\n    }\n}\n";
    let r = extract_source("Feed.cs", src).unwrap();
    let ep = r
        .nodes
        .iter()
        .find(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_endpoint"))
        .expect("endpoint");
    assert!(
        r.edges
            .iter()
            .any(|e| e.relation == "calls_service" && e.target == ep.id),
        "ClientWebSocket URL is the client side"
    );
}

#[cfg(feature = "lang-python")]
#[test]
fn httpx_client_detected() {
    let src = b"import httpx\n\ndef fetch():\n    return httpx.get(\"http://svc/api/h\")\n";
    let r = extract_source("hc.py", src).unwrap();
    assert!(
        r.nodes.iter().any(|n| n.label == "/api/h"),
        "httpx module verb detected"
    );
}

#[cfg(feature = "lang-python")]
#[test]
fn flask_route_defaults_to_get() {
    let src = b"from flask import Flask\napp = Flask(__name__)\n\n@app.route(\"/plain\")\ndef plain():\n    return \"\"\n";
    let r = extract_source("app.py", src).unwrap();
    let e = r
        .edges
        .iter()
        .find(|e| e.relation == "handled_by")
        .expect("handler");
    assert_eq!(e.context.as_deref(), Some("GET"), "no methods kwarg -> GET");
}

#[cfg(feature = "lang-typescript")]
#[test]
fn express_all_verb_detected() {
    let src = b"const express = require('express');\nconst app = express();\napp.all('/anything', (req, res) => res.send('ok'));\n";
    let r = extract_source("s.js", src).unwrap();
    let e = r
        .edges
        .iter()
        .find(|e| e.relation == "handled_by")
        .expect("handler");
    assert_eq!(e.context.as_deref(), Some("ALL"));
}

#[cfg(feature = "lang-typescript")]
#[test]
fn nodegyp_direct_node_require_binds() {
    let src = b"const addon = require('./build/Release/fasthash.node');\nexport function h(x) { return addon.hash(x); }\n";
    let r = extract_source("m.ts", src).unwrap();
    assert_eq!(
        native_target(&r, "node-gyp").as_deref(),
        Some("fasthash"),
        "direct .node require binds the addon"
    );
}

#[cfg(feature = "lang-cpp")]
#[test]
fn cpp_jni_impl_detected() {
    let src = b"#include <jni.h>\n\nextern \"C\" JNIEXPORT jint JNICALL Java_pkg_Calc_add(JNIEnv *env, jobject o, jint a, jint b) {\n    return a + b;\n}\n";
    let r = extract_source("calc.cpp", src).unwrap();
    let syms: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("jni_symbol"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(syms, vec!["jni:add"], "C++ ext runs the JNI impl scan");
}

#[cfg(feature = "lang-typescript")]
#[test]
fn electron_ipc_variants() {
    let src = b"const { ipcMain, ipcRenderer } = require('electron');\n\nipcMain.on('save-file', handleSave);\nipcMain.handleOnce('load-once', handleLoad);\nipcRenderer.send('save-file', data);\nipcRenderer.sendSync('query-state');\n";
    let r = extract_source("ipc.js", src).unwrap();
    let chans: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ipc_channel"))
        .map(|n| n.label.as_str())
        .collect();
    assert!(chans.contains(&"ipc #save-file"), "{chans:?}");
    assert!(chans.contains(&"ipc #load-once"), "{chans:?}");
    assert!(chans.contains(&"ipc #query-state"), "{chans:?}");
}

/// Masking: commented-out routes/commands stay undetected across languages.
#[cfg(all(
    feature = "lang-go",
    feature = "lang-typescript",
    feature = "lang-csharp"
))]
#[test]
fn masking_blanks_comments_across_languages() {
    let go = b"package main\n\n// func h() { exec.Command(\"ghosttool\").Run() }\nfunc real() {}\n";
    let rg = extract_source("m.go", go).unwrap();
    assert!(invoked(&rg).is_empty(), "{:?}", invoked(&rg));

    let js = b"// app.get('/ghost', handler);\nexport function real() {}\n";
    let rj = extract_source("m.js", js).unwrap();
    assert!(
        !rj.nodes.iter().any(|n| n.label == "/ghost"),
        "commented route not detected"
    );

    let cs = b"public class C {\n    // [DllImport(\"ghostlib\")] static extern int g();\n    public void Real() {}\n}\n";
    let rc = extract_source("C.cs", cs).unwrap();
    assert!(
        !rc.edges.iter().any(|e| e.relation == "binds_native"),
        "commented DllImport not detected"
    );
}

/// Re-audit regression (C5 widening): an axios/instance client call WITH a
/// body argument (`axios.post('/x', data)`) must not register a server route.
#[cfg(feature = "lang-typescript")]
#[test]
fn axios_post_with_body_is_not_a_route() {
    let src = b"import axios from 'axios';\nconst api = axios.create({ baseURL: '/api' });\nexport function createUser(u) {\n  return axios.post('/users', u);\n}\nexport function createThing(t) {\n  return api.post('/things', { payload: t });\n}\n";
    let r = extract_source("client.js", src).unwrap();
    assert!(
        !r.edges.iter().any(|e| e.relation == "handled_by"),
        "client calls with body args are not server routes"
    );
    let called: Vec<&str> = r
        .edges
        .iter()
        .filter(|e| e.relation == "calls_service")
        .filter_map(|e| r.nodes.iter().find(|n| n.id == e.target))
        .map(|n| n.label.as_str())
        .collect();
    assert!(called.contains(&"/users"), "client side intact: {called:?}");
    assert!(
        called.contains(&"/api/things"),
        "instance client intact: {called:?}"
    );
}

// --- Re-audit wave 2 (adversarial verification findings) ---

/// W1 [HIGH]: client wrappers with identifier body args are not servers.
#[cfg(feature = "lang-typescript")]
#[test]
fn client_wrapper_post_with_ident_body_not_a_route() {
    let src = b"export class UserService {\n  constructor(private http: HttpClient) {}\n  createUser(payload: User) {\n    return this.http.post('/api/users', payload);\n  }\n}\n";
    let r = extract_source("user.service.ts", src).unwrap();
    assert!(
        !r.edges.iter().any(|e| e.relation == "handled_by"),
        "Angular-style http.post must not register a server route"
    );
}

/// W1b: real Express/fastify routes on custom receivers still work (server
/// framework token present); without any server token, no route.
#[cfg(feature = "lang-typescript")]
#[test]
fn custom_receiver_route_needs_server_token() {
    let with_token = b"const express = require('express');\nconst api = express.Router();\napi.get('/users', (req, res) => res.json([]));\n";
    let r1 = extract_source("api.js", with_token).unwrap();
    assert!(
        r1.edges.iter().any(|e| e.relation == "handled_by"),
        "custom receiver + express token -> route"
    );
    let without = b"const api = makeClient();\napi.get('/users', handleResponse);\n";
    let r2 = extract_source("client.js", without).unwrap();
    assert!(
        !r2.edges.iter().any(|e| e.relation == "handled_by"),
        "custom receiver without any server-framework token -> no route"
    );
}

/// W2 [HIGH]: Ruby request-spec verbs are not server routes.
#[cfg(feature = "lang-ruby")]
#[test]
fn ruby_spec_verbs_not_routes() {
    let spec = b"require 'rails_helper'\n\nRSpec.describe 'Users' do\n  it 'lists users' do\n    get \"/users\"\n  end\nend\n";
    let r = extract_source("spec/requests/users_spec.rb", spec).unwrap();
    assert!(
        !r.edges.iter().any(|e| e.relation == "handled_by"),
        "spec file verbs are client-side test calls"
    );
    let sinatra = b"require 'sinatra'\n\nget '/api9/items' do\n  '[]'\nend\n";
    let rs = extract_source("app.rb", sinatra).unwrap();
    assert!(
        rs.edges.iter().any(|e| e.relation == "handled_by"),
        "sinatra file still registers routes"
    );
}

/// W3 [HIGH]: commented-out curl in shell scripts is not a client call.
#[cfg(feature = "lang-bash")]
#[test]
fn commented_curl_not_detected() {
    let src = b"#!/bin/bash\n# curl -X POST https://svc.internal/api/deploy\necho done\n";
    let r = extract_source("deploy.sh", src).unwrap();
    assert!(
        !r.edges.iter().any(|e| e.relation == "calls_service"),
        "commented curl must be masked"
    );
}

/// W4 [MED]: fetch method window does not bleed into the next call.
#[cfg(feature = "lang-typescript")]
#[test]
fn fetch_method_window_bounded_by_call() {
    let src =
        b"export function two() {\n  fetch('/alpha'); fetch('/beta', { method: 'POST' });\n}\n";
    let r = extract_source("two.js", src).unwrap();
    let ctx_of = |label: &str| {
        let n = r.nodes.iter().find(|n| n.label == label).unwrap();
        r.edges
            .iter()
            .find(|e| e.relation == "calls_service" && e.target == n.id)
            .and_then(|e| e.context.clone())
    };
    assert_eq!(ctx_of("/alpha").as_deref(), Some("GET"), "no bleed");
    assert_eq!(ctx_of("/beta").as_deref(), Some("POST"));
}

/// W5 [MED]: a ws URL in a client file named like server_url stays a client.
#[cfg(feature = "lang-python")]
#[test]
fn ws_server_url_variable_is_still_client() {
    let src = b"import websockets\n\nserver_url = \"ws://gateway.internal/feed\"\n\nasync def listen():\n    async with websockets.connect(server_url) as ws:\n        pass\n";
    let r = extract_source("wsclient.py", src).unwrap();
    let ep = r
        .nodes
        .iter()
        .find(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_endpoint"))
        .expect("endpoint");
    assert!(
        !r.edges
            .iter()
            .any(|e| e.relation == "handled_by" && e.source == ep.id),
        "no server side in a pure client file"
    );
}

/// W6 [MED]: chained axum handlers in ANOTHER file both survive as markers.
#[cfg(feature = "lang-rust")]
#[test]
fn axum_chained_cross_file_handlers_both_recorded() {
    let src = b"use axum::{routing::get, Router};\n\nfn app() -> Router {\n    Router::new().route(\"/multi\", get(list_things).post(create_thing))\n}\n";
    let r = extract_source("routes.rs", src).unwrap();
    let route = r
        .nodes
        .iter()
        .find(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .expect("route node");
    let handlers = route
        .extra
        .get("_route_handlers")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(
        handlers, 2,
        "both chained handler markers kept: {:?}",
        route.extra
    );
}

/// W7 [MED]: `std::process::exit` alone does not re-enable the clap FP.
#[cfg(feature = "lang-rust")]
#[test]
fn clap_with_process_exit_still_not_subprocess() {
    let src = b"use clap::Command;\n\nfn main() {\n    let _ = Command::new(\"myapp\");\n    std::process::exit(2);\n}\n";
    let r = extract_source("main.rs", src).unwrap();
    assert!(invoked(&r).is_empty(), "{:?}", invoked(&r));
    let real = b"use std::process::{Command, Stdio};\n\nfn run() {\n    Command::new(\"mytool\").status().unwrap();\n}\n";
    let r2 = extract_source("run.rs", real).unwrap();
    assert!(
        invoked(&r2).contains(&"mytool".to_string()),
        "brace import form still works"
    );
}

/// W8 [MED]: C# `+=` with property/call RHS is not an event subscribe.
#[cfg(feature = "lang-csharp")]
#[test]
fn cs_property_plus_equals_not_event() {
    let src = b"public class Basket {\n    public void Add(Line line, Stats stats) {\n        this.Total += line.Price;\n        stats.Sum += Convert.ToDecimal(line.Price);\n    }\n}\n";
    let r = extract_source("Basket.cs", src).unwrap();
    assert!(
        !r.nodes
            .iter()
            .any(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("event_channel")),
        "dotted/call RHS is arithmetic, not a handler"
    );
}

/// W9 [MED]: Django include(...) is not a route handler.
#[cfg(feature = "lang-python")]
#[test]
fn django_include_not_a_handler() {
    let src = b"from django.urls import path, include\n\nurlpatterns = [\n    path(\"api/\", include(\"api.urls\")),\n]\n";
    let r = extract_source("urls.py", src).unwrap();
    let bad = r
        .nodes
        .iter()
        .find(|n| n.extra.get("_route_handler").and_then(|v| v.as_str()) == Some("include"));
    assert!(bad.is_none(), "include() is a prefix mount, not a handler");
}

/// W10 [MED]: KV-style `.Get("/path", nil)` in Go is not a route.
#[cfg(feature = "lang-go")]
#[test]
fn go_kv_get_not_a_route() {
    let src = b"package main\n\nimport \"github.com/hashicorp/consul/api\"\n\nfunc read(kv *api.KV) {\n\tpair, _, _ := kv.Get(\"/config/app\", nil)\n\t_ = pair\n}\n";
    let r = extract_source("kv.go", src).unwrap();
    assert!(
        !r.edges.iter().any(|e| e.relation == "handled_by"),
        "consul KV get is not a web route"
    );
}

/// W11 [MED]: server-side bare `/` routes are skipped like client-side.
#[cfg(feature = "lang-typescript")]
#[test]
fn server_bare_root_route_skipped() {
    let src = b"const express = require('express');\nconst app = express();\napp.get('/', (req, res) => res.send('home'));\napp.get('/real', (req, res) => res.send('ok'));\n";
    let r = extract_source("s.js", src).unwrap();
    let labels: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("route"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(
        labels,
        vec!["/real"],
        "bare / too generic to key: {labels:?}"
    );
}

/// W12 [MED]: a 'websocket' string token alone does not make switch arms ws
/// handlers; a real WS API token does.
#[cfg(feature = "lang-typescript")]
#[test]
fn reducer_with_websocket_string_not_ws() {
    let src = b"const TRANSPORT = 'websocket';\nexport function todosReducer(state, action) {\n  switch (action.type) {\n    case \"ADD_TODO\":\n      return state;\n    case \"RESET_ALL\":\n      return [];\n  }\n}\n";
    let r = extract_source("reducer.js", src).unwrap();
    assert!(
        !r.nodes
            .iter()
            .any(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("ws_message")),
        "a string mentioning websocket is not a WS API usage"
    );
    let real = b"const socket = new WebSocket('ws://h/feed');\nsocket.onmessage = (raw) => {\n  switch (JSON.parse(raw).type) {\n    case \"tick\":\n      break;\n  }\n};\n";
    let r2 = extract_source("wsc.js", real).unwrap();
    assert!(
        r2.nodes.iter().any(|n| n.label == "ws #tick"),
        "real WebSocket API still detected"
    );
}

/// W13 [MED]: `.send("str")` on a non-producer receiver in a kafka file is
/// not a queue publish.
#[cfg(feature = "lang-python")]
#[test]
fn kafka_gate_requires_producer_receiver() {
    let src = b"from kafka import KafkaProducer\nimport socket\n\nproducer = KafkaProducer()\nsock = socket.socket()\n\ndef publish(order):\n    producer.send(\"orders\", order)\n\ndef ping():\n    sock.send(\"ping\")\n";
    let r = extract_source("mixed.py", src).unwrap();
    let topics: Vec<&str> = r
        .nodes
        .iter()
        .filter(|n| n.extra.get("_node_type").and_then(|v| v.as_str()) == Some("queue_topic"))
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(topics, vec!["queue #orders"], "{topics:?}");
}

/// W14 [HIGH]: non-ASCII text near matches must not panic byte slicing.
#[cfg(all(
    feature = "lang-typescript",
    feature = "lang-python",
    feature = "lang-rust"
))]
#[test]
fn non_ascii_near_matches_does_not_panic() {
    let js = "export function f() {\n  fetch('/api/x');\n  const junk = \"caf\u{e9}\u{e9}\u{e9} \u{4f60}\u{597d} caf\u{e9}\u{e9}\u{e9} caf\u{e9}\u{e9}\u{e9} caf\u{e9}\u{e9}\u{e9} caf\u{e9}\u{e9}\u{e9} caf\u{e9}\u{e9}\u{e9} caf\u{e9}\u{e9}\u{e9} caf\u{e9}\u{e9}\u{e9} caf\u{e9}\u{e9}\u{e9} caf\u{e9}\u{e9}\u{e9} caf\u{e9}\u{e9}\u{e9} caf\u{e9}\u{e9}\u{e9} caf\u{e9}\u{e9}\u{e9}\";\n}\n".as_bytes();
    let _ = extract_source("na.js", js).unwrap();

    let py = "import websockets\nlabel = \"\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\"\nurl = \"ws://host/feed\"\n\nasync def go():\n    async with websockets.connect(url) as ws:\n        pass\n".as_bytes();
    let _ = extract_source("na.py", py).unwrap();

    let rs = "use axum::{routing::get, Router};\nfn app() -> Router {\n    Router::new().route(\"/x\", get(h)\n}\nstatic S: &str = \"caf\u{e9}".as_bytes();
    let _ = extract_source("na.rs", rs).unwrap();
}
