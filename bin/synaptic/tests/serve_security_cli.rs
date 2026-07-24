use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use serde_json::Value;

const GRAPH: &str =
    r#"{"directed":false,"multigraph":false,"graph":{},"nodes":[],"links":[],"hyperedges":[]}"#;
const GRAPH_SHA256: &str = "645fc87acac992956fb4e0384c5668c4e00cabfc4495d2d94aa1091953d66286";

fn write_graph(root: &std::path::Path) -> std::path::PathBuf {
    let graph = root.join("graph.json");
    fs::write(&graph, GRAPH.as_bytes()).unwrap();
    graph
}

#[test]
fn expected_graph_digest_rejects_tampered_bytes_before_serve() {
    let dir = tempfile::tempdir().unwrap();
    let graph = write_graph(dir.path());

    let assertion = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["serve", "--graph"])
        .arg(&graph)
        .args([
            "--immutable-graph",
            "--expected-graph-sha256",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr);
    assert!(stderr.contains("graph SHA-256 mismatch"), "{stderr}");
}

#[test]
fn expected_graph_digest_parses_the_verified_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let graph = write_graph(dir.path());

    Command::cargo_bin("synaptic")
        .unwrap()
        .args(["serve", "--graph"])
        .arg(&graph)
        .args(["--immutable-graph", "--expected-graph-sha256", GRAPH_SHA256])
        .write_stdin("")
        .assert()
        .success();
}

#[test]
fn expected_graph_digest_requires_immutable_mode() {
    let dir = tempfile::tempdir().unwrap();
    let graph = write_graph(dir.path());

    let assertion = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["serve", "--graph"])
        .arg(&graph)
        .args(["--expected-graph-sha256", GRAPH_SHA256])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr);
    assert!(stderr.contains("--immutable-graph"), "{stderr}");
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn port_zero_ready_file_reports_the_bound_listener_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let graph = write_graph(dir.path());
    let ready = dir.path().join("private").join("ready.json");
    fs::create_dir(ready.parent().unwrap()).unwrap();

    let child = ProcessCommand::new(env!("CARGO_BIN_EXE_synaptic"))
        .args(["serve", "--graph"])
        .arg(&graph)
        .args([
            "--immutable-graph",
            "--expected-graph-sha256",
            GRAPH_SHA256,
            "--http",
            "127.0.0.1:0",
            "--ready-file",
        ])
        .arg(&ready)
        .args(["--api-key", "test-key"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = ChildGuard(child);

    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        if let Some(status) = child.0.try_wait().unwrap() {
            let mut stderr = String::new();
            child
                .0
                .stderr
                .as_mut()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            panic!("server exited before readiness ({status}): {stderr}");
        }
        assert!(Instant::now() < deadline, "ready file was not published");
        thread::sleep(Duration::from_millis(20));
    }

    let document: Value = serde_json::from_slice(&fs::read(&ready).unwrap()).unwrap();
    let addr: SocketAddr = document["address"].as_str().unwrap().parse().unwrap();
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
    assert_ne!(addr.port(), 0);
    assert_eq!(
        document["mcp_url"],
        format!("http://{addr}/mcp"),
        "the ready document must describe the exact bound listener"
    );

    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    stream
        .write_all(
            format!(
                "GET /api/stats HTTP/1.1\r\nHost: {addr}\r\nX-API-Key: test-key\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

#[test]
fn ready_file_requires_http_transport() {
    let dir = tempfile::tempdir().unwrap();
    let graph = write_graph(dir.path());

    let assertion = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["serve", "--graph"])
        .arg(&graph)
        .args(["--ready-file", "ready.json"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr);
    assert!(stderr.contains("--http"), "{stderr}");
}
