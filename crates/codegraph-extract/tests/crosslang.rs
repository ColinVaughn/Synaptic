//! Cross-language linkage edges (FFI / subprocess / HTTP), gated on the
//! `cross-language` feature. Each test exercises a detector via the real
//! `extract_source` dispatch, which runs the post-passes.

#![cfg(feature = "cross-language")]
#![allow(unused_imports)]

use codegraph_core::Confidence;
use codegraph_extract::extract_source;

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
fn invoked(r: &codegraph_extract::ExtractionResult) -> Vec<String> {
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
fn native_target(r: &codegraph_extract::ExtractionResult, ctx: &str) -> Option<String> {
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
fn pyo3_registers(r: &codegraph_extract::ExtractionResult, module: &str) -> Vec<String> {
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
    r: &codegraph_extract::ExtractionResult,
    rel: &str,
) -> Option<(String, String, Option<String>)> {
    let e = r.edges.iter().find(|e| e.relation == rel)?;
    let lbl = |id: &codegraph_core::NodeId| {
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
    assert_eq!(ctx.as_deref(), Some("GET"));
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
    assert_eq!(ctx.as_deref(), Some("GET"));
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
    assert_eq!(ctx.as_deref(), Some("POST"));
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
    let target_of = |svc_id: &codegraph_core::NodeId| {
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
fn route_labels(r: &codegraph_extract::ExtractionResult) -> Vec<String> {
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
