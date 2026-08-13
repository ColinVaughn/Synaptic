//! Cost of repeated blast-radius forecasts against one static graph.
//!
//! The vulnerability scan calls `ImpactIndex::forecast` once per finding that
//! has call sites. This measures what that loop costs with the adjacency
//! rebuilt per call versus built once, behind a counting allocator so the
//! numbers are exact rather than subject to allocator caching.
//!
//! Usage: `cargo run --release --example forecastbench -- <graph.json> [rounds]`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use synaptic_core::NodeId;
use synaptic_graph::KnowledgeGraph;
use synaptic_predict::{
    DEFAULT_AFFECTED_RELATIONS, ForecastOptions, ReverseImpactIndex, forecast_nodes,
    forecast_nodes_with_index,
};

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            ALLOCATED.fetch_add(l.size(), Ordering::Relaxed);
            let now = LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
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
                ALLOCATED.fetch_add(d, Ordering::Relaxed);
                let now = LIVE.fetch_add(d, Ordering::Relaxed) + d;
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}
#[global_allocator]
static A: Counting = Counting;

fn mib(b: usize) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: forecastbench <graph.json> [rounds]");
    let rounds: usize = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);

    let text = std::fs::read_to_string(&path).expect("read graph");
    let gd: synaptic_core::GraphData = serde_json::from_str(&text).expect("parse graph");
    drop(text);
    let kg = KnowledgeGraph::from_graph_data(gd);
    println!(
        "graph: {} nodes, {} edges | {rounds} forecasts\n",
        kg.node_count(),
        kg.edge_count()
    );

    // Spread the seeds so the walk is representative rather than one hot node.
    let seeds: Vec<Vec<NodeId>> = kg
        .nodes()
        .filter(|n| n.is_code_symbol())
        .step_by((kg.node_count() / rounds.max(1)).max(1))
        .take(rounds)
        .map(|n| vec![n.id.clone()])
        .collect();
    let opts = ForecastOptions::default();

    // A: adjacency rebuilt on every call (what the scan loop used to do).
    ALLOCATED.store(0, Ordering::Relaxed);
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let t0 = Instant::now();
    let mut total = 0usize;
    for s in &seeds {
        total += forecast_nodes(&kg, s, &opts).blast_radius_total;
    }
    let per_call = (
        t0.elapsed(),
        ALLOCATED.load(Ordering::Relaxed),
        PEAK.load(Ordering::Relaxed),
    );

    // B: adjacency built once, reused.
    ALLOCATED.store(0, Ordering::Relaxed);
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let t1 = Instant::now();
    let index = ReverseImpactIndex::build(&kg, DEFAULT_AFFECTED_RELATIONS);
    let build = t1.elapsed();
    let mut total_indexed = 0usize;
    for s in &seeds {
        total_indexed += forecast_nodes_with_index(&kg, &index, s, &opts).blast_radius_total;
    }
    let indexed = (
        t1.elapsed(),
        ALLOCATED.load(Ordering::Relaxed),
        PEAK.load(Ordering::Relaxed),
    );

    assert_eq!(
        total, total_indexed,
        "indexed walk must agree with the per-call walk"
    );

    println!(
        "{:<26} {:>12} {:>16} {:>14}",
        "", "wall", "bytes allocated", "peak live"
    );
    println!(
        "{:<26} {:>10.2}s {:>13.1} MiB {:>11.1} MiB",
        "rebuilt per call",
        per_call.0.as_secs_f64(),
        mib(per_call.1),
        mib(per_call.2)
    );
    println!(
        "{:<26} {:>10.2}s {:>13.1} MiB {:>11.1} MiB",
        "built once, reused",
        indexed.0.as_secs_f64(),
        mib(indexed.1),
        mib(indexed.2)
    );
    println!("\nindex build alone         : {:.2}s", build.as_secs_f64());
    println!(
        "allocation reduction      : {:.1}x  ({:.1} MiB -> {:.1} MiB)",
        per_call.1 as f64 / indexed.1.max(1) as f64,
        mib(per_call.1),
        mib(indexed.1)
    );
    println!(
        "speedup                   : {:.1}x",
        per_call.0.as_secs_f64() / indexed.0.as_secs_f64().max(1e-9)
    );
    println!("(identical results: blast-radius totals agree)");
}
