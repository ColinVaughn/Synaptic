//! Criterion bench for `forecast_changes`: a forecast over a synthetic graph
//! with a dense dependency web, to catch any pathological blow-up in the
//! files->nodes + reverse-impact composition.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use serde_json::Map;
use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};
use synaptic_graph::KnowledgeGraph;
use synaptic_predict::{forecast_changes, forecast_changes_with_index, ForecastOptions};
use synaptic_query::ReverseImpactIndex;

fn edge(s: &str, t: &str, rel: &str) -> Edge {
    Edge {
        source: NodeId(s.into()),
        target: NodeId(t.into()),
        relation: rel.into(),
        confidence: Confidence::Extracted,
        source_file: "x".into(),
        source_location: None,
        confidence_score: None,
        weight: 1.0,
        context: None,
        cross_repo: false,
        extra: Map::new(),
    }
}

/// `n` nodes spread over 50 files; each node depends on a few earlier ones, so
/// reverse-impact walks fan out broadly.
fn synthetic_graph(n: usize) -> KnowledgeGraph {
    let mut nodes = Vec::with_capacity(n);
    let mut links = Vec::new();
    for i in 0..n {
        nodes.push(Node {
            id: NodeId(format!("n{i}")),
            label: format!("symbol_{i}"),
            file_type: FileType::Code,
            source_file: format!("src/mod_{}.rs", i % 50),
            source_location: Some("L1".into()),
            community: Some((i % 10) as u32),
            repo: None,
            extra: Map::new(),
        });
        if i > 0 {
            links.push(edge(&format!("n{i}"), &format!("n{}", i - 1), "calls"));
        }
        if i > 4 {
            links.push(edge(&format!("n{i}"), &format!("n{}", i - 5), "references"));
        }
    }
    KnowledgeGraph::from_graph_data(GraphData {
        directed: true,
        multigraph: false,
        graph: Map::new(),
        nodes,
        links,
        hyperedges: vec![],
        built_at_commit: None,
    })
}

fn bench_forecast(c: &mut Criterion) {
    let kg = synthetic_graph(5000);
    let opts = ForecastOptions::default();
    let changed = vec!["src/mod_0.rs".to_string()];
    // Build-the-adjacency-every-call path (one-shot CLI invocation).
    c.bench_function("forecast_changes_5k_nodes", |b| {
        b.iter(|| forecast_changes(black_box(&kg), black_box(&changed), black_box(&opts)))
    });
    // Prebuilt-index path (long-lived server): the O(edges) reverse-adjacency
    // build is hoisted out of the measured per-request work.
    let rels: Vec<&str> = opts.relations.iter().map(String::as_str).collect();
    let index = ReverseImpactIndex::build(&kg, &rels);
    c.bench_function("forecast_changes_5k_nodes_cached_index", |b| {
        b.iter(|| {
            forecast_changes_with_index(
                black_box(&kg),
                black_box(&index),
                black_box(&changed),
                black_box(&opts),
            )
        })
    });
}

criterion_group!(benches, bench_forecast);
criterion_main!(benches);
