//! `Server::load` must not hold the `graph.json` byte buffer while it builds the
//! graph and its indexes.
//!
//! The index build is the most memory-hungry phase of a load, so a buffer that
//! outlives the parse adds its whole length to peak RSS. On a 965 MiB corporate
//! graph that alone was ~1 GiB of dead weight, enough to push a 4 GiB container
//! over. This test runs the real load under a counting allocator and pins the
//! buffer's lifetime: at peak, the file must not still be resident.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = LIVE.fetch_add(layout.size(), Relaxed) + layout.size();
            PEAK.fetch_max(now, Relaxed);
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
            if new_size >= layout.size() {
                let now =
                    LIVE.fetch_add(new_size - layout.size(), Relaxed) + new_size - layout.size();
                PEAK.fetch_max(now, Relaxed);
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// A graph big enough that the file buffer is an unmistakable share of the load,
/// with realistic label/path shapes so the indexes do proportionate work.
fn write_fixture(path: &std::path::Path) -> u64 {
    let n = 6000usize;
    let mut nodes = Vec::with_capacity(n);
    for i in 0..n {
        nodes.push(serde_json::json!({
            "id": format!("crate_module_{i:05}_handler_function"),
            "label": format!("handle_request_variant_{i:05}()"),
            "file_type": "code",
            "source_file": format!("crates/service-{}/src/handlers/route_{i:05}.rs", i % 40),
            "source_location": format!("L{}", i % 900 + 1),
            "kind": "function",
            "visibility": "public",
            "span": {"start_line": i % 900 + 1, "start_col": 1,
                     "end_line": i % 900 + 24, "end_col": 2},
            "norm_label": format!("handle request variant {i:05}"),
            "_origin": "ast",
        }));
    }
    let mut links = Vec::with_capacity(n * 3);
    for i in 0..n {
        for hop in [1usize, 7, 53] {
            let t = (i + hop) % n;
            links.push(serde_json::json!({
                "source": format!("crate_module_{i:05}_handler_function"),
                "target": format!("crate_module_{t:05}_handler_function"),
                "relation": "calls",
                "confidence": "EXTRACTED",
                "confidence_score": 0.9,
                "weight": 1.0,
                "source_file": format!("crates/service-{}/src/handlers/route_{i:05}.rs", i % 40),
                "source_location": format!("L{}", i % 900 + 1),
            }));
        }
    }
    let gd = serde_json::json!({
        "directed": true, "multigraph": false, "graph": {},
        "nodes": nodes, "links": links, "hyperedges": [],
    });
    std::fs::write(path, serde_json::to_vec_pretty(&gd).unwrap()).unwrap();
    std::fs::metadata(path).unwrap().len()
}

#[test]
fn load_does_not_hold_the_file_buffer_across_the_index_build() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.json");
    let file_len = write_fixture(&path) as usize;

    // Start measuring only once the fixture is on disk and freed.
    PEAK.store(LIVE.load(Relaxed), Relaxed);
    let before = LIVE.load(Relaxed);

    let server = synaptic_server::Server::load(path).expect("load graph.json");

    let peak = PEAK.load(Relaxed);
    let resident = LIVE.load(Relaxed);
    std::hint::black_box(&server);

    let headroom = peak.saturating_sub(resident);
    assert!(
        headroom < file_len / 2,
        "peak exceeded the loaded server by {headroom} B on a {file_len} B graph.json, \
         which means the input buffer was still resident at peak. \
         peak={peak} resident={resident} before={before}"
    );
}
