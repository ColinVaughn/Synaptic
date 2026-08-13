//! What a graph-backed vulnerability scan retains, per package.
//!
//! A scan asks the usage oracle about a package once per matching advisory, and
//! every ask clones that package's whole call-site list onto the resulting
//! finding. This reports the distribution so the retained cost can be sized
//! before capping it.
//!
//! Usage: `cargo run --release --example scanprofile -- <graph.json>`

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

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

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: scanprofile <graph.json>");

    let base = live();
    let text = std::fs::read_to_string(&path).expect("read graph");
    let gd: synaptic_core::GraphData = serde_json::from_str(&text).expect("parse graph");
    drop(text);
    println!(
        "graph: {} nodes, {} edges   resident {:.0} MiB\n",
        gd.nodes.len(),
        gd.links.len(),
        mib(live() - base)
    );

    // Every external package the graph knows about, from its SDK stub labels.
    let mut packages: BTreeMap<(String, String), ()> = BTreeMap::new();
    for node in &gd.nodes {
        if !node.source_file.is_empty() {
            continue;
        }
        let Some(body) = node.label.strip_prefix("Sdk: ") else {
            continue;
        };
        let Some((coordinate, _member)) = body.split_once('#') else {
            continue;
        };
        let Some((eco, name)) = coordinate.split_once(':') else {
            continue;
        };
        packages.insert(
            (eco.trim().to_ascii_lowercase(), name.trim().to_string()),
            (),
        );
    }
    println!("external packages in graph: {}", packages.len());

    // Replicate the oracle's two keyings without building it, since building it
    // is what runs out of memory. The first loop keys by (package, member); the
    // second iterates per stub NODE and clones that pair's whole site list each
    // time. Any node/pair ratio above 1 is duplicated storage.
    let mut stub_nodes: BTreeMap<&str, (String, String)> = BTreeMap::new();
    for node in &gd.nodes {
        if !node.source_file.is_empty() {
            continue;
        }
        let Some(body) = node.label.strip_prefix("Sdk: ") else {
            continue;
        };
        let Some((coordinate, member)) = body.split_once('#') else {
            continue;
        };
        let Some((eco, name)) = coordinate.split_once(':') else {
            continue;
        };
        stub_nodes.insert(
            node.id.0.as_str(),
            (
                format!("{}:{}", eco.trim().to_ascii_lowercase(), name.trim()),
                member.trim().to_string(),
            ),
        );
    }
    let pairs: std::collections::BTreeSet<(&str, &str)> = stub_nodes
        .values()
        .map(|(k, m)| (k.as_str(), m.as_str()))
        .collect();

    // Sites per (package, member), as the first loop accumulates them.
    let by_id: BTreeMap<&str, &synaptic_core::Node> =
        gd.nodes.iter().map(|n| (n.id.0.as_str(), n)).collect();
    let mut sites_per_pair: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for edge in &gd.links {
        let Some((key, member)) = stub_nodes.get(edge.target.0.as_str()) else {
            continue;
        };
        let Some(source) = by_id.get(edge.source.0.as_str()) else {
            continue;
        };
        if source.source_file.is_empty() {
            continue;
        }
        *sites_per_pair
            .entry((key.as_str(), member.as_str()))
            .or_default() += 1;
    }
    let total_sites: usize = sites_per_pair.values().sum();

    // What the second loop actually stores: one copy per stub node.
    let stored: usize = stub_nodes
        .values()
        .map(|(k, m)| {
            sites_per_pair
                .get(&(k.as_str(), m.as_str()))
                .copied()
                .unwrap_or(0)
        })
        .sum();

    println!("stub nodes                : {}", stub_nodes.len());
    println!("distinct (package,member) : {}", pairs.len());
    println!(
        "duplication factor        : {:.1}x",
        stub_nodes.len() as f64 / pairs.len().max(1) as f64
    );
    println!();
    println!("call sites (distinct)     : {total_sites}");
    println!("call sites STORED         : {stored}");
    println!(
        "storage amplification     : {:.1}x  (~{:.2} GiB at ~200 B/site)",
        stored as f64 / total_sites.max(1) as f64,
        (stored * 200) as f64 / (1024.0 * 1024.0 * 1024.0)
    );
    println!();
    let mut worst: Vec<_> = sites_per_pair.iter().collect();
    worst.sort_by(|a, b| b.1.cmp(a.1));
    println!("{:<58} {:>10}", "heaviest (package, member)", "sites");
    for ((k, m), n) in worst.iter().take(10) {
        println!("{:<58} {:>10}", format!("{k}#{m}"), n);
    }
}
