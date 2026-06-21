use criterion::{criterion_group, criterion_main, Criterion};
use serde_json::Map;
use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, NodeId, NodeKind, Span};
use synaptic_graph::KnowledgeGraph;
use synaptic_synql::run;

fn synthetic(n: usize) -> KnowledgeGraph {
    let mut nodes = Vec::with_capacity(n);
    let mut edges = Vec::new();
    for i in 0..n {
        let mut node = Node {
            id: NodeId(format!("n{i}")),
            label: format!("Sym{i}"),
            file_type: FileType::Code,
            source_file: format!("src/mod_{}.rs", i % 32),
            source_location: Some("L1".into()),
            community: Some((i % 16) as u32),
            repo: None,
            extra: Map::new(),
        };
        node.set_kind(if i % 3 == 0 {
            NodeKind::Class
        } else {
            NodeKind::Function
        });
        node.set_span(Span {
            start_line: 1,
            start_col: 1,
            end_line: 1 + (i % 900) as u32,
            end_col: 1,
        });
        nodes.push(node);
        if i > 0 {
            edges.push(Edge {
                source: NodeId(format!("n{i}")),
                target: NodeId(format!("n{}", i - 1)),
                relation: "calls".into(),
                confidence: Confidence::Extracted,
                source_file: "x.rs".into(),
                source_location: None,
                confidence_score: None,
                weight: 1.0,
                context: None,
                cross_repo: false,
                extra: Map::new(),
            });
        }
    }
    KnowledgeGraph::from_graph_data(GraphData {
        nodes,
        links: edges,
        ..Default::default()
    })
}

fn bench_synql(c: &mut Criterion) {
    let kg = synthetic(2000);
    c.bench_function("synql/property_filter", |b| {
        b.iter(|| {
            run(
                &kg,
                "MATCH (c:class) WHERE c.loc > 500 AND c.fan_out > 0 RETURN c",
            )
        })
    });
    c.bench_function("synql/relationship", |b| {
        b.iter(|| run(&kg, "MATCH (a:function)-[:calls]->(b) RETURN a, b"))
    });
}

criterion_group!(benches, bench_synql);
criterion_main!(benches);
