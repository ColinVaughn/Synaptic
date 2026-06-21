//! Criterion benchmarks for `synaptic-incremental` core merge primitives on
//! synthetic graphs at N = 1,000 / 10,000 nodes.
//!
//!   * `topology` — the no-change fingerprint (sorted ids + edge triples).
//!   * `merge_incremental` — re-merge with ~10% of nodes refreshed (the
//!     "changed a few files" path).
//!   * `union_graphs` — merge-driver union of two graphs.
//!
//! `rebuild` is intentionally not benched here — it needs an on-disk repo
//! (detect + extract + I/O); the extract/graph suites already cover its hot
//! inner stages.
//!
//! Run: `cargo bench -p synaptic-incremental`

use std::collections::HashSet;

use criterion::{
    black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput,
};
use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};
use synaptic_incremental::{merge_incremental, topology, union_graphs};

fn node(i: usize, id_prefix: &str) -> Node {
    Node {
        id: NodeId(format!("{id_prefix}{i}")),
        label: format!("Symbol_{i}"),
        file_type: FileType::Code,
        source_file: format!("src/mod_{}.rs", i % 32),
        source_location: Some(format!("L{i}")),
        community: None,
        repo: None,
        extra: serde_json::Map::new(),
    }
}

fn edge(src: String, dst: String) -> Edge {
    Edge {
        source: NodeId(src),
        target: NodeId(dst),
        relation: "calls".to_string(),
        confidence: Confidence::Extracted,
        source_file: "src/mod_0.rs".to_string(),
        source_location: None,
        confidence_score: None,
        weight: 1.0,
        context: None,
        cross_repo: false,
        extra: serde_json::Map::new(),
    }
}

fn synth_parts(n: usize, id_prefix: &str) -> (Vec<Node>, Vec<Edge>) {
    let nodes = (0..n).map(|i| node(i, id_prefix)).collect();
    let mut edges = Vec::with_capacity(n * 2);
    for i in 0..n {
        edges.push(edge(
            format!("{id_prefix}{i}"),
            format!("{id_prefix}{}", (i + 1) % n),
        ));
        edges.push(edge(
            format!("{id_prefix}{i}"),
            format!("{id_prefix}{}", (i * 7 + 3) % n),
        ));
    }
    (nodes, edges)
}

fn synth_graph_data(n: usize) -> GraphData {
    let (nodes, links) = synth_parts(n, "n");
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

fn bench_incremental(c: &mut Criterion) {
    let mut group = c.benchmark_group("incremental");
    for &n in &[1_000usize, 10_000] {
        let existing = synth_graph_data(n);
        let other = synth_graph_data(n);
        // ~10% "changed": regenerate the first tenth of the nodes/edges by id.
        let changed = (n / 10).max(1);
        let (fresh_nodes, fresh_edges) = {
            let (mut ns, mut es) = synth_parts(n, "n");
            ns.truncate(changed);
            es.truncate(changed * 2);
            (ns, es)
        };
        let no_evict: HashSet<String> = HashSet::new();

        group.throughput(Throughput::Elements(n as u64));

        group.bench_with_input(BenchmarkId::new("topology", n), &n, |b, _| {
            b.iter(|| black_box(topology(black_box(&existing))));
        });

        group.bench_with_input(BenchmarkId::new("merge_incremental", n), &n, |b, _| {
            b.iter_batched(
                || (fresh_nodes.clone(), fresh_edges.clone()),
                |(fn_, fe)| black_box(merge_incremental(&existing, fn_, fe, &no_evict, false)),
                BatchSize::SmallInput,
            );
        });

        group.bench_with_input(BenchmarkId::new("union_graphs", n), &n, |b, _| {
            b.iter_batched(
                || (existing.clone(), other.clone()),
                |(a, b2)| black_box(union_graphs(a, b2)),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_incremental);
criterion_main!(benches);
