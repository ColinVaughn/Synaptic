//! Criterion benchmarks for the MCP `synaptic-server` tool surface added in
//! the Tier 1 work: `get_source`, `affected`, and the `query_graph` request
//! path. The `query_graph` comparison holds traversal, budget, fullness, and
//! edge inclusion constant, then measures direct text rendering against full
//! MCP dispatch (the same retrieval plus structured serialization/enveloping).
//!
//! Run: `cargo bench -p synaptic-server`

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use serde_json::json;
use std::hint::black_box;
use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};
use synaptic_query::TraversalMode;
use synaptic_server::Server;

const SCALES: [usize; 2] = [1_000, 5_000];
const AFFECTED_SCALES: [usize; 2] = [5_000, 50_000];
const COMPLETION_SCALES: [usize; 2] = [5_000, 50_000];

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

fn star_graph(n: usize) -> GraphData {
    let nodes: Vec<Node> = (0..n).map(node).collect();
    let links = (1..n).map(|i| edge(i, 0)).collect();
    GraphData {
        directed: true,
        multigraph: false,
        graph: serde_json::Map::new(),
        nodes,
        links,
        hyperedges: vec![],
        built_at_commit: None,
    }
}

/// The `query_graph` request path: text-only render vs the full `tools/call`
/// dispatch (text + a typed `structuredContent` mirror). Setup asserts the
/// text is identical so a future default change cannot invalidate the comparison.
fn bench_query_graph_structured(c: &mut Criterion) {
    let mut group = c.benchmark_group("server/query_graph");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    let q = "service handler";
    for &n in &SCALES {
        let mut server = Server::from_graph_data(synthetic_graph(n), None);

        // Text only: the same full, 2,000-token BFS workload as the request below.
        group.bench_with_input(BenchmarkId::new("text_full_direct", n), &n, |b, _| {
            b.iter(|| black_box(server.tool_query_graph(q, TraversalMode::Bfs, 2000, &[])));
        });

        // Full dispatch: one shared retrieval rendered as text + structuredContent.
        let req = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "query_graph", "arguments": {
                "question": q,
                "mode": "bfs",
                "full": true,
                "token_budget": 2000,
                "context_filter": []
            } }
        });
        let expected = server.tool_query_graph(q, TraversalMode::Bfs, 2000, &[]);
        let response = server
            .handle_request(&req)
            .expect("equivalence request must produce a response");
        let actual = response["result"]["content"][0]["text"]
            .as_str()
            .expect("query_graph text result");
        assert_eq!(
            actual, expected,
            "query_graph benchmark workloads diverged at scale {n}"
        );
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

    for &n in &AFFECTED_SCALES {
        let mut server = Server::from_graph_data(synthetic_graph(n), None)
            .with_source_root(dir.path().to_path_buf());

        group.bench_with_input(BenchmarkId::new("affected_depth8", n), &n, |b, _| {
            b.iter(|| black_box(server.tool_affected("n0", 8, &[], 50, true)));
        });

        let affected_req = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "affected", "arguments": {
                "label": "n0", "depth": 8, "limit": 50, "verbose": true
            } }
        });
        group.bench_with_input(
            BenchmarkId::new("affected_full_dispatch_depth8", n),
            &n,
            |b, _| b.iter(|| black_box(server.handle_request(&affected_req))),
        );

        // get_source is independent of graph scale; retain its existing coverage
        // only at the smaller affected scale.
        if n != AFFECTED_SCALES[0] {
            continue;
        }

        group.bench_with_input(BenchmarkId::new("get_source_40", n), &n, |b, _| {
            b.iter(|| black_box(server.tool_get_source("n0", None, None, 40)));
        });
    }
    group.finish();
}

/// `completion/complete` over the full dispatch after the graph-version index
/// has been built during Criterion warm-up. Cover a broad duplicate-heavy
/// match, a miss, and bare-name lookup past leading method punctuation.
fn bench_completion(c: &mut Criterion) {
    let mut group = c.benchmark_group("server/completion");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    let request = |prefix: &str| {
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "completion/complete",
            "params": {
                "ref": { "type": "ref/resource", "uri": "synaptic://node/{label}" },
                "argument": { "name": "label", "value": prefix }
            }
        })
    };
    for &n in &COMPLETION_SCALES {
        let mut server = Server::from_graph_data(synthetic_graph(n), None);
        let broad = request("Serv");
        group.bench_with_input(BenchmarkId::new("broad_match", n), &n, |b, _| {
            b.iter(|| black_box(server.handle_request(&broad)));
        });
        let miss = request("NoSuchLabelPrefix");
        group.bench_with_input(BenchmarkId::new("miss", n), &n, |b, _| {
            b.iter(|| black_box(server.handle_request(&miss)));
        });

        let mut leading_graph = synthetic_graph(n);
        for node in &mut leading_graph.nodes {
            node.label.insert(0, '.');
        }
        let mut leading_server = Server::from_graph_data(leading_graph, None);
        let leading = request("Serv");
        group.bench_with_input(BenchmarkId::new("leading_punctuation", n), &n, |b, _| {
            b.iter(|| black_box(leading_server.handle_request(&leading)));
        });
    }
    group.finish();
}

/// Broad structural search with a small response limit. This isolates whether
/// full MCP dispatch executes SynQL twice and whether node-view projection stops
/// at the configured limit.
fn bench_structural_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("server/structural_search");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    const QUERY: &str = "MATCH (n) RETURN n";
    for &n in &AFFECTED_SCALES {
        let mut server = Server::from_graph_data(synthetic_graph(n), None);
        group.bench_with_input(BenchmarkId::new("direct_limit25", n), &n, |b, _| {
            b.iter(|| black_box(server.tool_structural_search(Some(QUERY), None, None, 25)));
        });

        let req = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "structural_search", "arguments": {
                "query": QUERY, "limit": 25
            } }
        });
        group.bench_with_input(BenchmarkId::new("full_dispatch_limit25", n), &n, |b, _| {
            b.iter(|| black_box(server.handle_request(&req)));
        });
    }
    group.finish();
}

fn bench_remaining_structured_mirrors(c: &mut Criterion) {
    let mut group = c.benchmark_group("server/structured_mirrors");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    const N: usize = 50_000;
    let mut general = Server::from_graph_data(synthetic_graph(N), None);
    for (name, direct) in [
        (
            "list_repos_direct",
            Server::tool_list_repos as fn(&Server) -> String,
        ),
        (
            "graph_stats_direct",
            Server::tool_graph_stats as fn(&Server) -> String,
        ),
    ] {
        group.bench_function(name, |b| b.iter(|| black_box(direct(&general))));
    }
    for name in ["list_repos", "graph_stats"] {
        let request = json!({
            "jsonrpc":"2.0", "id":1, "method":"tools/call",
            "params":{"name":name,"arguments":{}}
        });
        group.bench_function(format!("{name}_full_dispatch"), |b| {
            b.iter(|| black_box(general.handle_request(&request)));
        });
    }

    let mut star = Server::from_graph_data(star_graph(N), None);
    group.bench_function("get_neighbors_star_direct", |b| {
        b.iter(|| black_box(star.tool_get_neighbors("n0", None, false, 50, false)));
    });
    let request = json!({
        "jsonrpc":"2.0", "id":1, "method":"tools/call",
        "params":{"name":"get_neighbors","arguments":{"label":"n0","limit":50}}
    });
    group.bench_function("get_neighbors_star_full_dispatch", |b| {
        b.iter(|| black_box(star.handle_request(&request)));
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_query_graph_structured,
    bench_new_tools,
    bench_completion,
    bench_structural_search,
    bench_remaining_structured_mirrors
);
criterion_main!(benches);
