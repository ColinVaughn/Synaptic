//! What writing the shard store costs, stage by stage.
//!
//! `extract` peaks about 5 GiB higher with the store than with `--no-store` on a
//! large repository. This walks the same stages the store writer does behind a
//! counting allocator so the cost can be attributed before anything is changed.
//!
//! Usage: `cargo run --release --example storeprofile -- <graph.json>`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use synaptic_query::{QueryIndex, ReverseImpactIndex, DEFAULT_AFFECTED_RELATIONS};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let n = LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(n, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                let d = new - l.size();
                let n = LIVE.fetch_add(d, Ordering::Relaxed) + d;
                PEAK.fetch_max(n, Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}
#[global_allocator]
static A: Counting = Counting;

fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}
fn mib(b: usize) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn mark(name: &str, before: usize) -> usize {
    let now = live();
    println!(
        "{name:<38} live {:>9.1} MiB   delta {:>+9.1} MiB",
        mib(now),
        mib(now) - mib(before)
    );
    now
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: storeprofile <graph.json>");

    let mut m = live();
    let text = std::fs::read_to_string(&path).expect("read graph");
    let gd: synaptic_core::GraphData = serde_json::from_str(&text).expect("parse graph");
    drop(text);
    println!(
        "graph: {} nodes, {} edges\n",
        gd.nodes.len(),
        gd.links.len()
    );
    m = mark("graph resident (the export view)", m);

    // The store splits the graph into per-repo shards; each is an owned copy.
    let split = synaptic_store::migrate::split(gd.clone());
    println!(
        "  -> {} shard(s), {} bridge edge(s)",
        split.shards.len(),
        split.bridge.len()
    );
    m = mark("split() into shards", m);

    for (tag, shard) in &split.shards {
        println!(
            "\nshard {tag:?}: {} nodes, {} edges",
            shard.nodes.len(),
            shard.links.len()
        );

        // What the index callback does now: borrow the shard in place.
        let nodes: Vec<&synaptic_core::Node> = shard.nodes.iter().collect();
        let edges: Vec<&synaptic_core::Edge> = shard.links.iter().collect();
        m = mark("  borrow shard (no clone)", m);

        let qi = QueryIndex::build_from_parts(&nodes, &edges);
        m = mark("  QueryIndex::build_from_parts", m);

        let qi_bytes = qi.to_bytes().expect("query index bytes");
        println!("    (query index blob: {:.1} MiB)", mib(qi_bytes.len()));
        m = mark("  QueryIndex::to_bytes", m);

        let ai = ReverseImpactIndex::build_from_parts(&edges, DEFAULT_AFFECTED_RELATIONS);
        m = mark("  ReverseImpactIndex::build", m);

        let ai_bytes = ai.to_bytes().expect("impact index bytes");
        println!("    (impact index blob: {:.1} MiB)", mib(ai_bytes.len()));
        m = mark("  ReverseImpactIndex::to_bytes", m);

        drop((qi, ai, qi_bytes, ai_bytes, nodes, edges));
        m = mark("  drop shard work", m);
    }

    println!(
        "\nPEAK ALLOCATED: {:.1} MiB   (graph alone was {:.1} MiB)",
        mib(PEAK.load(Ordering::Relaxed)),
        mib(live())
    );
}
