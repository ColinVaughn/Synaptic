//! `extract` command(s) split from main.rs.

use anyhow::{Context, Result};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use synaptic_core::NodeId;
use synaptic_detect::{FileType, Manifest, detect};
use synaptic_extract::{
    ExtractionResult, attach_cpp_methods, cached_extract_source, load_alias_resolver,
    prune_local_sdk_candidates, resolve_imports,
};
use synaptic_graph::{
    BuildOptions, ClusterOptions, KnowledgeGraph, ambiguous_concept_pairs, analyze,
    apply_communities, build_from_parts, cluster, deduplicate_entities,
    deterministic_tiebreak_candidates, link_dynamic_refs, merge_pairs, resolve_command_invocations,
    resolve_parameterized_routes, resolve_pyo3_imports, resolve_pyo3_modules,
    resolve_route_handlers, resolve_sql_queries, resolve_symbols,
};
use synaptic_llm::{
    LlmClient, SemanticCache, build_client, default_concurrency, estimate_cost, resolve_backend,
};
use synaptic_output::{
    BULK_EXPORT_MAX_NODES, bulk_export_is_viable, to_cypher, to_dot, to_force3d, to_graphml,
    to_html, to_json, to_mermaid, to_obsidian, to_svg, to_tree_html, to_wiki,
};
use synaptic_report::write_report;
use synaptic_semantic::{label_communities, llm_tiebreak, run_semantic_pass};

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_extract(
    root: &Path,
    directed: bool,
    obsidian: bool,
    wiki: bool,
    semantic: bool,
    no_columns: bool,
    store: bool,
    no_resources: bool,
) -> Result<()> {
    // Process-wide SQL extraction switch; set before any file is extracted.
    if no_columns {
        synaptic_extract::set_emit_sql_columns(false);
        println!("note: --no-columns set; SQL column/index nodes will be skipped");
    }
    // Process-wide resource-indexing switch; set before any file is extracted.
    if no_resources {
        synaptic_extract::set_emit_resources(false);
        println!("note: --no-resources set; data/resource files will not be indexed");
    }
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;
    let det = detect(&root);
    let code_files = det.of(FileType::Code);
    println!(
        "Detected {} files ({} code, {} doc, {} paper) · ~{} words",
        det.total_files,
        code_files.len(),
        det.of(FileType::Document).len(),
        det.of(FileType::Paper).len(),
        det.total_words
    );
    if let Some(w) = &det.warning {
        println!("note: {w}");
    }

    let out_dir = root.join("synaptic-out");
    let cache_dir = out_dir.join("cache");
    // Provenance snapshot BEFORE extraction (extract is the longest build, so
    // the likeliest to overlap an edit): saving a post-extraction walk instead
    // would stamp a mid-build edit as seen without ever ingesting it.
    let manifest_snapshot = synaptic_incremental::snapshot_manifest(&out_dir, &det);

    // LLM client for the optional semantic pass + dedup tiebreaker. Opt-in
    // (`--semantic`) so we never make surprise paid API calls; needs a backend
    // env key. Tokio runtime created only when a client is available.
    let (llm, backend_name): (Option<Box<dyn LlmClient>>, Option<&'static str>) = if semantic {
        let get = |k: &str| std::env::var(k).ok();
        match resolve_backend(&get) {
            Some(name) => match build_client(name, &get) {
                Ok(c) => {
                    println!("Semantic pass: using the {name} backend");
                    (Some(c), Some(name))
                }
                Err(e) => {
                    eprintln!("note: --semantic set but backend init failed ({e}); skipping");
                    (None, None)
                }
            },
            None => {
                eprintln!("note: --semantic set but no API key detected; skipping semantic pass");
                (None, None)
            }
        }
    } else {
        (None, None)
    };
    // Cumulative LLM token usage across the run (extraction pass, the dominant
    // cost; the tiny tiebreaker/labeling prompts are not metered here), for the
    // end-of-run cost estimate.
    let mut llm_input_tokens: u64 = 0;
    let mut llm_output_tokens: u64 = 0;
    let rt = match &llm {
        Some(_) => Some(tokio::runtime::Runtime::new().context("starting async runtime")?),
        None => None,
    };
    let semantic_cache = SemanticCache::new(cache_dir.join("semantic"));

    // Extract files in parallel (rayon). Each file is read + extracted with the
    // path RELATIVE to root so node ids and source_file are portable across
    // machines/checkouts (the file-node id is make_id(path)). `map`+ordered
    // `collect` preserves the (path-sorted) input order, so the merge, and thus
    // graph.json, is deterministic regardless of thread scheduling. The AST
    // cache lets an unchanged file skip re-parsing on a rebuild.
    let results: Vec<Option<ExtractionResult>> = synaptic_extract::with_extraction_pool(|| {
        code_files
            .par_iter()
            .map(|file| {
                let rel = file.strip_prefix(&root).unwrap_or(file);
                let rel_str = rel.to_string_lossy();
                match std::fs::read(file) {
                    Ok(bytes) => cached_extract_source(Some(&cache_dir), rel_str.as_ref(), &bytes),
                    Err(e) => {
                        eprintln!("warning: failed to read {}: {e}", file.display());
                        None
                    }
                }
            })
            .collect()
    });

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut raw_calls = Vec::new();
    let mut imports = Vec::new();
    let mut extracted = 0usize;
    // A file whose grammar errored yields partial (often only file-node) output,
    // which is indistinguishable from a file that declares nothing. Track it so the
    // loss is reported instead of silently missing from the graph.
    let mut parse_errors = 0usize;
    let mut lost_files: Vec<String> = Vec::new();
    for (res, file) in results.into_iter().zip(code_files.iter()) {
        let Some(res) = res else { continue };
        extracted += 1;
        if res.parse_error {
            parse_errors += 1;
            // Only the file node survived: every symbol in this file is missing.
            if res.nodes.len() <= 1 {
                let rel = file.strip_prefix(&root).unwrap_or(file);
                lost_files.push(rel.to_string_lossy().into_owned());
            }
        }
        nodes.extend(res.nodes);
        edges.extend(res.edges);
        raw_calls.extend(res.raw_calls);
        imports.extend(res.imports);
    }
    // Bind JS/TS imports to real nodes now that the full file set is known (the
    // per-file extractor could only emit specifier stubs): relative code imports
    // -> file nodes, `@/...` aliases -> real files (via tsconfig paths), and
    // non-code imports (css/json/assets) -> distinct classified asset nodes.
    let aliases = load_alias_resolver(&root, &det.ts_config_files);
    let stats = resolve_imports(&mut nodes, &mut edges, &aliases);
    let local_sdk_calls = prune_local_sdk_candidates(&root, &mut nodes, &mut edges);
    println!(
        "Extracted {extracted} code files → {} nodes, {} edges (pre-build); \
         {} relative, {} alias, {} QL module, {} asset import(s) resolved ({} asset nodes)",
        nodes.len(),
        edges.len(),
        stats.relative_bound,
        stats.alias_bound,
        stats.ql_bound,
        stats.assets,
        stats.asset_nodes,
    );
    if parse_errors > 0 {
        let lost = lost_files.len();
        print!("Parse errors: {parse_errors} file(s) had grammar errors");
        if lost > 0 {
            let sample: Vec<&str> = lost_files.iter().take(3).map(String::as_str).collect();
            print!(
                "; {lost} yielded no symbols at all ({}{})",
                sample.join(", "),
                if lost > sample.len() { ", ..." } else { "" }
            );
        }
        println!();
    }
    if local_sdk_calls > 0 {
        println!("Dropped {local_sdk_calls} SDK candidate(s) that resolve to this repository");
    }
    // Markdown structure: heading hierarchy -> `document` nodes + `contains`
    // edges. Deterministic + cheap (a line scan), so it runs unconditionally,
    // independent of the opt-in LLM semantic pass, which still adds concepts on
    // top when `--semantic`. Markdown files are documents (not in `code_files`),
    // so they're extracted here via the same content-addressed AST cache.
    {
        let md_files: Vec<PathBuf> = det
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
        if !md_files.is_empty() {
            let md_results: Vec<Option<ExtractionResult>> =
                synaptic_extract::with_extraction_pool(|| {
                    md_files
                        .par_iter()
                        .map(|file| {
                            let rel = file.strip_prefix(&root).unwrap_or(file);
                            let rel_str = rel.to_string_lossy();
                            match std::fs::read(file) {
                                Ok(bytes) => cached_extract_source(
                                    Some(&cache_dir),
                                    rel_str.as_ref(),
                                    &bytes,
                                ),
                                Err(e) => {
                                    eprintln!("warning: failed to read {}: {e}", file.display());
                                    None
                                }
                            }
                        })
                        .collect()
                });
            let mut md_nodes = 0usize;
            for res in md_results.into_iter().flatten() {
                md_nodes += res.nodes.len();
                nodes.extend(res.nodes);
                edges.extend(res.edges);
            }
            if md_nodes > 0 {
                println!("Markdown structure: +{md_nodes} heading/file node(s)");
            }
        }
    }

    // Bind resource-file references only after Markdown file nodes have joined
    // the corpus. Incremental rebuilds already resolve over code + Markdown
    // together; keeping the full extractor in the same order prevents valid
    // references to README/docs files from being dropped in only one pipeline.
    let res_stats = synaptic_extract::resolve_resource_refs(&mut nodes, &mut edges);
    if res_stats.bound + res_stats.dropped + res_stats.shadows > 0 {
        println!(
            "Resources: {} reference(s) bound, {} dropped, {} generated-shadow edge(s)",
            res_stats.bound, res_stats.dropped, res_stats.shadows
        );
    }
    let attached_methods = attach_cpp_methods(&mut nodes, &mut edges);
    if attached_methods > 0 {
        println!("Attached {attached_methods} out-of-line C++ method definition(s)");
    }

    // Semantic pass: documents/papers -> concept nodes/edges via the LLM, merged
    // in before build (so ghost-remap can collapse concepts onto AST symbols).
    if let (Some(client), Some(rt)) = (&llm, &rt) {
        let mut docs: Vec<(String, String)> = Vec::new();
        for ft in [FileType::Document, FileType::Paper] {
            for f in det.of(ft) {
                let rel = f
                    .strip_prefix(&root)
                    .unwrap_or(f)
                    .to_string_lossy()
                    .into_owned();
                if let Ok(content) = std::fs::read_to_string(f) {
                    docs.push((rel, content));
                }
            }
        }
        if !docs.is_empty() {
            match rt.block_on(run_semantic_pass(
                client.as_ref(),
                docs,
                Some(&semantic_cache),
                backend_name.map(default_concurrency).unwrap_or(1),
            )) {
                Ok(out) => {
                    println!(
                        "Semantic pass: +{} concept node(s), +{} edge(s)",
                        out.nodes.len(),
                        out.edges.len()
                    );
                    llm_input_tokens += out.input_tokens as u64;
                    llm_output_tokens += out.output_tokens as u64;
                    nodes.extend(out.nodes);
                    edges.extend(out.edges);
                }
                Err(e) => eprintln!("note: semantic pass failed: {e}"),
            }
        }
    }

    let opts = BuildOptions {
        directed,
        root: Some(root.to_string_lossy().into_owned()),
    };
    let mut kg = build_from_parts(nodes, edges, vec![], &opts);

    // Cross-file symbol resolution: raw calls + import evidence -> `calls` edges
    // (import-guided EXTRACTED, single-candidate INFERRED). Needs the built
    // graph's index; the passes after it are pure (nodes, edges) transforms, so
    // they chain on owned vecs and the graph is rebuilt ONCE at the end (the
    // final build applies the same endpoint reconciliation + dedup to every
    // pass's additions). Mirrored by synaptic-incremental::rebuild_with_detect;
    // keep the pass order in sync.
    let resolved = resolve_symbols(&kg, &raw_calls, &imports);
    if !resolved.is_empty() {
        println!("Resolved {} cross-file call edge(s)", resolved.len());
    }
    // Seed the per-file called-name sidecar so the FIRST incremental update can
    // already ripple-re-resolve new definitions against unchanged callers. Built
    // here, right after the only other consumer, so the raw-call and import
    // vectors -- one entry per call site in the whole corpus -- are released
    // before the build/cluster/analyse/write stages instead of staying resident
    // for the rest of the run. `from_raw_calls` is a pure fold over the same
    // input, so moving it earlier cannot change the sidecar.
    let callnames = synaptic_incremental::from_raw_calls(&raw_calls);
    drop(raw_calls);
    drop(imports);

    let mut parts = kg.into_graph_data();
    parts.links.extend(resolved);
    let n = parts.nodes;
    let e = parts.links;

    // Cross-language: retarget subprocess command stubs to a matching in-repo file
    // node (e.g. Python subprocess.run("tool") -> the Rust binary src/bin/tool.rs).
    let before = n.len();
    let (n, e) = resolve_command_invocations(n, e);
    if n.len() < before {
        println!(
            "Resolved {} command invocation(s) to in-repo targets",
            before - n.len()
        );
    }

    // Cross-language: resolve named route handlers (axum `.route("/p", get(h))`)
    // to the handler fn when it is defined in another file. Runs before the
    // parameterized-route merge so a cross-file-resolved server route is not
    // mistaken for a client route.
    let before = e.len();
    let (n, e) = resolve_route_handlers(n, e);
    if e.len() > before {
        println!("Resolved {} cross-file route handler(s)", e.len() - before);
    }

    // Cross-language: collapse SQL table stubs (from scan_sql code-side detection)
    // into the real table node defined in a .sql file (same id, stub has empty
    // source_file). Runs after route resolution; edges are unchanged.
    let before = n.len();
    let (n, e) = resolve_sql_queries(n, e);
    if n.len() < before {
        println!("Resolved {} SQL table stub(s)", before - n.len());
    }

    // Cross-language: merge concrete client route paths into the parameterized
    // server route they match (/users/7 -> /users/{id}), so a client call connects
    // to the route's handler.
    let before = n.len();
    let (n, e) = resolve_parameterized_routes(n, e);
    if n.len() < before {
        println!("Resolved {} parameterized route(s)", before - n.len());
    }

    // Cross-language pyo3: first stitch each #[pymodule] boundary to the
    // #[pyfunction]/#[pyclass] definitions it registers (matched by name, across
    // files), then connect Python importers of the module to that boundary -- so
    // impact crosses from the Rust impl to the Python caller even when the module
    // and the function are in different files.
    let before = e.len();
    let (n, e) = resolve_pyo3_modules(n, e);
    let (n, e) = resolve_pyo3_imports(n, e);
    if e.len() > before {
        println!(
            "Connected {} pyo3 edge(s) (module exports + imports)",
            e.len() - before
        );
    }

    // Dynamic-dispatch evidence-link: resolve reflection sites whose key is a
    // string literal to their unique defining symbol, adding a low-confidence
    // `dynamic_ref` edge so the target shows up as a (caveated) dependent and is
    // flagged `dynamically_referenced`. Runs after cross-language resolution so the
    // full symbol set (incl. cross-file/cross-repo) is present.
    let before = e.len();
    let (n, e) = link_dynamic_refs(n, e);
    if e.len() > before {
        println!(
            "Evidence-linked {} dynamic-dispatch site(s)",
            e.len() - before
        );
    }

    // Merge near-duplicate non-code entities (documents/concepts). A no-op on a
    // code-only graph (code symbols are keyed by id, never label-merged) but
    // ready for the semantic layer (B5). Community boost is off here (no
    // communities yet).
    let before = n.len();
    let (mut n, mut e) = deduplicate_entities(n, e, &std::collections::HashMap::new());
    if n.len() < before {
        println!("Deduplicated {} node(s)", before - n.len());
    }

    // Overlay vendor-scoped API operation anchors after route normalization and
    // entity dedup. Missing config is an opt-out; malformed opted-in config is a
    // hard error. The incremental pipeline calls the same pure binder here.
    let registry = synaptic_api::load_optional_registry(&root)?;
    let (dependencies, sbom) = synaptic_api::scan_graph_dependency_evidence(&root)?;
    if let Some(registry) = registry.as_ref() {
        let report = synaptic_api::bind_repository_api_usages_with_dependencies(
            &mut n,
            &mut e,
            registry,
            &dependencies,
        );
        if report.operations > 0 || report.sdk_packages > 0 || report.ambiguous > 0 {
            println!(
                "API bindings: {} usage(s) across {} operation(s), {} SDK package(s), and {} vendor(s){}",
                report.usages,
                report.operations,
                report.sdk_packages,
                report.vendors,
                if report.ambiguous > 0 {
                    format!(", {} ambiguous host match(es) skipped", report.ambiguous)
                } else {
                    String::new()
                }
            );
        }
    }
    synaptic_api::attach_api_coverage_with_evidence(
        &mut n,
        &mut e,
        &dependencies,
        registry.as_ref(),
        &sbom,
    );

    kg = build_from_parts(n, e, parts.hyperedges, &opts);

    // Dedup tiebreaker: resolve ambiguous concept pairs (fuzzy score in the 75-92
    // band) the structural pass left unmerged. With an LLM (--semantic) the model
    // decides each pair; offline a conservative deterministic rule merges only
    // word-reorderings/duplications and flags the genuinely ambiguous rest for
    // review (auto-merging the band offline would risk corrupting the graph).
    //
    // Candidates are computed from BORROWED nodes and the graph is CONSUMED to
    // apply a merge. The previous version cloned every node unconditionally --
    // and then every edge -- while `kg` was still alive, so three full copies of
    // the graph existed at once even when it went on to merge nothing. On a
    // 379k-node corpus that clone-and-rebuild was the run's peak (4.4 -> 8.7 GiB)
    // and it merged 3 pairs.
    {
        let no_communities = std::collections::HashMap::new();
        let (confirmed, total) = match (&llm, &rt) {
            (Some(client), Some(rt)) => {
                let pairs = ambiguous_concept_pairs(kg.nodes(), &no_communities);
                let total = pairs.len();
                if pairs.is_empty() {
                    (Vec::new(), 0)
                } else {
                    // The LLM tiebreaker needs an owned slice; only pay for the
                    // clone when there is actually something ambiguous to judge.
                    let nodes_vec: Vec<_> = kg.nodes().cloned().collect();
                    let confirmed = rt.block_on(llm_tiebreak(client.as_ref(), &nodes_vec, pairs));
                    (confirmed, total)
                }
            }
            _ => {
                let confirmed = deterministic_tiebreak_candidates(kg.nodes(), &no_communities);
                let total = confirmed.len();
                (confirmed, total)
            }
        };

        if !confirmed.is_empty() {
            let gd = kg.into_graph_data();
            let (mn, me) = merge_pairs(gd.nodes, gd.links, &confirmed);
            kg = build_from_parts(mn, me, gd.hyperedges, &opts);
        }

        if total > 0 {
            if llm.is_some() {
                let flagged = total - confirmed.len();
                println!(
                    "Dedup tiebreaker (LLM): merged {} of {total} ambiguous concept pair(s){}",
                    confirmed.len(),
                    if flagged > 0 {
                        format!(", {flagged} left for review")
                    } else {
                        String::new()
                    }
                );
            } else if !confirmed.is_empty() {
                println!(
                    "Dedup tiebreaker (deterministic indexed): merged {} confirmed pair(s)",
                    confirmed.len()
                );
            }
        }
    }

    kg.built_at_commit = synaptic_incremental::git_head(&root);
    let communities = cluster(&kg, &ClusterOptions::default());
    apply_communities(&mut kg, &communities);
    // Name communities via the LLM when a backend is available (semantic mode);
    // otherwise labels stay empty and everything falls back to `Community N`.
    let community_labels = match (&llm, &rt) {
        (Some(client), Some(rt)) => {
            let members = community_member_labels(&kg, &communities);
            rt.block_on(label_communities(client.as_ref(), &members))
        }
        _ => BTreeMap::new(),
    };
    let analysis = analyze(&kg, &communities, &community_labels);
    let extras = write_outputs(
        &kg,
        &analysis,
        &communities,
        &community_labels,
        &out_dir,
        obsidian,
        wiki,
    )?;
    // ONE export view, shared by the coverage refresh and the store writer
    // below. Each `to_graph_data()` deep-clones every node and edge -- about
    // 1.8 GiB on a 379k-node graph -- and this path used to pay for it twice.
    let export = kg.to_graph_data();
    let coverage = crate::commands::api::refresh_coverage_artifact_with_inventory(
        &root,
        &out_dir,
        &export,
        &dependencies,
        &sbom,
    )?;

    println!(
        "Built graph: {} nodes · {} edges · {} communities",
        kg.node_count(),
        kg.edge_count(),
        communities.len()
    );
    if !coverage.observations.is_empty() || !coverage.development_dependencies.is_empty() {
        println!(
            "API coverage: {} observation(s), {} unresolved gap(s), {} development negative control(s)",
            coverage.observations.len(),
            coverage.gaps.len(),
            coverage.development_dependencies.len()
        );
    }
    if let Some(top) = analysis.god_nodes.first() {
        println!("Top god node: {} (degree {})", top.label, top.degree);
    }
    // Estimated LLM spend for the semantic extraction pass (zero on a cache hit
    // or for local/subscription backends). Reported only when a paid call was made.
    if let Some(name) = backend_name
        && (llm_input_tokens > 0 || llm_output_tokens > 0)
    {
        let cost = estimate_cost(name, llm_input_tokens, llm_output_tokens);
        println!(
            "LLM usage (extraction pass): {llm_input_tokens} input + {llm_output_tokens} \
                 output tokens (~${cost:.4} estimated on {name})"
        );
    }
    // Git-free change-detection ledger: rebuild the per-file manifest with the
    // stat-index fastpath (unchanged mtime -> skip re-hash), report what changed
    // since the previous build, and persist it for next time.
    {
        let manifest_path = cache_dir.join("manifest.json");
        let prior = Manifest::load(&manifest_path);
        let current =
            Manifest::build_incremental(code_files.iter().map(|p| p.as_path()), &root, &prior);
        if !prior.0.is_empty() {
            let d = prior.diff(&current);
            if !d.added.is_empty() || !d.changed.is_empty() || !d.removed.is_empty() {
                println!(
                    "Changes since last build: +{} added · ~{} changed · -{} removed",
                    d.added.len(),
                    d.changed.len(),
                    d.removed.len()
                );
            }
        }
        if let Err(e) = current.save(&manifest_path) {
            eprintln!("note: could not write change-detection manifest: {e}");
        }
    }
    // Serve catch-up provenance: a code+markdown manifest at a stable location
    // (survives `cache clear`) that `synaptic serve` diffs against to learn which
    // files an agent added/changed since this build. Distinct from the cache-dir
    // manifest above, which only drives the "changes since last build" line.
    if let Err(e) = manifest_snapshot.save(&synaptic_incremental::manifest_path(&out_dir)) {
        eprintln!("note: could not write serve provenance manifest: {e}");
    }
    // Sidecar was built right after symbol resolution (see above), so the
    // raw-call vector could be freed before the heavy stages.
    if let Err(e) = synaptic_incremental::save_callnames(&out_dir, &callnames) {
        eprintln!("note: could not write call-name sidecar: {e}");
    }
    println!("Wrote {}/{{{}}}", out_dir.display(), extras);
    if store {
        let report = crate::commands::common::write_store(export, &out_dir.join("store"))?;
        println!(
            "Wrote {}/store ({} shard(s){})",
            out_dir.display(),
            report.shard_tags.len(),
            if report.bridge_edges > 0 {
                format!(", {} bridge edge(s)", report.bridge_edges)
            } else {
                String::new()
            }
        );
    }
    Ok(())
}

/// Representative member labels per community (highest-degree first), for the LLM
/// community-naming prompt.
pub(crate) fn community_member_labels(
    kg: &KnowledgeGraph,
    communities: &BTreeMap<u32, Vec<NodeId>>,
) -> BTreeMap<u32, Vec<String>> {
    let mut deg: std::collections::HashMap<&NodeId, usize> = std::collections::HashMap::new();
    for e in kg.edges() {
        if e.source == e.target || !synaptic_graph::is_structural_edge(e) {
            continue;
        }
        *deg.entry(&e.source).or_default() += 1;
        *deg.entry(&e.target).or_default() += 1;
    }
    let mut out = BTreeMap::new();
    for (cid, members) in communities {
        let mut ranked: Vec<&NodeId> = members.iter().collect();
        ranked.sort_by(|a, b| {
            deg.get(b)
                .copied()
                .unwrap_or(0)
                .cmp(&deg.get(a).copied().unwrap_or(0))
        });
        let labels: Vec<String> = ranked
            .iter()
            .filter_map(|id| kg.node(id).map(|n| n.label.clone()))
            .take(8)
            .collect();
        out.insert(*cid, labels);
    }
    out
}

/// Write the standard artifact set for a built+clustered graph; returns the
/// human-readable list of what was written. Shared by `extract` and `update`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_outputs(
    kg: &KnowledgeGraph,
    analysis: &synaptic_graph::AnalysisResult,
    communities: &BTreeMap<u32, Vec<NodeId>>,
    community_labels: &BTreeMap<u32, String>,
    out_dir: &Path,
    obsidian: bool,
    wiki: bool,
) -> Result<String> {
    to_json(kg, &out_dir.join("graph.json")).context("writing graph.json")?;
    super::common::warn_if_over_caps(&out_dir.join("graph.json"), kg.node_count());
    to_html(kg, &out_dir.join("graph.html")).context("writing graph.html")?;
    write_report(
        kg,
        analysis,
        communities,
        community_labels,
        &out_dir.join("GRAPH_REPORT.md"),
    )
    .context("writing GRAPH_REPORT.md")?;
    to_mermaid(kg, &out_dir.join("callflow.html")).context("writing callflow.html")?;
    to_tree_html(kg, &out_dir.join("tree.html")).context("writing tree.html")?;
    to_svg(kg, &out_dir.join("graph.svg")).context("writing graph.svg")?;
    let mut extras = String::from(
        "graph.json, graph.html, GRAPH_REPORT.md, callflow.html, tree.html, graph.svg",
    );

    // Whole-graph text exports aggregate nothing, so their size tracks the graph
    // exactly and they stop being openable long before they stop being written.
    if bulk_export_is_viable(kg.node_count()) {
        to_graphml(kg, &out_dir.join("graph.graphml")).context("writing graph.graphml")?;
        to_cypher(kg, &out_dir.join("graph.cypher")).context("writing graph.cypher")?;
        to_dot(kg, &out_dir.join("graph.dot")).context("writing graph.dot")?;
        to_force3d(kg, &out_dir.join("graph-3d.html")).context("writing graph-3d.html")?;
        extras.push_str(", graph.graphml, graph.cypher, graph.dot, graph-3d.html");
    } else {
        println!(
            "note: {} nodes exceeds the {BULK_EXPORT_MAX_NODES}-node bulk-export cap; \
             skipped graph.graphml, graph.cypher, graph.dot and graph-3d.html \
             (graph.json and graph.html are always written)",
            kg.node_count()
        );
    }
    if obsidian {
        let n = to_obsidian(kg, community_labels, &out_dir.join("obsidian"))
            .context("writing Obsidian vault")?;
        extras.push_str(&format!(", obsidian/ ({n} notes)"));
    }
    if wiki {
        let n = to_wiki(kg, community_labels, &out_dir.join("wiki")).context("writing wiki")?;
        extras.push_str(&format!(", wiki/ ({n} pages)"));
    }
    Ok(extras)
}
