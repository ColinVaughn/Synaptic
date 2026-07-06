//! Criterion benches for the shard store's hot paths: writing a shard
//! (`migrate_into` on a fresh dir) and reading it back (`read_graph_data`,
//! `materialize`). The synthetic graph mimics real extraction output --
//! path-shaped ids, qualified labels, `extra` metadata -- so encoded sizes
//! and compression behavior track the real artifact, not toy strings.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use serde_json::Map;
use std::hint::black_box;
use synaptic_core::{Confidence, Edge, FileType, GraphData, Node, NodeId};
use synaptic_store::{migrate, ShardStore};

const NODES: usize = 20_000;
const EDGES: usize = 30_000;

fn bench_graph() -> GraphData {
    let dirs = ["core", "server", "extract", "workspace", "query"];
    let mut nodes = Vec::with_capacity(NODES);
    for i in 0..NODES {
        let d = dirs[i % dirs.len()];
        let mut extra = Map::new();
        extra.insert(
            "norm_label".into(),
            serde_json::json!(format!("handle_{i}")),
        );
        extra.insert("_origin".into(), serde_json::json!("ast"));
        extra.insert("kind".into(), serde_json::json!("function"));
        nodes.push(Node {
            id: NodeId(format!(
                "crates/synaptic-{d}/src/module_{}.rs::handle_{i}",
                i % 97
            )),
            label: format!("handle_{i}()"),
            file_type: FileType::Code,
            source_file: format!("crates/synaptic-{d}/src/module_{}.rs", i % 97),
            source_location: Some(format!("L{}", (i * 7) % 900 + 1)),
            community: Some((i % 40) as u32),
            repo: None,
            extra,
        });
    }
    let mut links = Vec::with_capacity(EDGES);
    for i in 0..EDGES {
        let s = i % NODES;
        let t = (i * 13 + 1) % NODES;
        let d = dirs[s % dirs.len()];
        links.push(Edge {
            source: nodes[s].id.clone(),
            target: nodes[t].id.clone(),
            relation: if i % 5 == 0 { "imports" } else { "calls" }.into(),
            confidence: Confidence::Extracted,
            source_file: format!("crates/synaptic-{d}/src/module_{}.rs", s % 97),
            source_location: Some(format!("L{}", (i * 11) % 900 + 1)),
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        });
    }
    GraphData {
        directed: true,
        multigraph: false,
        graph: Map::new(),
        nodes,
        links,
        hyperedges: vec![],
        built_at_commit: Some("bench".into()),
    }
}

fn write_bench(c: &mut Criterion) {
    let gd = bench_graph();
    let mut g = c.benchmark_group("store");
    g.sample_size(10);
    g.throughput(Throughput::Elements((NODES + EDGES) as u64));
    g.bench_function("write_20k_nodes_30k_edges", |b| {
        b.iter_batched(
            tempfile::tempdir,
            |dir| {
                let d = dir.as_ref().unwrap().path().join("store");
                let mut store = ShardStore::open(&d).unwrap();
                black_box(migrate::migrate_into(&mut store, &gd).unwrap());
                dir
            },
            BatchSize::PerIteration,
        )
    });

    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join("store");
    let mut store = ShardStore::open(&store_dir).unwrap();
    migrate::migrate_into(&mut store, &gd).unwrap();
    let tag = store.list_shards()[0].tag.clone();
    g.bench_function("read_graph_data_20k_30k", |b| {
        let store = ShardStore::open(&store_dir).unwrap();
        b.iter(|| black_box(store.read_graph_data(&tag).unwrap()))
    });
    g.bench_function("materialize_20k_30k", |b| {
        let store = ShardStore::open(&store_dir).unwrap();
        b.iter(|| black_box(store.materialize(&tag).unwrap()))
    });
    g.finish();
}

criterion_group!(benches, write_bench);
criterion_main!(benches);
