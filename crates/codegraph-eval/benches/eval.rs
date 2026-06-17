//! Microbench for the pure scoring core: per-commit forecast + scoring over a
//! moderate graph, and set scoring. The IO replay (worktrees, extraction) is not
//! a meaningful criterion target.

use std::collections::BTreeSet;

use codegraph_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};
use codegraph_eval::{score_sets, Scores};
use codegraph_graph::KnowledgeGraph;
use criterion::{criterion_group, criterion_main, Criterion};
use serde_json::Map;

fn node(id: &str, file: &str) -> Node {
    Node {
        id: NodeId(id.into()),
        label: id.into(),
        file_type: FileType::Code,
        source_file: file.into(),
        source_location: Some("L1".into()),
        community: Some(0),
        repo: None,
        extra: Map::new(),
    }
}

fn edge(s: &str, t: &str) -> Edge {
    Edge {
        source: NodeId(s.into()),
        target: NodeId(t.into()),
        relation: "calls".into(),
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

// A graph where `core` is called by many functions and tests.
fn big_graph() -> KnowledgeGraph {
    let mut nodes = vec![node("core", "src/core.py")];
    let mut edges = Vec::new();
    for i in 0..500 {
        let file = if i % 5 == 0 {
            format!("tests/test_{i}.py")
        } else {
            format!("src/m_{i}.py")
        };
        let id = format!("n{i}");
        nodes.push(node(&id, &file));
        edges.push(edge(&id, "core"));
    }
    KnowledgeGraph::from_graph_data(GraphData {
        directed: true,
        multigraph: false,
        graph: Map::new(),
        nodes,
        links: edges,
        hyperedges: vec![],
        built_at_commit: None,
    })
}

fn bench(c: &mut Criterion) {
    let g = big_graph();
    let changed = vec!["src/core.py".to_string()];
    let edited_tests: BTreeSet<String> = (0..100)
        .filter(|i| i % 5 == 0)
        .map(|i| format!("tests/test_{i}.py"))
        .collect();
    let empty: BTreeSet<String> = BTreeSet::new();
    c.bench_function("score_commit_500_nodes", |b| {
        b.iter(|| {
            codegraph_eval::score_commit(
                "c",
                "p",
                std::hint::black_box(&g),
                std::hint::black_box(&changed),
                std::hint::black_box(&edited_tests),
                std::hint::black_box(&empty),
                3,
            )
        })
    });

    let pred: BTreeSet<String> = (0..200).map(|i| format!("t{i}")).collect();
    let truth: BTreeSet<String> = (0..200)
        .filter(|i| i % 2 == 0)
        .map(|i| format!("t{i}"))
        .collect();
    c.bench_function("score_sets_200", |b| {
        b.iter(|| {
            let _: Scores = score_sets(std::hint::black_box(&pred), std::hint::black_box(&truth));
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
