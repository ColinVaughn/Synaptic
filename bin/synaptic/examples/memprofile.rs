//! Deterministic allocation profile of the extract pipeline.
//!
//! Peak RSS / private bytes are the wrong tool for judging this pipeline on
//! Windows: the Low-Fragmentation Heap deliberately caches freed blocks instead
//! of decommitting them, so the OS counters report a high-water mark that
//! includes memory the process has already released, and they vary ~10% run to
//! run. This walks the same stages `run_extract` does behind a counting global
//! allocator, so every number is exact and reproducible.
//!
//! Usage: `cargo run --release --example memprofile -- <repo-root>`

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;
use synaptic_detect::{detect, FileType};
use synaptic_extract::{cached_extract_source, load_alias_resolver, resolve_imports};
use synaptic_graph::{
    analyze, apply_communities, build_from_parts, cluster, deduplicate_entities,
    deterministic_tiebreak_candidates, link_dynamic_refs, merge_pairs, resolve_command_invocations,
    resolve_parameterized_routes, resolve_pyo3_imports, resolve_pyo3_modules,
    resolve_route_handlers, resolve_sql_queries, resolve_symbols, BuildOptions, ClusterOptions,
};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
/// Never reset, unlike PEAK (which is rearmed per stage).
static GLOBAL_PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            let now = LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
            GLOBAL_PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = System.realloc(p, l, new);
        if !q.is_null() {
            if new >= l.size() {
                let d = new - l.size();
                let now = LIVE.fetch_add(d, Ordering::Relaxed) + d;
                PEAK.fetch_max(now, Ordering::Relaxed);
                GLOBAL_PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}
fn peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}
fn global_peak() -> usize {
    GLOBAL_PEAK.load(Ordering::Relaxed)
}
fn mib(b: usize) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

/// Report a stage: what it left resident, and the high-water mark reached while
/// it ran. Resets the peak so the next stage is measured independently.
fn stage(name: &str, before: usize) -> usize {
    let now = live();
    let pk = peak();
    println!(
        "{name:<34} live {:>9.1} MiB   delta {:>+9.1} MiB   peak-in-stage {:>9.1} MiB",
        mib(now),
        mib(now) - mib(before),
        mib(pk)
    );
    PEAK.store(now, Ordering::Relaxed);
    now
}

fn main() {
    let root = std::env::args()
        .nth(1)
        .expect("usage: memprofile <repo-root>");
    let root = Path::new(&root)
        .canonicalize()
        .expect("resolving repo root");
    let opts = BuildOptions {
        directed: false,
        root: Some(root.to_string_lossy().into_owned()),
    };

    println!("profiling {}\n", root.display());
    let mut m = live();

    let det = detect(&root);
    let code_files = det.of(FileType::Code).to_vec();
    let n_code = code_files.len();
    m = stage("detect", m);

    // Extraction is the accumulator: every node/edge/raw-call of the corpus is
    // live at once from here until build.
    let cache_dir = root.join("synaptic-out").join("cache");
    let results: Vec<_> = synaptic_extract::with_extraction_pool(|| {
        code_files
            .par_iter()
            .map(|file| {
                let rel = file.strip_prefix(&root).unwrap_or(file);
                let rel_str = rel.to_string_lossy();
                std::fs::read(file).ok().and_then(|bytes| {
                    cached_extract_source(Some(&cache_dir), rel_str.as_ref(), &bytes)
                })
            })
            .collect()
    });
    m = stage("extract (parallel, all files)", m);

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut raw_calls = Vec::new();
    let mut imports = Vec::new();
    for res in results.into_iter().flatten() {
        nodes.extend(res.nodes);
        edges.extend(res.edges);
        raw_calls.extend(res.raw_calls);
        imports.extend(res.imports);
    }
    let (n_nodes, n_edges, n_calls) = (nodes.len(), edges.len(), raw_calls.len());
    m = stage("accumulate nodes/edges/calls", m);

    let md: Vec<_> = det
        .of(FileType::Document)
        .iter()
        .filter(|f| {
            matches!(
                f.extension().and_then(|e| e.to_str()),
                Some("md") | Some("mdx") | Some("qmd")
            )
        })
        .cloned()
        .collect();
    let md_results: Vec<_> = synaptic_extract::with_extraction_pool(|| {
        md.par_iter()
            .map(|file| {
                let rel = file.strip_prefix(&root).unwrap_or(file);
                let rel_str = rel.to_string_lossy();
                std::fs::read(file)
                    .ok()
                    .and_then(|b| cached_extract_source(Some(&cache_dir), rel_str.as_ref(), &b))
            })
            .collect()
    });
    for res in md_results.into_iter().flatten() {
        nodes.extend(res.nodes);
        edges.extend(res.edges);
    }
    m = stage("markdown structure", m);

    let aliases = load_alias_resolver(&root, &det.ts_config_files);
    resolve_imports(&mut nodes, &mut edges, &aliases);
    synaptic_extract::resolve_resource_refs(&mut nodes, &mut edges);
    m = stage("resolve imports + resources", m);

    let mut kg = build_from_parts(nodes, edges, vec![], &opts);
    m = stage("build_from_parts", m);

    let resolved = resolve_symbols(&kg, &raw_calls, &imports);
    let callnames = synaptic_incremental::from_raw_calls(&raw_calls);
    drop(raw_calls);
    drop(imports);
    std::hint::black_box(&callnames);
    m = stage("resolve_symbols (+ free calls)", m);

    let mut parts = kg.into_graph_data();
    parts.links.extend(resolved);
    let (n, e) = (parts.nodes, parts.links);
    let (n, e) = resolve_command_invocations(n, e);
    let (n, e) = resolve_route_handlers(n, e);
    let (n, e) = resolve_sql_queries(n, e);
    let (n, e) = resolve_parameterized_routes(n, e);
    let (n, e) = resolve_pyo3_modules(n, e);
    let (n, e) = resolve_pyo3_imports(n, e);
    let (n, e) = link_dynamic_refs(n, e);
    let (mut n, mut e) = deduplicate_entities(n, e, &std::collections::HashMap::new());
    m = stage("cross-language + dedup passes", m);

    let registry = synaptic_api::load_optional_registry(&root).ok().flatten();
    let (dependencies, sbom) =
        synaptic_api::scan_dependencies_and_sbom_evidence(&root).unwrap_or_default();
    if let Some(reg) = registry.as_ref() {
        synaptic_api::bind_repository_api_usages_with_dependencies(
            &mut n,
            &mut e,
            reg,
            &dependencies,
        );
    }
    synaptic_api::attach_api_coverage_with_evidence(
        &mut n,
        &mut e,
        &dependencies,
        registry.as_ref(),
        &sbom,
    );
    m = stage("api coverage overlay", m);

    kg = build_from_parts(n, e, parts.hyperedges, &opts);
    m = stage("rebuild graph", m);

    let confirmed =
        deterministic_tiebreak_candidates(kg.nodes(), &std::collections::HashMap::new());
    if !confirmed.is_empty() {
        let gd = kg.into_graph_data();
        let (mn, me) = merge_pairs(gd.nodes, gd.links, &confirmed);
        kg = build_from_parts(mn, me, gd.hyperedges, &opts);
    }
    m = stage("dedup tiebreaker", m);

    let communities = cluster(&kg, &ClusterOptions::default());
    apply_communities(&mut kg, &communities);
    m = stage("cluster + apply communities", m);

    let analysis = analyze(&kg, &communities, &Default::default());
    std::hint::black_box(&analysis);
    m = stage("analyze", m);

    let tmp = std::env::temp_dir().join("memprofile-out");
    let _ = std::fs::create_dir_all(&tmp);
    let out = tmp.join("graph.json");
    synaptic_output::to_json(&kg, &out).expect("write graph.json");
    m = stage("to_json (streamed)", m);

    // The tail stages the CLI runs after the graph is built. These are what the
    // core-pipeline profile above excludes, and the CLI's OS-level peak is far
    // above this profile's, so the difference has to be in here.
    synaptic_output::to_html(&kg, &tmp.join("graph.html")).expect("html");
    m = stage("to_html", m);

    synaptic_report::write_report(
        &kg,
        &analysis,
        &communities,
        &Default::default(),
        &tmp.join("GRAPH_REPORT.md"),
    )
    .expect("report");
    m = stage("write_report", m);

    synaptic_output::to_mermaid(&kg, &tmp.join("callflow.html")).expect("mermaid");
    synaptic_output::to_tree_html(&kg, &tmp.join("tree.html")).expect("tree");
    synaptic_output::to_svg(&kg, &tmp.join("graph.svg")).expect("svg");
    m = stage("mermaid + tree + svg", m);

    let export = kg.to_graph_data();
    m = stage("kg.to_graph_data() clone", m);

    let cov = synaptic_api::analyze_api_coverage_with_evidence(
        &export,
        &dependencies,
        registry.as_ref(),
        &[],
        &sbom,
    );
    std::hint::black_box(&cov);
    let _ = stage("api coverage report", m);

    println!();
    println!("corpus            : {n_code} code files");
    println!("pre-build         : {n_nodes} nodes, {n_edges} edges, {n_calls} raw calls");
    println!(
        "final graph       : {} nodes, {} edges, {} communities",
        kg.node_count(),
        kg.edge_count(),
        communities.len()
    );
    println!(
        "graph.json        : {:.1} MiB",
        std::fs::metadata(&out).map(|x| x.len()).unwrap_or(0) as f64 / 1048576.0
    );
    println!();
    println!(
        "PEAK ALLOCATED    : {:.1} MiB  (deterministic; live bytes, excludes allocator caching)",
        mib(global_peak())
    );
    println!("still live at exit: {:.1} MiB", mib(live()));
    let _ = std::fs::remove_file(&out);
}
