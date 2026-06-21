//! Criterion benchmarks for `synaptic-graph` algorithm kernels.
//!
//! Two input modes:
//!   * `scaling/*` — seeded, deterministic synthetic graphs at N = 100 / 1k /
//!     10k nodes, run through each kernel. The point is the *growth curve*:
//!     flat-ish cost/element is fine; an exploding curve flags accidentally
//!     super-linear (O(n^2)) code — the highest-value find in an audit.
//!   * `real/*` — parse this workspace's `.rs` files once, assemble a real
//!     `KnowledgeGraph`, and bench the kernels on its actual shape.
//!
//! Kernels covered: `build_from_parts`, `deduplicate_entities`,
//! `resolve_symbols`, `cluster` (community detection), `analyze`.
//!
//! Run: `cargo bench -p synaptic-graph`

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use criterion::{
    black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput,
};
use synaptic_core::{Confidence, Edge, FileType, ImportRecord, Node, NodeId, RawCall};
use synaptic_graph::{
    analyze, build_from_parts, cluster, deduplicate_entities, find_import_cycles, god_nodes,
    resolve_symbols, suggest_questions, surprising_connections, BuildOptions, ClusterOptions,
    KnowledgeGraph,
};

const SCALES: [usize; 3] = [100, 1_000, 10_000];

// Synthetic, deterministic fixtures

fn node(i: usize) -> Node {
    Node {
        // 64 distinct label stems => deliberate label collisions so dedup's
        // MinHash/LSH near-duplicate detection has real work to do.
        id: NodeId(format!("n{i}")),
        label: format!("func_{}", i % 64),
        file_type: FileType::Code,
        source_file: format!("src/mod_{}.rs", i % 32),
        source_location: Some(format!("L{i}")),
        community: None,
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

/// `n` nodes; ~2n edges: a ring (keeps the graph connected) plus one
/// deterministic "far" link per node (gives community detection structure).
fn synthetic_parts(n: usize) -> (Vec<Node>, Vec<Edge>) {
    let nodes: Vec<Node> = (0..n).map(node).collect();
    let mut edges = Vec::with_capacity(n * 2);
    for i in 0..n {
        edges.push(edge(i, (i + 1) % n));
        edges.push(edge(i, (i * 7 + 3) % n));
    }
    (nodes, edges)
}

fn synthetic_raw_calls(n: usize) -> Vec<RawCall> {
    (0..n)
        .map(|i| RawCall {
            caller: NodeId(format!("n{i}")),
            callee: format!("func_{}", (i * 13 + 1) % 64),
            is_member_call: false,
            source_file: format!("src/mod_{}.rs", i % 32),
            source_location: Some(format!("L{i}")),
            span: None,
        })
        .collect()
}

fn build_opts() -> BuildOptions {
    BuildOptions {
        directed: false,
        root: None,
    }
}

/// Build communities + labels from a `cluster` result, in the shape `analyze`
/// expects.
fn communities_and_labels(
    kg: &KnowledgeGraph,
) -> (BTreeMap<u32, Vec<NodeId>>, BTreeMap<u32, String>) {
    let communities = cluster(kg, &ClusterOptions::default());
    let labels = communities
        .keys()
        .map(|&id| (id, format!("community_{id}")))
        .collect();
    (communities, labels)
}

// Real workspace fixture (parsed once)

struct RealFixture {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    raw_calls: Vec<RawCall>,
    imports: Vec<ImportRecord>,
    kg: KnowledgeGraph,
    communities: BTreeMap<u32, Vec<NodeId>>,
    labels: BTreeMap<u32, String>,
}

fn workspace_crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("crates")
        .canonicalize()
        .expect("canonicalize crates dir")
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            collect_rs(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn real_fixture() -> &'static RealFixture {
    static FIXTURE: OnceLock<RealFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let mut files = Vec::new();
        collect_rs(&workspace_crates_dir(), &mut files);
        files.sort();

        let (mut nodes, mut edges, mut raw_calls, mut imports) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for path in &files {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let path_str = path.to_string_lossy();
            if let Some(res) = synaptic_extract::extract_source(&path_str, &bytes) {
                nodes.extend(res.nodes);
                edges.extend(res.edges);
                raw_calls.extend(res.raw_calls);
                imports.extend(res.imports);
            }
        }

        let kg = build_from_parts(nodes.clone(), edges.clone(), Vec::new(), &build_opts());
        let (communities, labels) = communities_and_labels(&kg);
        RealFixture {
            nodes,
            edges,
            raw_calls,
            imports,
            kg,
            communities,
            labels,
        }
    })
}

// Scaling benchmarks

fn bench_scaling(c: &mut Criterion) {
    let empty_communities: HashMap<NodeId, u32> = HashMap::new();

    let mut group = c.benchmark_group("graph/scaling");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));

    for &n in &SCALES {
        let (nodes, edges) = synthetic_parts(n);
        let raw_calls = synthetic_raw_calls(n);
        let kg = build_from_parts(nodes.clone(), edges.clone(), Vec::new(), &build_opts());
        let (communities, labels) = communities_and_labels(&kg);

        group.throughput(Throughput::Elements(n as u64));

        // build_from_parts consumes its Vecs, so clone per iteration.
        group.bench_with_input(BenchmarkId::new("build_from_parts", n), &n, |b, _| {
            b.iter_batched(
                || (nodes.clone(), edges.clone()),
                |(ns, es)| black_box(build_from_parts(ns, es, Vec::new(), &build_opts())),
                BatchSize::SmallInput,
            );
        });

        // deduplicate_entities also consumes its Vecs (MinHash/LSH path).
        group.bench_with_input(BenchmarkId::new("deduplicate_entities", n), &n, |b, _| {
            b.iter_batched(
                || (nodes.clone(), edges.clone()),
                |(ns, es)| black_box(deduplicate_entities(ns, es, &empty_communities)),
                BatchSize::SmallInput,
            );
        });

        // resolve_symbols borrows the graph + call/import evidence.
        group.bench_with_input(BenchmarkId::new("resolve_symbols", n), &n, |b, _| {
            b.iter(|| black_box(resolve_symbols(&kg, &raw_calls, &[])));
        });

        // cluster: community detection (Leiden).
        group.bench_with_input(BenchmarkId::new("cluster", n), &n, |b, _| {
            b.iter(|| black_box(cluster(&kg, &ClusterOptions::default())));
        });

        // analyze: god nodes, surprises, questions, import cycles.
        group.bench_with_input(BenchmarkId::new("analyze", n), &n, |b, _| {
            b.iter(|| black_box(analyze(&kg, &communities, &labels)));
        });
    }
    group.finish();
}

// Real-workspace benchmarks

fn bench_real(c: &mut Criterion) {
    let fx = real_fixture();
    let empty_communities: HashMap<NodeId, u32> = HashMap::new();

    let mut group = c.benchmark_group("graph/real");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));
    group.throughput(Throughput::Elements(fx.nodes.len() as u64));

    group.bench_function("build_from_parts", |b| {
        b.iter_batched(
            || (fx.nodes.clone(), fx.edges.clone()),
            |(ns, es)| black_box(build_from_parts(ns, es, Vec::new(), &build_opts())),
            BatchSize::SmallInput,
        );
    });

    group.bench_function("deduplicate_entities", |b| {
        b.iter_batched(
            || (fx.nodes.clone(), fx.edges.clone()),
            |(ns, es)| black_box(deduplicate_entities(ns, es, &empty_communities)),
            BatchSize::SmallInput,
        );
    });

    group.bench_function("resolve_symbols", |b| {
        b.iter(|| black_box(resolve_symbols(&fx.kg, &fx.raw_calls, &fx.imports)));
    });

    group.bench_function("cluster", |b| {
        b.iter(|| black_box(cluster(&fx.kg, &ClusterOptions::default())));
    });

    group.bench_function("analyze", |b| {
        b.iter(|| black_box(analyze(&fx.kg, &fx.communities, &fx.labels)));
    });

    group.finish();
}

/// `analyze` is the single largest graph-pipeline cost on the real fixture, so
/// break it into its four components (matching the calls inside `analyze`) to
/// see where the time actually goes.
fn bench_analyze_breakdown(c: &mut Criterion) {
    let fx = real_fixture();

    let mut group = c.benchmark_group("graph/analyze_breakdown");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));

    group.bench_function("god_nodes", |b| {
        b.iter(|| black_box(god_nodes(&fx.kg, 10)));
    });
    group.bench_function("surprising_connections", |b| {
        b.iter(|| black_box(surprising_connections(&fx.kg, &fx.communities, 5)));
    });
    group.bench_function("suggest_questions", |b| {
        b.iter(|| black_box(suggest_questions(&fx.kg, &fx.communities, &fx.labels, 7)));
    });
    group.bench_function("find_import_cycles", |b| {
        b.iter(|| black_box(find_import_cycles(&fx.kg, 5, 20)));
    });

    group.finish();
}

/// H2: a node's degree. The pre-H2 server `degree()` scanned the *entire* edge
/// list per lookup (O(E)); the new `KnowledgeGraph::degree` uses petgraph's
/// incident-edge adjacency (O(degree)). Same 100 lookups, both ways.
fn bench_degree(c: &mut Criterion) {
    let fx = real_fixture();
    let ids: Vec<NodeId> = fx.kg.nodes().take(100).map(|n| n.id.clone()).collect();

    let mut group = c.benchmark_group("graph/degree_lookup");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    // OLD: O(edges) full scan per lookup.
    group.bench_function("full_scan_old", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for id in &ids {
                total += fx
                    .kg
                    .edges()
                    .filter(|e| &e.source == id || &e.target == id)
                    .count();
            }
            black_box(total)
        });
    });

    // NEW (H2): O(degree) via petgraph adjacency.
    group.bench_function("incident_new", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for id in &ids {
                total += fx.kg.degree(id);
            }
            black_box(total)
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_scaling,
    bench_real,
    bench_analyze_breakdown,
    bench_degree
);
criterion_main!(benches);
