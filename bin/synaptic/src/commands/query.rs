//! `query` command(s) split from main.rs.

use crate::commands::common::{
    default_graph_path, label_or_id, load_graph, load_scoped_graph, resolve_or_message,
};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use synaptic_core::NodeId;
use synaptic_graph::KnowledgeGraph;
use synaptic_query::{
    affected_nodes, explain, query_modal, shortest_path, QueryIndex, Recency, RecencyMode,
    TraversalMode, DEFAULT_AFFECTED_RELATIONS,
};

/// `query` recency-boost strength (mirrors the MCP server's RECENCY_BOOST).
const RECENCY_BOOST: f64 = 4.0;

/// Resolved `--since` signal: (changed node ids, per-node churn weight, base
/// label, changed-file count).
type ResolvedRecency = (HashSet<NodeId>, HashMap<NodeId, f64>, String, usize);

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_query(
    text: &str,
    graph: Option<PathBuf>,
    max_nodes: usize,
    repo: Option<&str>,
    dfs: bool,
    since: Option<&str>,
    seed_changed: bool,
) -> Result<()> {
    let kg = load_scoped_graph(&default_graph_path(graph), repo)?;
    let mode = if dfs {
        TraversalMode::Dfs
    } else {
        TraversalMode::Bfs
    };
    // Resolve the changed-files set when --since is given (current dir = repo root).
    let resolved = since.and_then(|s| resolve_recency_cli(&kg, Path::new("."), s));
    if let Some((_, _, label, n_files)) = resolved.as_ref() {
        println!("Recency: since {label} | {n_files} changed file(s)");
    } else if since.is_some() {
        println!("Recency: unavailable (not a git repo, bad ref, or no changes) — plain query.");
    }
    let rec = resolved.as_ref().map(|(changed, churn, _, _)| Recency {
        changed,
        churn: Some(churn),
        mode: if seed_changed {
            RecencyMode::Seed
        } else {
            RecencyMode::Boost
        },
        boost: RECENCY_BOOST,
    });
    let r = match &rec {
        Some(_) => {
            QueryIndex::build(&kg).query_with_recency(&kg, text, max_nodes, mode, rec.as_ref())
        }
        None => query_modal(&kg, text, max_nodes, mode),
    };
    if r.seeds.is_empty() && r.nodes.is_empty() {
        println!("No matches for {text:?}.");
        return Ok(());
    }
    println!("Seeds:");
    for s in &r.seeds {
        println!("  - {}", label_or_id(&kg, s));
    }
    let changed_set = resolved.as_ref().map(|(c, ..)| c);
    println!("\nRanked nodes ({}):", r.nodes.len());
    for (id, score) in r.nodes.iter().zip(r.scores.iter()) {
        let mark = if changed_set.is_some_and(|c| c.contains(id)) {
            " (changed)"
        } else {
            ""
        };
        println!("  [{score:.2}]{mark} {}", label_or_id(&kg, id));
    }
    println!(
        "\nSubgraph ({} nodes, {} edges):",
        r.nodes.len(),
        r.edges.len()
    );
    for e in &r.edges {
        println!(
            "  {} --{}--> {}",
            label_or_id(&kg, &e.source),
            e.relation,
            label_or_id(&kg, &e.target)
        );
    }
    Ok(())
}

/// Resolve `--since` to (changed node ids, per-node churn weight, base label,
/// changed-file count) via git, or `None` if git is unavailable / nothing changed.
/// Scope: merge-base(SINCE, HEAD)..working-tree (includes uncommitted edits).
/// Mirrors the MCP server's `resolve_recency`, but shells git directly (the CLI is
/// not sandboxed).
fn resolve_recency_cli(kg: &KnowledgeGraph, root: &Path, since: &str) -> Option<ResolvedRecency> {
    use synaptic_history::git;
    // Base ref: try as a git rev, then as a date.
    let base = git::rev_parse(root, since)
        .or_else(|_| git::rev_before(root, since))
        .ok()?;
    let mb = git::merge_base(root, &base, "HEAD").unwrap_or(base);
    let rows = git::numstat(root, &mb, None).ok()?;

    let mut file_churn: HashMap<String, usize> = HashMap::new();
    for (a, d, p) in rows {
        *file_churn.entry(p.replace('\\', "/")).or_default() += a + d;
    }
    if file_churn.is_empty() {
        return None;
    }
    let max = file_churn.values().copied().max().unwrap_or(1).max(1) as f64;
    let denom = (1.0 + max).ln();

    let mut changed = HashSet::new();
    let mut churn = HashMap::new();
    for n in kg.nodes() {
        let sf = n.source_file.replace('\\', "/");
        if let Some(&lines) = file_churn.get(&sf) {
            let w = if lines == 0 {
                0.1
            } else {
                ((1.0 + lines as f64).ln() / denom).max(0.1)
            };
            changed.insert(n.id.clone());
            churn.insert(n.id.clone(), w);
        }
    }
    if changed.is_empty() {
        return None;
    }
    let short = &mb[..mb.len().min(7)];
    Some((
        changed,
        churn,
        format!("{since} (merge-base {short})"),
        file_churn.len(),
    ))
}

pub(crate) fn run_path(
    from: &str,
    to: &str,
    graph: Option<PathBuf>,
    repo: Option<&str>,
) -> Result<()> {
    let kg = load_scoped_graph(&default_graph_path(graph), repo)?;
    let a = match resolve_or_message(&kg, from) {
        Ok(id) => id,
        Err(msg) => {
            println!("source: {msg}");
            return Ok(());
        }
    };
    let b = match resolve_or_message(&kg, to) {
        Ok(id) => id,
        Err(msg) => {
            println!("target: {msg}");
            return Ok(());
        }
    };
    match shortest_path(&kg, &a, &b) {
        Some(path) => {
            let labels: Vec<String> = path.iter().map(|id| label_or_id(&kg, id)).collect();
            println!("{}", labels.join(" → "));
        }
        None => println!("No path between {from} and {to}."),
    }
    Ok(())
}

pub(crate) fn run_explain(node: &str, graph: Option<PathBuf>, repo: Option<&str>) -> Result<()> {
    let kg = load_scoped_graph(&default_graph_path(graph), repo)?;
    let id = match resolve_or_message(&kg, node) {
        Ok(id) => id,
        Err(msg) => {
            println!("{msg}");
            return Ok(());
        }
    };
    let e = explain(&kg, &id).expect("resolved node exists");
    println!("{} [{}]", e.label, e.source_file);
    if let Some(c) = e.community {
        println!("community: {c}");
    }
    println!("neighbours ({}):", e.neighbors.len());
    for nb in &e.neighbors {
        let arrow = if nb.direction == "out" { "-->" } else { "<--" };
        println!("  {arrow} {} ({})", nb.label, nb.relation);
    }
    Ok(())
}

pub(crate) fn run_affected(
    node: &str,
    graph: Option<PathBuf>,
    depth: usize,
    relations: Vec<String>,
    limit: usize,
    verbose: bool,
) -> Result<()> {
    let kg = load_graph(&default_graph_path(graph))?;
    let seed = match resolve_or_message(&kg, node) {
        Ok(id) => id,
        Err(msg) => {
            println!("{msg}");
            return Ok(());
        }
    };
    // Default to the structural impact relations when none are given.
    let rel_owned: Vec<String> = if relations.is_empty() {
        DEFAULT_AFFECTED_RELATIONS
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        relations
    };
    let rel_refs: Vec<&str> = rel_owned.iter().map(String::as_str).collect();
    let hits = affected_nodes(&kg, &seed, &rel_refs, depth);

    println!("Affected nodes for {}", label_or_id(&kg, &seed));
    println!("Relations: {}", rel_owned.join(", "));
    println!("Depth: {depth}");
    if hits.is_empty() {
        println!("No affected nodes found.");
        return Ok(());
    }
    // Per-depth breakdown so a hub's blast radius is summarized even when the list
    // is truncated (mirrors the MCP `affected` tool).
    let mut by_depth: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    for h in &hits {
        *by_depth.entry(h.depth).or_default() += 1;
    }
    let breakdown = by_depth
        .iter()
        .map(|(d, c)| format!("depth {d}: {c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let cap = if verbose { usize::MAX } else { limit.max(1) };
    println!("Total: {} [{breakdown}]", hits.len());
    for h in hits.iter().take(cap) {
        let loc = kg
            .node(&h.node_id)
            .map(|n| match &n.source_location {
                Some(l) if !l.is_empty() => format!("{}:{}", n.source_file, l),
                _ => {
                    if n.source_file.is_empty() {
                        "-".to_string()
                    } else {
                        n.source_file.clone()
                    }
                }
            })
            .unwrap_or_else(|| "-".to_string());
        println!(
            "- {} [{}] {}",
            label_or_id(&kg, &h.node_id),
            h.via_relation,
            loc
        );
    }
    if hits.len() > cap {
        println!(
            "... (+{} more; pass --verbose for the full list)",
            hits.len() - cap
        );
    }
    Ok(())
}
