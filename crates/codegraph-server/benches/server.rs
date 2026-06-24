//! Criterion benchmarks for the MCP `codegraph-server` tool surface added in
//! the Tier 1 work: `get_source`, `affected`, and the `query_graph` request
//! path. The headline measurement is the **structured-output double-compute**:
//! a `tools/call` for `query_graph` now renders the text (one index query) and
//! a typed `structuredContent` mirror (a second index query), so the two groups
//! below isolate that added per-request cost against a single text-only call.
//!
//! Run: `cargo bench -p codegraph-server`

use std::time::Duration;

use codegraph_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};
use codegraph_query::TraversalMode;
use codegraph_server::Server;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use serde_json::json;

const SCALES: [usize; 2] = [1_000, 5_000];

fn node(i: usize) -> Node {
    Node {
        id: NodeId(format!("n{i}")),
        label: format!("Service_{} handler_{}", i % 64, i % 16),
        file_type: FileType::Code,
        source_file: format!("src/mod_{}.rs", i % 32),
        source_location: Some(format!("L{i}")),
        community: Some((i % 8) as u32),
        repo: None,
        extra: serde_json::Map::new(),
    }
}

fn edge(src: usize, dst: usize) -> Edge {
    Edge {
        source: NodeId(format!("n{src}")),
        target: NodeId(format!("n{dst}")),
        relation: "calls".to_string(),
        confidence: Confidence::Extracted,
        source_file: format!("src/mod_{}.rs", src % 32),
        source_location: None,
        confidence_score: None,
        weight: 1.0,
        context: None,
        cross_repo: false,
        extra: serde_json::Map::new(),
    }
}

/// `n` nodes, ~2n edges (ring + one "far" link each), as GraphData.
fn synthetic_graph(n: usize) -> GraphData {
    let nodes: Vec<Node> = (0..n).map(node).collect();
    let mut links = Vec::with_capacity(n * 2);
    for i in 0..n {
        links.push(edge(i, (i + 1) % n));
        links.push(edge(i, (i * 7 + 3) % n));
    }
    GraphData {
        directed: false,
        multigraph: false,
        graph: serde_json::Map::new(),
        nodes,
        links,
        hyperedges: vec![],
        built_at_commit: None,
    }
}

/// The `query_graph` request path: text-only render vs the full `tools/call`
/// dispatch (text + a typed `structuredContent` mirror). The delta is exactly
/// the extra index query the structured output adds per request.
fn bench_query_graph_structured(c: &mut Criterion) {
    let mut group = c.benchmark_group("server/query_graph");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    let q = "service handler";
    for &n in &SCALES {
        let mut server = Server::from_graph_data(synthetic_graph(n), None);

        // Text only: one retrieval (the pre-structured-output cost).
        group.bench_with_input(BenchmarkId::new("text_only", n), &n, |b, _| {
            b.iter(|| black_box(server.tool_query_graph(q, TraversalMode::Bfs, 2000, &[])));
        });

        // Full dispatch: text + structuredContent (two retrievals today).
        let req = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "query_graph", "arguments": { "question": q } }
        });
        group.bench_with_input(BenchmarkId::new("full_dispatch", n), &n, |b, _| {
            b.iter(|| black_box(server.handle_request(&req)));
        });
    }
    group.finish();
}

/// `affected` (reverse-impact BFS) and `get_source` (jailed file read + line
/// window) - the two genuinely new tools that touch their own code paths.
fn bench_new_tools(c: &mut Criterion) {
    let mut group = c.benchmark_group("server/tools");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    // A real source file under a jail root, so get_source does its actual work.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let body: String = (0..500).map(|i| format!("fn line_{i}() {{}}\n")).collect();
    std::fs::write(dir.path().join("src/mod_0.rs"), body).unwrap();

    for &n in &SCALES {
        let server = Server::from_graph_data(synthetic_graph(n), None)
            .with_source_root(dir.path().to_path_buf());

        group.bench_with_input(BenchmarkId::new("affected_depth8", n), &n, |b, _| {
            b.iter(|| black_box(server.tool_affected("n0", 8, &[])));
        });

        group.bench_with_input(BenchmarkId::new("get_source_40", n), &n, |b, _| {
            b.iter(|| black_box(server.tool_get_source("n0", 40)));
        });
    }
    group.finish();
}

/// `completion/complete` over the full dispatch: an O(nodes) label prefix scan
/// plus sort/dedup/cap. Low-frequency autocomplete, but it scans every node.
fn bench_completion(c: &mut Criterion) {
    let mut group = c.benchmark_group("server/completion");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    // "Serv" matches every synthetic label ("Service_..."), the worst case.
    let req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "completion/complete",
        "params": { "argument": { "name": "label", "value": "Serv" } }
    });
    for &n in &SCALES {
        let mut server = Server::from_graph_data(synthetic_graph(n), None);
        group.bench_with_input(BenchmarkId::new("label_prefix", n), &n, |b, _| {
            b.iter(|| black_box(server.handle_request(&req)));
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_query_graph_structured,
    bench_new_tools,
    bench_completion
);
criterion_main!(benches);
