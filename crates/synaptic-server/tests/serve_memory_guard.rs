//! The serve path must refuse an over-cap graph with an explanation rather than
//! growing until the OOM killer takes the process.
//!
//! `SYNAPTIC_MAX_SERVE_MB` is process-global, so this lives in its own
//! integration binary with a single test: nothing else runs concurrently to see
//! the mutated environment.

use std::path::Path;

fn write_graph(path: &Path, nodes: usize) -> u64 {
    let nodes: Vec<_> = (0..nodes)
        .map(|i| {
            serde_json::json!({
                "id": format!("node_{i:06}"),
                "label": format!("some_reasonably_long_symbol_name_{i:06}()"),
                "file_type": "code",
                "source_file": format!("crates/example/src/module_{i:06}.rs"),
                "source_location": "L1",
            })
        })
        .collect();
    let gd = serde_json::json!({
        "directed": true, "multigraph": false, "graph": {},
        "nodes": nodes, "links": [], "hyperedges": [],
    });
    std::fs::write(path, serde_json::to_vec_pretty(&gd).unwrap()).unwrap();
    std::fs::metadata(path).unwrap().len()
}

#[test]
fn an_over_cap_graph_is_refused_with_a_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.json");
    let len = write_graph(&path, 8000);
    assert!(
        len > 1024 * 1024,
        "fixture must exceed the 1 MiB cap: {len}"
    );

    // SAFETY: single-threaded, single-test binary; nothing else reads the env.
    unsafe { std::env::set_var(synaptic_core::MAX_SERVE_MB_ENV, "1") };

    let Err(err) = synaptic_server::Server::load(path.clone()) else {
        panic!("a graph over the serve cap must be refused");
    };
    let msg = err.to_string();
    assert!(msg.contains(synaptic_core::MAX_SERVE_MB_ENV), "{msg}");
    assert!(msg.contains("1 MiB"), "names the cap: {msg}");

    // Raising the cap lets the same graph load.
    unsafe { std::env::set_var(synaptic_core::MAX_SERVE_MB_ENV, "0") };
    synaptic_server::Server::load(path).expect("uncapped load succeeds");
}
