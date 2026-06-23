//! `extract` command(s) split from main.rs.

use anyhow::{Context, Result};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use synaptic_core::NodeId;
use synaptic_detect::{detect, FileType, Manifest};
use synaptic_extract::{
    cached_extract_source, load_alias_resolver, resolve_imports, ExtractionResult,
};
use synaptic_graph::{
    ambiguous_concept_pairs, analyze, apply_communities, build_from_parts, cluster,
    deduplicate_entities, deterministic_tiebreak, link_dynamic_refs, merge_pairs,
    resolve_command_invocations, resolve_parameterized_routes, resolve_pyo3_imports,
    resolve_pyo3_modules, resolve_route_handlers, resolve_sql_queries, resolve_symbols,
    BuildOptions, ClusterOptions, KnowledgeGraph,
};
use synaptic_llm::{
    build_client, default_concurrency, estimate_cost, resolve_backend, LlmClient, SemanticCache,
};
use synaptic_output::{
    to_cypher, to_dot, to_force3d, to_graphml, to_html, to_json, to_mermaid, to_obsidian, to_svg,
    to_tree_html, to_wiki,
};
use synaptic_report::write_report;
use synaptic_semantic::{label_communities, llm_tiebreak, run_semantic_pass};

pub(crate) fn run_extract(
    root: &Path,
    directed: bool,
    obsidian: bool,
    wiki: bool,
    semantic: bool,
    no_columns: bool,
) -> Result<()> {
    // Process-wide SQL extraction switch; set before any file is extracted.
    if no_columns {
        synaptic_extract::set_emit_sql_columns(false);
        println!("note: --no-columns set; SQL column/index nodes will be skipped");
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
    let results: Vec<Option<ExtractionResult>> = code_files
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
        .collect();

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut raw_calls = Vec::new();
    let mut imports = Vec::new();
    let mut extracted = 0usize;
    for res in results.into_iter().flatten() {
        extracted += 1;
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
    println!(
        "Extracted {extracted} code files → {} nodes, {} edges (pre-build); \
         {} relative, {} alias, {} asset import(s) resolved ({} asset nodes)",
        nodes.len(),
        edges.len(),
        stats.relative_bound,
        stats.alias_bound,
        stats.assets,
        stats.asset_nodes,
    );

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
            let md_results: Vec<Option<ExtractionResult>> = md_files
                .par_iter()
                .map(|file| {
                    let rel = file.strip_prefix(&root).unwrap_or(file);
                    let rel_str = rel.to_string_lossy();
                    match std::fs::read(file) {
                        Ok(bytes) => {
                            cached_extract_source(Some(&cache_dir), rel_str.as_ref(), &bytes)
                        }
                        Err(e) => {
                            eprintln!("warning: failed to read {}: {e}", file.display());
                            None
                        }
                    }
                })
                .collect();
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
    // (import-guided EXTRACTED, single-candidate INFERRED). Rebuild so the new
    // edges go through the same endpoint reconciliation + dedup as everything else.
    let resolved = resolve_symbols(&kg, &raw_calls, &imports);
    if !resolved.is_empty() {
        let n = resolved.len();
        let nodes2: Vec<_> = kg.nodes().cloned().collect();
        let mut edges2: Vec<_> = kg.edges().cloned().collect();
        edges2.extend(resolved);
        kg = build_from_parts(nodes2, edges2, vec![], &opts);
        println!("Resolved {n} cross-file call edge(s)");
    }

    // Cross-language: retarget subprocess command stubs to a matching in-repo file
    // node (e.g. Python subprocess.run("tool") -> the Rust binary src/bin/tool.rs).
    let before_cmd = kg.node_count();
    let (cn, ce) =
        resolve_command_invocations(kg.nodes().cloned().collect(), kg.edges().cloned().collect());
    if cn.len() < before_cmd {
        let n = before_cmd - cn.len();
        kg = build_from_parts(cn, ce, vec![], &opts);
        println!("Resolved {n} command invocation(s) to in-repo targets");
    }

    // Cross-language: resolve named route handlers (axum `.route("/p", get(h))`)
    // to the handler fn when it is defined in another file. Runs before the
    // parameterized-route merge so a cross-file-resolved server route is not
    // mistaken for a client route.
    let before_edges = kg.edges().count();
    let (hn, he) =
        resolve_route_handlers(kg.nodes().cloned().collect(), kg.edges().cloned().collect());
    if he.len() > before_edges {
        let n = he.len() - before_edges;
        kg = build_from_parts(hn, he, vec![], &opts);
        println!("Resolved {n} cross-file route handler(s)");
    }

    // Cross-language: collapse SQL table stubs (from scan_sql code-side detection)
    // into the real table node defined in a .sql file (same id, stub has empty
    // source_file). Runs after route resolution; edges are unchanged.
    let before_sql = kg.node_count();
    let (sn, se) =
        resolve_sql_queries(kg.nodes().cloned().collect(), kg.edges().cloned().collect());
    if sn.len() < before_sql {
        let n = before_sql - sn.len();
        kg = build_from_parts(sn, se, vec![], &opts);
        println!("Resolved {n} SQL table stub(s)");
    }

    // Cross-language: merge concrete client route paths into the parameterized
    // server route they match (/users/7 -> /users/{id}), so a client call connects
    // to the route's handler.
    let before_routes = kg.node_count();
    let (rn, re) =
        resolve_parameterized_routes(kg.nodes().cloned().collect(), kg.edges().cloned().collect());
    if rn.len() < before_routes {
        let n = before_routes - rn.len();
        kg = build_from_parts(rn, re, vec![], &opts);
        println!("Resolved {n} parameterized route(s)");
    }

    // Cross-language pyo3: first stitch each #[pymodule] boundary to the
    // #[pyfunction]/#[pyclass] definitions it registers (matched by name, across
    // files), then connect Python importers of the module to that boundary -- so
    // impact crosses from the Rust impl to the Python caller even when the module
    // and the function are in different files.
    let before_edges = kg.edges().count();
    let (pn, pe) =
        resolve_pyo3_modules(kg.nodes().cloned().collect(), kg.edges().cloned().collect());
    let (pn, pe) = resolve_pyo3_imports(pn, pe);
    if pe.len() > before_edges {
        let n = pe.len() - before_edges;
        kg = build_from_parts(pn, pe, vec![], &opts);
        println!("Connected {n} pyo3 edge(s) (module exports + imports)");
    }

    // Dynamic-dispatch evidence-link: resolve reflection sites whose key is a
    // string literal to their unique defining symbol, adding a low-confidence
    // `dynamic_ref` edge so the target shows up as a (caveated) dependent and is
    // flagged `dynamically_referenced`. Runs after cross-language resolution so the
    // full symbol set (incl. cross-file/cross-repo) is present.
    let before_edges = kg.edges().count();
    let (yn, ye) = link_dynamic_refs(kg.nodes().cloned().collect(), kg.edges().cloned().collect());
    if ye.len() > before_edges {
        let n = ye.len() - before_edges;
        kg = build_from_parts(yn, ye, vec![], &opts);
        println!("Evidence-linked {n} dynamic-dispatch site(s)");
    }

    // Merge near-duplicate non-code entities (documents/concepts). A no-op on a
    // code-only graph (code symbols are keyed by id, never label-merged) but
    // ready for the semantic layer (B5). Community boost is off here (no
    // communities yet); rebuild only when a merge actually happened.
    let before = kg.node_count();
    let (dn, de) = deduplicate_entities(
        kg.nodes().cloned().collect(),
        kg.edges().cloned().collect(),
        &std::collections::HashMap::new(),
    );
    if dn.len() < before {
        let merged = before - dn.len();
        kg = build_from_parts(dn, de, vec![], &opts);
        println!("Deduplicated {merged} node(s)");
    }

    // Dedup tiebreaker: resolve ambiguous concept pairs (fuzzy score in the 75-92
    // band) the structural pass left unmerged. With an LLM (--semantic) the model
    // decides each pair; offline a conservative deterministic rule merges only
    // word-reorderings/duplications and flags the genuinely ambiguous rest for
    // review (auto-merging the band offline would risk corrupting the graph).
    {
        let nodes_vec: Vec<_> = kg.nodes().cloned().collect();
        let pairs = ambiguous_concept_pairs(&nodes_vec, &std::collections::HashMap::new());
        if !pairs.is_empty() {
            let total = pairs.len();
            let confirmed = match (&llm, &rt) {
                (Some(client), Some(rt)) => {
                    rt.block_on(llm_tiebreak(client.as_ref(), &nodes_vec, pairs))
                }
                _ => deterministic_tiebreak(&nodes_vec, &pairs),
            };
            let flagged = total - confirmed.len();
            if !confirmed.is_empty() {
                let edges_vec: Vec<_> = kg.edges().cloned().collect();
                let (mn, me) = merge_pairs(nodes_vec, edges_vec, &confirmed);
                kg = build_from_parts(mn, me, vec![], &opts);
            }
            let how = if llm.is_some() {
                "LLM"
            } else {
                "deterministic"
            };
            println!(
                "Dedup tiebreaker ({how}): merged {} of {total} ambiguous concept pair(s){}",
                confirmed.len(),
                if flagged > 0 {
                    format!(", {flagged} left for review")
                } else {
                    String::new()
                }
            );
        }
    }

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

    println!(
        "Built graph: {} nodes · {} edges · {} communities",
        kg.node_count(),
        kg.edge_count(),
        communities.len()
    );
    if let Some(top) = analysis.god_nodes.first() {
        println!("Top god node: {} (degree {})", top.label, top.degree);
    }
    // Estimated LLM spend for the semantic extraction pass (zero on a cache hit
    // or for local/subscription backends). Reported only when a paid call was made.
    if let Some(name) = backend_name {
        if llm_input_tokens > 0 || llm_output_tokens > 0 {
            let cost = estimate_cost(name, llm_input_tokens, llm_output_tokens);
            println!(
                "LLM usage (extraction pass): {llm_input_tokens} input + {llm_output_tokens} \
                 output tokens (~${cost:.4} estimated on {name})"
            );
        }
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
    if let Err(e) = synaptic_incremental::persist_manifest_with(&out_dir, &det) {
        eprintln!("note: could not write serve provenance manifest: {e}");
    }
    println!("Wrote {}/{{{}}}", out_dir.display(), extras);
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
        if e.source == e.target {
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
    to_html(kg, &out_dir.join("graph.html")).context("writing graph.html")?;
    write_report(
        kg,
        analysis,
        communities,
        community_labels,
        &out_dir.join("GRAPH_REPORT.md"),
    )
    .context("writing GRAPH_REPORT.md")?;
    to_graphml(kg, &out_dir.join("graph.graphml")).context("writing graph.graphml")?;
    to_cypher(kg, &out_dir.join("graph.cypher")).context("writing graph.cypher")?;
    to_mermaid(kg, &out_dir.join("callflow.html")).context("writing callflow.html")?;
    to_tree_html(kg, &out_dir.join("tree.html")).context("writing tree.html")?;
    to_svg(kg, &out_dir.join("graph.svg")).context("writing graph.svg")?;
    to_dot(kg, &out_dir.join("graph.dot")).context("writing graph.dot")?;
    to_force3d(kg, &out_dir.join("graph-3d.html")).context("writing graph-3d.html")?;
    let mut extras = String::from("graph.json, graph.html, GRAPH_REPORT.md, graph.graphml, graph.cypher, graph.dot, callflow.html, tree.html, graph.svg, graph-3d.html");
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
