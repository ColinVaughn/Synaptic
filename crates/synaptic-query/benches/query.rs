//! Criterion benchmarks for `synaptic-query`.
//!
//! The headline measurement is **H1**: `query_modal` rebuilds the IDF token
//! index + undirected adjacency from a full graph scan on *every* call, whereas
//! a [`QueryIndex`] built once and reused only does the per-query scoring +
//! expansion. The two groups below are the same query run both ways, so the
//! delta is exactly the per-call index-rebuild cost the MCP server used to pay
//! on every request.
//!
//! Run: `cargo bench -p synaptic-query`

use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};
use synaptic_graph::KnowledgeGraph;
use synaptic_query::{query_modal, QueryIndex, TraversalMode};

const SCALES: [usize; 2] = [1_000, 5_000];

fn node(i: usize) -> Node {
    Node {
        id: NodeId(format!("n{i}")),
        // Multi-token labels so tokenize/IDF has real work; deliberate stem reuse.
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

/// `n` nodes, ~2n edges (ring + one "far" link each), as a built graph.
fn synthetic_kg(n: usize) -> KnowledgeGraph {
    let nodes: Vec<Node> = (0..n).map(node).collect();
    let mut links = Vec::with_capacity(n * 2);
    for i in 0..n {
        links.push(edge(i, (i + 1) % n));
        links.push(edge(i, (i * 7 + 3) % n));
    }
    let gd = GraphData {
        directed: false,
        multigraph: false,
        graph: serde_json::Map::new(),
        nodes,
        links,
        hyperedges: vec![],
        built_at_commit: None,
    };
    KnowledgeGraph::from_graph_data(gd)
}

fn bench_query_idf_reuse(c: &mut Criterion) {
    let mut group = c.benchmark_group("query/idf_reuse");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    let q = "service handler";
    for &n in &SCALES {
        let kg = synthetic_kg(n);

        // OLD (pre-H1): each query rebuilds the IDF index + adjacency.
        group.bench_with_input(BenchmarkId::new("per_query_rebuild", n), &n, |b, _| {
            b.iter(|| black_box(query_modal(&kg, q, 50, TraversalMode::Bfs)));
        });

        // NEW (H1): index built once, reused - only scoring + expansion per query.
        let index = QueryIndex::build(&kg);
        group.bench_with_input(BenchmarkId::new("reused_index", n), &n, |b, _| {
            b.iter(|| black_box(index.query(&kg, q, 50, TraversalMode::Bfs)));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_query_idf_reuse);
criterion_main!(benches);
