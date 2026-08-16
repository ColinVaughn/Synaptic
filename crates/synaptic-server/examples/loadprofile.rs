//! Where the memory goes when `graph.json` is expanded into a served graph.
//!
//! Runs the real load path under a counting global allocator, reporting live and
//! peak bytes at each stage plus a per-field breakdown of the parsed `GraphData`.
//! The breakdown is derived from the actual structures and its total is printed
//! next to the allocator's measurement, so drift is visible.
//!
//! ```text
//! cargo run --release -p synaptic-server --example loadprofile -- <graph.json> [mode]
//! ```
//!
//! Modes: `breakdown` (default), `current`, `dropfirst`, `reader`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use serde_json::{Map, Value};
use synaptic_core::{Edge, GraphData, Node};

// Counting global allocator: live bytes, peak, allocation count.

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

struct Counting;

fn grew(by: usize) {
    let now = LIVE.fetch_add(by, Relaxed) + by;
    PEAK.fetch_max(now, Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            ALLOCS.fetch_add(1, Relaxed);
            grew(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            ALLOCS.fetch_add(1, Relaxed);
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            ALLOCS.fetch_add(1, Relaxed);
            if new_size >= layout.size() {
                grew(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn live() -> usize {
    LIVE.load(Relaxed)
}
fn peak() -> usize {
    PEAK.load(Relaxed)
}
fn reset_peak() {
    PEAK.store(live(), Relaxed);
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn stage(name: &str) {
    println!(
        "  {name:<44} live {:>9.1} MiB   peak {:>9.1} MiB",
        mib(live()),
        mib(peak())
    );
}

// What an `extra` map costs before any payload.

/// What a `serde_json::Map` costs for `n` keys, excluding the key/value payloads.
/// This is `BTreeMap`'s fixed-capacity internal node allocation, which is what
/// makes a one-key map expensive.
fn map_overhead(n: usize) -> usize {
    let keys: Vec<String> = (0..n).map(|i| format!("k{i:06}")).collect();
    let payload: usize = keys.iter().map(|k| k.capacity() * 2).sum();
    let before = live();
    let mut m = Map::new();
    for k in &keys {
        m.insert(k.clone(), Value::String(k.clone()));
    }
    let cost = live() - before;
    std::hint::black_box(&m);
    drop(m);
    cost.saturating_sub(payload)
}

// Deep size of the parsed structures, by field.

fn value_deep(v: &Value, leaf: usize) -> usize {
    match v {
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
        Value::String(s) => s.len(),
        Value::Array(a) => {
            a.capacity() * size_of::<Value>() + a.iter().map(|x| value_deep(x, leaf)).sum::<usize>()
        }
        Value::Object(m) => map_deep(m, leaf),
    }
}

fn map_deep(m: &Map<String, Value>, leaf: usize) -> usize {
    if m.is_empty() {
        return 0;
    }
    leaf + m
        .iter()
        .map(|(k, v)| k.len() + value_deep(v, leaf))
        .sum::<usize>()
}

#[derive(Default)]
struct Breakdown {
    node_vec: usize,
    node_ids: usize,
    node_labels: usize,
    node_source_files: usize,
    node_locations: usize,
    node_signatures: usize,
    node_extra_maps: usize,
    node_extra_payload: usize,
    nodes_with_extra: usize,
    nodes_extra_only_norm_label: usize,

    edge_vec: usize,
    edge_ids: usize,
    edge_relations: usize,
    edge_source_files: usize,
    edge_locations: usize,
    edge_contexts: usize,
    edge_extra_maps: usize,
    edge_extra_payload: usize,
    edges_with_extra: usize,

    key_costs: Vec<(String, usize)>,
}

fn measure(gd: &GraphData, leaf: usize) -> Breakdown {
    let mut b = Breakdown::default();
    let mut key_costs: std::collections::BTreeMap<String, usize> = Default::default();

    b.node_vec = gd.nodes.capacity() * size_of::<Node>();
    for n in &gd.nodes {
        b.node_ids += n.id.as_str().len();
        b.node_labels += n.label.len();
        b.node_source_files += n.source_file.len();
        b.node_locations += n.source_location.as_deref().map_or(0, str::len);
        if let Some(sig) = &n.signature {
            b.node_signatures += size_of::<synaptic_core::Signature>()
                + sig.params.capacity() * size_of::<synaptic_core::Param>()
                + sig
                    .params
                    .iter()
                    .map(|p| p.name.len() + p.type_ref.as_deref().map_or(0, str::len))
                    .sum::<usize>()
                + sig.return_type.as_deref().map_or(0, str::len)
                + sig.raw.len();
        }
        if !n.extra.is_empty() {
            b.nodes_with_extra += 1;
            b.node_extra_maps += leaf;
            if n.extra.len() == 1 && n.extra.contains_key("norm_label") {
                b.nodes_extra_only_norm_label += 1;
            }
            let share = leaf / n.extra.len();
            for (k, v) in &n.extra {
                let cost = k.len() + value_deep(v, leaf);
                b.node_extra_payload += cost;
                *key_costs.entry(k.clone()).or_default() += cost + share;
            }
        }
    }

    b.edge_vec = gd.links.capacity() * size_of::<Edge>();
    for e in &gd.links {
        b.edge_ids += e.source.as_str().len() + e.target.as_str().len();
        b.edge_relations += e.relation.len();
        b.edge_source_files += e.source_file.len();
        b.edge_locations += e.source_location.as_deref().map_or(0, str::len);
        b.edge_contexts += e.context.as_deref().map_or(0, str::len);
        if !e.extra.is_empty() {
            b.edges_with_extra += 1;
            b.edge_extra_maps += leaf;
            let share = leaf / e.extra.len();
            for (k, v) in &e.extra {
                let cost = k.len() + value_deep(v, leaf);
                b.edge_extra_payload += cost;
                *key_costs.entry(k.clone()).or_default() += cost + share;
            }
        }
    }

    let mut keys: Vec<(String, usize)> = key_costs.into_iter().collect();
    keys.sort_by_key(|(_, cost)| std::cmp::Reverse(*cost));
    b.key_costs = keys;
    b
}

fn row(label: &str, bytes: usize, total: usize) {
    println!(
        "  {label:<36} {:>9.1} MiB  {:>5.1}%",
        mib(bytes),
        100.0 * bytes as f64 / total as f64
    );
}

// Entry point.

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: loadprofile <graph.json> [breakdown|current|dropfirst|reader]");
        std::process::exit(2);
    });
    let mode = args.next().unwrap_or_else(|| "breakdown".to_string());

    let file_len = std::fs::metadata(&path).expect("stat graph.json").len() as usize;
    println!(
        "\n### {path}  ({:.1} MiB on disk)   mode={mode}\n",
        mib(file_len)
    );

    match mode.as_str() {
        "current" => {
            // Exactly what `Server::load` does: the input buffer stays alive
            // across the parse AND the whole index build.
            reset_peak();
            stage("start");
            let bytes = std::fs::read(&path).expect("read");
            stage("after fs::read");
            let gd: GraphData = serde_json::from_slice(&bytes).expect("parse");
            stage("after from_slice");
            let server = synaptic_server::Server::from_graph_data(gd, None);
            stage("after Server::from_graph_data");
            std::hint::black_box(&server);
            std::hint::black_box(&bytes);
            drop(bytes);
            stage("after drop(buffer)");
        }
        "dropfirst" => {
            // Same, but the input buffer is released before the index build.
            reset_peak();
            stage("start");
            let gd: GraphData = {
                let bytes = std::fs::read(&path).expect("read");
                serde_json::from_slice(&bytes).expect("parse")
            };
            stage("after parse, buffer released");
            let server = synaptic_server::Server::from_graph_data(gd, None);
            stage("after Server::from_graph_data");
            std::hint::black_box(&server);
        }
        "indexes" => {
            // Attribute the post-parse growth: petgraph + id index, then each
            // derived search index in turn.
            let gd: GraphData = {
                let bytes = std::fs::read(&path).expect("read");
                serde_json::from_slice(&bytes).expect("parse")
            };
            let (n_nodes, n_edges) = (gd.nodes.len(), gd.links.len());
            let after_parse = live();
            stage("GraphData");

            let kg = synaptic_graph::KnowledgeGraph::from_graph_data(gd);
            let after_kg = live();
            stage("+ KnowledgeGraph (petgraph + id index)");

            let qi = synaptic_query::QueryIndex::build(&kg);
            let after_qi = live();
            stage("+ QueryIndex");

            let ri = synaptic_query::ReverseImpactIndex::build(
                &kg,
                synaptic_query::DEFAULT_AFFECTED_RELATIONS,
            );
            let after_ri = live();
            stage("+ ReverseImpactIndex");
            std::hint::black_box((&qi, &ri, &kg));

            println!("\n=== deltas ===");
            println!(
                "  GraphData -> KnowledgeGraph  {:>+9.1} MiB   ({:>6.0} B/node)",
                mib(after_kg) - mib(after_parse),
                (after_kg as f64 - after_parse as f64) / n_nodes as f64
            );
            println!(
                "  QueryIndex                   {:>+9.1} MiB   ({:>6.0} B/node)",
                mib(after_qi - after_kg),
                (after_qi - after_kg) as f64 / n_nodes as f64
            );
            println!(
                "  ReverseImpactIndex           {:>+9.1} MiB   ({:>6.0} B/edge)",
                mib(after_ri - after_qi),
                (after_ri - after_qi) as f64 / n_edges as f64
            );
        }
        "shapes" => {
            // Cost of the *shapes* QueryIndex/ReverseImpactIndex are built from,
            // against the index-keyed equivalent. Nothing here depends on the
            // private tokenizer, so these numbers are exact.
            use std::collections::HashMap;
            use synaptic_core::NodeId;

            let gd: GraphData = {
                let bytes = std::fs::read(&path).expect("read");
                serde_json::from_slice(&bytes).expect("parse")
            };
            let (n_nodes, n_edges) = (gd.nodes.len(), gd.links.len());
            println!("  {n_nodes} nodes, {n_edges} edges\n");

            let before = live();
            let by_id: HashMap<NodeId, f64> =
                gd.nodes.iter().map(|n| (n.id.clone(), 1.0)).collect();
            let id_map = live() - before;
            std::hint::black_box(&by_id);
            drop(by_id);
            println!(
                "  HashMap<NodeId, f64> (cloned ids)      {:>8.1} MiB  ({:>5.0} B/node)",
                mib(id_map),
                id_map as f64 / n_nodes as f64
            );
            println!(
                "    QueryIndex holds 6 such id-keyed maps {:>7.1} MiB",
                mib(id_map * 6)
            );

            let before = live();
            let by_ix: Vec<f64> = vec![1.0; n_nodes];
            let ix_vec = live() - before;
            std::hint::black_box(&by_ix);
            drop(by_ix);
            println!(
                "  Vec<f64> keyed by node index           {:>8.1} MiB  ({:>5.0} B/node)",
                mib(ix_vec),
                ix_vec as f64 / n_nodes as f64
            );

            let before = live();
            let mut adj: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
            for e in &gd.links {
                adj.entry(e.source.clone())
                    .or_default()
                    .push(e.target.clone());
                adj.entry(e.target.clone())
                    .or_default()
                    .push(e.source.clone());
            }
            let adj_cost = live() - before;
            std::hint::black_box(&adj);
            drop(adj);
            println!(
                "  HashMap<NodeId, Vec<NodeId>> adjacency {:>8.1} MiB  ({:>5.0} B/edge)",
                mib(adj_cost),
                adj_cost as f64 / n_edges as f64
            );

            let ids: HashMap<&str, u32> = gd
                .nodes
                .iter()
                .enumerate()
                .map(|(i, n)| (n.id.as_str(), i as u32))
                .collect();
            let before = live();
            let mut adj_ix: Vec<Vec<u32>> = vec![Vec::new(); n_nodes];
            for e in &gd.links {
                if let (Some(&s), Some(&t)) =
                    (ids.get(e.source.as_str()), ids.get(e.target.as_str()))
                {
                    adj_ix[s as usize].push(t);
                    adj_ix[t as usize].push(s);
                }
            }
            let adj_ix_cost = live() - before;
            std::hint::black_box(&adj_ix);
            drop(adj_ix);
            println!(
                "  Vec<Vec<u32>> adjacency                {:>8.1} MiB  ({:>5.0} B/edge)",
                mib(adj_ix_cost),
                adj_ix_cost as f64 / n_edges as f64
            );

            let before = live();
            let rev: HashMap<NodeId, Vec<(NodeId, String)>> = {
                let mut m: HashMap<NodeId, Vec<(NodeId, String)>> = HashMap::new();
                for e in &gd.links {
                    m.entry(e.target.clone())
                        .or_default()
                        .push((e.source.clone(), e.relation.to_string()));
                }
                m
            };
            let rev_cost = live() - before;
            std::hint::black_box(&rev);
            drop(rev);
            println!(
                "  ReverseImpact rev map (id+relation)    {:>8.1} MiB  ({:>5.0} B/edge)",
                mib(rev_cost),
                rev_cost as f64 / n_edges as f64
            );

            let distinct: std::collections::BTreeSet<&str> =
                gd.links.iter().map(|e| e.relation.as_str()).collect();
            println!(
                "\n  distinct relation strings in the graph: {} (cloned {n_edges}x in rev)",
                distinct.len()
            );
            let distinct_files: std::collections::BTreeSet<&str> = gd
                .nodes
                .iter()
                .map(|n| n.source_file.as_str())
                .chain(gd.links.iter().map(|e| e.source_file.as_str()))
                .collect();
            let sf_bytes: usize = gd.nodes.iter().map(|n| n.source_file.len()).sum::<usize>()
                + gd.links.iter().map(|e| e.source_file.len()).sum::<usize>();
            let sf_distinct: usize = distinct_files.iter().map(|s| s.len()).sum();
            println!(
                "  distinct source_file paths: {} holding {:.1} MiB; stored uninterned: {:.1} MiB",
                distinct_files.len(),
                mib(sf_distinct),
                mib(sf_bytes)
            );
        }
        "roundtrip" => {
            // Read a real graph.json and write it back through the export path.
            // The bytes must match: the reader drops `norm_label` and the writer
            // re-derives it, and interned fields serialize as plain strings.
            let original = std::fs::read(&path).expect("read");
            let gd: GraphData = serde_json::from_slice(&original).expect("parse");
            let kg = synaptic_graph::KnowledgeGraph::from_graph_data(gd);
            let out = std::env::temp_dir().join("synaptic-roundtrip-graph.json");
            synaptic_output::to_json(&kg, &out).expect("write");
            let written = std::fs::read(&out).expect("read back");
            let _ = std::fs::remove_file(&out);
            println!(
                "  original {} B, rewritten {} B",
                original.len(),
                written.len()
            );
            if original == written {
                println!("  BYTE-IDENTICAL: load/store round trip preserves graph.json exactly");
            } else {
                let at = original
                    .iter()
                    .zip(&written)
                    .position(|(a, b)| a != b)
                    .unwrap_or(original.len().min(written.len()));
                let window = |b: &[u8]| {
                    String::from_utf8_lossy(&b[at.saturating_sub(60)..(at + 60).min(b.len())])
                        .to_string()
                };
                println!("  DIFFERS at byte {at}");
                println!("  original:  {}", window(&original));
                println!("  rewritten: {}", window(&written));
                std::process::exit(1);
            }
        }
        "reader" => {
            // Streaming parse: the file is never a heap buffer at all.
            reset_peak();
            stage("start");
            let f = std::fs::File::open(&path).expect("open");
            let gd: GraphData =
                serde_json::from_reader(std::io::BufReader::with_capacity(1 << 20, f))
                    .expect("parse");
            stage("after from_reader");
            let server = synaptic_server::Server::from_graph_data(gd, None);
            stage("after Server::from_graph_data");
            std::hint::black_box(&server);
        }
        _ => {
            println!("=== struct sizes ===");
            println!("  size_of::<Node>()  = {:>4} B", size_of::<Node>());
            println!("  size_of::<Edge>()  = {:>4} B", size_of::<Edge>());
            println!("  size_of::<Value>() = {:>4} B", size_of::<Value>());

            println!("\n=== serde_json::Map (BTreeMap) overhead, payload excluded ===");
            let mut leaf = 0usize;
            for n in [1usize, 2, 5, 11, 12, 24] {
                let cost = map_overhead(n);
                if n == 1 {
                    leaf = cost;
                }
                println!("  {n:>3} key(s): {cost:>6} B  ({:>6} B/key)", cost / n);
            }

            println!("\n=== load ===");
            reset_peak();
            stage("start");
            let bytes = std::fs::read(&path).expect("read");
            stage("after fs::read (input buffer)");
            let gd: GraphData = serde_json::from_slice(&bytes).expect("parse");
            let after_parse = live();
            stage("after from_slice (buffer + GraphData)");
            drop(bytes);
            let gd_only = live();
            stage("after drop(buffer) (GraphData only)");

            let n_nodes = gd.nodes.len();
            let n_edges = gd.links.len();

            println!("\n=== GraphData breakdown ({n_nodes} nodes, {n_edges} edges) ===");
            let b = measure(&gd, leaf);
            let total = gd_only;
            row("Vec<Node> buffer", b.node_vec, total);
            row("  node ids", b.node_ids, total);
            row("  node labels", b.node_labels, total);
            row("  node source_file", b.node_source_files, total);
            row("  node source_location", b.node_locations, total);
            row("  node signatures", b.node_signatures, total);
            row("  node extra: MAP OVERHEAD", b.node_extra_maps, total);
            row("  node extra: payload", b.node_extra_payload, total);
            row("Vec<Edge> buffer", b.edge_vec, total);
            row("  edge source+target ids", b.edge_ids, total);
            row("  edge relation", b.edge_relations, total);
            row("  edge source_file", b.edge_source_files, total);
            row("  edge source_location", b.edge_locations, total);
            row("  edge context", b.edge_contexts, total);
            row("  edge extra: MAP OVERHEAD", b.edge_extra_maps, total);
            row("  edge extra: payload", b.edge_extra_payload, total);
            let accounted = b.node_vec
                + b.node_ids
                + b.node_labels
                + b.node_source_files
                + b.node_locations
                + b.node_signatures
                + b.node_extra_maps
                + b.node_extra_payload
                + b.edge_vec
                + b.edge_ids
                + b.edge_relations
                + b.edge_source_files
                + b.edge_locations
                + b.edge_contexts
                + b.edge_extra_maps
                + b.edge_extra_payload;
            row("ACCOUNTED", accounted, total);
            row(
                "unaccounted (rounding, misc)",
                total.saturating_sub(accounted),
                total,
            );
            println!(
                "  nodes with a non-empty extra: {}/{n_nodes} ({:.1}%)   of those, \
                 norm_label-only: {}",
                b.nodes_with_extra,
                100.0 * b.nodes_with_extra as f64 / n_nodes.max(1) as f64,
                b.nodes_extra_only_norm_label
            );
            println!(
                "  edges with a non-empty extra: {}/{n_edges} ({:.1}%)",
                b.edges_with_extra,
                100.0 * b.edges_with_extra as f64 / n_edges.max(1) as f64
            );

            println!("\n  top `extra` keys by total cost (incl. share of map overhead):");
            for (k, cost) in b.key_costs.iter().take(10) {
                println!("    {k:<28} {:>8.1} MiB", mib(*cost));
            }

            // What dropping the derived key at parse time would actually save.
            let mut gd = gd;
            let before_strip = live();
            for n in &mut gd.nodes {
                if n.extra.len() == 1 && n.extra.contains_key("norm_label") {
                    n.extra = Map::new();
                }
            }
            println!(
                "\n  stripping norm_label-only extras frees   {:>8.1} MiB  ({:.1}% of GraphData)",
                mib(before_strip - live()),
                100.0 * (before_strip - live()) as f64 / total as f64
            );

            println!("\n=== downstream ===");
            reset_peak();
            let server = synaptic_server::Server::from_graph_data(gd, None);
            stage("after Server::from_graph_data (indexes)");
            std::hint::black_box(&server);

            println!("\n=== summary ===");
            println!("  on disk                 {:>9.1} MiB", mib(file_len));
            println!(
                "  peak during parse       {:>9.1} MiB  ({:.2}x file)",
                mib(after_parse),
                after_parse as f64 / file_len as f64
            );
            println!(
                "  GraphData resident      {:>9.1} MiB  ({:.2}x file)",
                mib(gd_only),
                gd_only as f64 / file_len as f64
            );
            println!(
                "  served resident         {:>9.1} MiB  ({:.2}x file)",
                mib(live()),
                live() as f64 / file_len as f64
            );
            println!(
                "  bytes/node (GraphData)  {:>9.0} B",
                gd_only as f64 / n_nodes as f64
            );
            println!(
                "  bytes/edge (est.)       {:>9.0} B",
                b.edge_vec as f64 / n_edges as f64
            );
            println!("  allocations             {:>9}", ALLOCS.load(Relaxed));
        }
    }
}
