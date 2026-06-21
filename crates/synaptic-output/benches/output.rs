//! Criterion benchmarks for `synaptic-output` — the serialization / export
//! hot paths on a synthetic `KnowledgeGraph`.
//!
//!   * `serde` — `to_json_value`, the full graph.json write (`to_graph_data` +
//!     `serde_json::to_string`), and the load path (`from_str` +
//!     `from_graph_data`), across two graph sizes.
//!   * `formats` — each alternate exporter (graphml / cypher / mermaid /
//!     tree-html / svg / html) at one size, to see which is expensive.
//!
//! Run: `cargo bench -p synaptic-output`

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};
use synaptic_graph::KnowledgeGraph;
use synaptic_output::{
    to_cypher_string, to_force3d_html, to_graphml_string, to_html_string, to_json_value,
    to_mermaid_string, to_svg_string, to_tree_html_string,
};

fn node(i: usize) -> Node {
    Node {
        id: NodeId(format!("n{i}")),
        label: format!("Symbol_{i}"),
        file_type: FileType::Code,
        source_file: format!("src/mod_{}.rs", i % 32),
        source_location: Some(format!("L{i}")),
        community: Some((i % 16) as u32),
        repo: None,
        extra: serde_json::Map::new(),
    }
}

fn edge(src: usize, dst: usize, relation: &str) -> Edge {
    Edge {
        source: NodeId(format!("n{src}")),
        target: NodeId(format!("n{dst}")),
        relation: relation.to_string(),
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

/// `n` nodes; a `contains` spine (file → symbol, for the tree/hierarchy
/// exporters) plus a ring + far-link of `references` edges.
fn synth_graph_data(n: usize) -> GraphData {
    let nodes: Vec<Node> = (0..n).map(node).collect();
    let mut links = Vec::with_capacity(n * 3);
    for i in 0..n {
        links.push(edge(i - (i % 8), i, "contains"));
        links.push(edge(i, (i + 1) % n, "references"));
        links.push(edge(i, (i * 7 + 3) % n, "calls"));
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

fn synth_kg(n: usize) -> KnowledgeGraph {
    KnowledgeGraph::from_graph_data(synth_graph_data(n))
}

fn bench_serde(c: &mut Criterion) {
    let mut group = c.benchmark_group("output/serde");
    for &n in &[1_000usize, 5_000] {
        let kg = synth_kg(n);
        let json = serde_json::to_string(&kg.to_graph_data()).expect("serialize");
        group.throughput(Throughput::Elements(n as u64));

        group.bench_with_input(BenchmarkId::new("to_json_value", n), &n, |b, _| {
            b.iter(|| black_box(to_json_value(black_box(&kg))));
        });
        group.bench_with_input(BenchmarkId::new("serialize_string", n), &n, |b, _| {
            b.iter(|| black_box(serde_json::to_string(&kg.to_graph_data()).unwrap()));
        });
        group.bench_with_input(BenchmarkId::new("deserialize", n), &json, |b, j| {
            b.iter(|| {
                let gd: GraphData = serde_json::from_str(black_box(j)).unwrap();
                black_box(KnowledgeGraph::from_graph_data(gd))
            });
        });
    }
    group.finish();
}

fn bench_formats(c: &mut Criterion) {
    let n = 1_000usize;
    let kg = synth_kg(n);

    let mut group = c.benchmark_group("output/formats");
    group.throughput(Throughput::Elements(n as u64));

    group.bench_function("graphml", |b| b.iter(|| black_box(to_graphml_string(&kg))));
    group.bench_function("cypher", |b| b.iter(|| black_box(to_cypher_string(&kg))));
    group.bench_function("mermaid", |b| b.iter(|| black_box(to_mermaid_string(&kg))));
    group.bench_function("tree_html", |b| {
        b.iter(|| black_box(to_tree_html_string(&kg)))
    });
    group.bench_function("html", |b| b.iter(|| black_box(to_html_string(&kg))));
    group.bench_function("force3d", |b| b.iter(|| black_box(to_force3d_html(&kg))));

    group.finish();
}

/// 3D viewer HTML generation (the Rust side: serialize nodes/links + fill the
/// template). Browser-side force-sim/WebGL cost is separate and not measured
/// here. Checks the generation scales linearly.
fn bench_force3d_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("output/force3d_scaling");
    group.sample_size(10);
    for &n in &[500usize, 2_000, 5_000] {
        let kg = synth_kg(n);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| black_box(to_force3d_html(&kg)));
        });
    }
    group.finish();
}

/// SVG layout scales O(n log n) via Barnes–Hut. 500 is apples-to-apples with the
/// old FR cap; 2000/5000 are newly supported. Sample size lowered so the 5000
/// case stays bounded.
fn bench_svg_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("output/svg_scaling");
    group.sample_size(10);
    for &n in &[500usize, 2_000, 5_000] {
        let kg = synth_kg(n);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| black_box(to_svg_string(&kg)));
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_serde,
    bench_formats,
    bench_svg_scaling,
    bench_force3d_scaling
);
criterion_main!(benches);
