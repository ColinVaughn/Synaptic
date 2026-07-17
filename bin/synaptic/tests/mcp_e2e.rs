//! End-to-end MCP conformance: extract a tiny corpus, launch the real
//! `synaptic serve` binary over stdio, and drive the protocol handshake plus a
//! representative slice of the Tier 1-3 tool surface, asserting the responses.
//! This exercises the actual process (default graph path, default source root,
//! JSON-RPC framing), not just the in-crate dispatcher.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

fn send(stdin: &mut ChildStdin, v: &Value) {
    writeln!(stdin, "{v}").unwrap();
    stdin.flush().unwrap();
}

fn recv(out: &mut BufReader<ChildStdout>) -> Value {
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad json line {line:?}: {e}"))
}

/// Call a tool and return its text content.
fn call_text(
    stdin: &mut ChildStdin,
    out: &mut BufReader<ChildStdout>,
    id: u64,
    name: &str,
    args: Value,
) -> String {
    send(
        stdin,
        &json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}}),
    );
    let resp = recv(out);
    resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text in {name} response: {resp}"))
        .to_string()
}

#[test]
fn mcp_stdio_conformance_over_the_real_binary() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/analysis.py"),
        "def run_analysis(d):\n    return compute_score(d)\n\n\ndef compute_score(d):\n    return sum(d)\n",
    )
    .unwrap();

    // Build the graph with the real CLI.
    assert_cmd::Command::cargo_bin("synaptic")
        .unwrap()
        .arg("extract")
        .arg(root)
        .assert()
        .success();

    // Launch the server on stdio from the repo root (default graph + source root).
    let mut child: Child = Command::new(env!("CARGO_BIN_EXE_synaptic"))
        .current_dir(root)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());

    // Lifecycle gate: normal operations and malformed initialize requests are
    // rejected before a valid handshake.
    send(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":90,"method":"tools/list"}),
    );
    assert_eq!(recv(&mut out)["error"]["code"], -32002);
    send(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":91,"method":"initialize","params":{}}),
    );
    assert_eq!(recv(&mut out)["error"]["code"], -32602);

    let init_params = json!({
        "protocolVersion": "2025-11-25",
        "capabilities": {},
        "clientInfo": {"name": "mcp-e2e", "version": "1.0"}
    });
    send(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params.clone()}),
    );
    let init = recv(&mut out);
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25", "{init}");
    let caps = &init["result"]["capabilities"];
    assert!(
        caps["resources"].get("subscribe").is_none(),
        "stdio has no asynchronous resource-update channel: {caps}"
    );
    assert!(caps["prompts"].is_object(), "prompts cap: {caps}");
    assert!(caps["completions"].is_object(), "completions cap: {caps}");
    assert!(caps["logging"].is_object(), "logging cap: {caps}");

    // The initialize response alone is not Ready; a duplicate initialize is
    // rejected and normal operations wait for notifications/initialized.
    send(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":92,"method":"tools/list"}),
    );
    assert_eq!(recv(&mut out)["error"]["code"], -32002);
    send(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":93,"method":"initialize","params":init_params}),
    );
    assert_eq!(recv(&mut out)["error"]["code"], -32600);
    send(
        &mut stdin,
        &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    );

    // tools/list: every tool annotated read-only.
    send(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    let tl = recv(&mut out);
    let tools = tl["result"]["tools"].as_array().unwrap();
    assert!(
        tools.len() >= 17,
        "expected the full tool set: {}",
        tools.len()
    );
    assert!(
        tools
            .iter()
            .all(|t| t["annotations"]["readOnlyHint"] == true),
        "all tools must be annotated read-only"
    );

    // graph_stats: structured content comes back over the wire.
    send(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"graph_stats","arguments":{}}}),
    );
    let stats = recv(&mut out);
    assert!(
        stats["result"]["structuredContent"]["nodes"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "structured graph_stats: {stats}"
    );

    // get_source: the real function body (validates default source-root jail).
    let src = call_text(
        &mut stdin,
        &mut out,
        4,
        "get_source",
        json!({"label":"run_analysis"}),
    );
    assert!(src.contains("def run_analysis"), "get_source body: {src}");

    // affected: compute_score is called by run_analysis, so changing it affects it.
    let aff = call_text(
        &mut stdin,
        &mut out,
        5,
        "affected",
        json!({"label":"compute_score"}),
    );
    assert!(aff.contains("run_analysis"), "affected dependents: {aff}");

    // find_callers: who calls compute_score.
    let callers = call_text(
        &mut stdin,
        &mut out,
        6,
        "find_callers",
        json!({"label":"compute_score"}),
    );
    assert!(callers.contains("run_analysis"), "callers: {callers}");

    // prompts/list includes the onboarding workflow.
    send(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":7,"method":"prompts/list"}),
    );
    let prompts = recv(&mut out);
    assert!(
        prompts["result"]["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "onboard"),
        "prompts/list: {prompts}"
    );

    // completion/complete suggests the function label by prefix.
    send(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":8,"method":"completion/complete",
                "params":{"ref":{"type":"ref/resource","uri":"synaptic://node/{label}"},
                          "argument":{"name":"label","value":"run"}}}),
    );
    let comp = recv(&mut out);
    let values: Vec<String> = comp["result"]["completion"]["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        values.iter().any(|v| v.starts_with("run_analysis")),
        "completion: {values:?}"
    );

    // resources/templates/list advertises the node template.
    send(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":9,"method":"resources/templates/list"}),
    );
    let templates = recv(&mut out);
    assert!(
        templates["result"]["resourceTemplates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["uriTemplate"] == "synaptic://node/{label}"),
        "templates: {templates}"
    );

    // Clean shutdown: EOF on stdin ends the stdio loop.
    drop(stdin);
    let _ = child.wait();
}
