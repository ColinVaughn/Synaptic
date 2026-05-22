//! `query` command(s) split from main.rs.

use crate::commands::common::{
    default_graph_path, label_or_id, load_graph, load_scoped_graph, resolve,
};
use anyhow::Result;
use codegraph_query::{
    affected_nodes, explain, query_modal, resolve_seed, shortest_path, TraversalMode,
    DEFAULT_AFFECTED_RELATIONS,
};
use std::path::PathBuf;

pub(crate) fn run_query(
    text: &str,
    graph: Option<PathBuf>,
    max_nodes: usize,
    repo: Option<&str>,
    dfs: bool,
) -> Result<()> {
    let kg = load_scoped_graph(&default_graph_path(graph), repo)?;
    let mode = if dfs {
        TraversalMode::Dfs
    } else {
        TraversalMode::Bfs
    };
    let r = query_modal(&kg, text, max_nodes, mode);
    if r.seeds.is_empty() {
        println!("No matches for {text:?}.");
        return Ok(());
    }
    println!("Seeds:");
    for s in &r.seeds {
        println!("  - {}", label_or_id(&kg, s));
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

pub(crate) fn run_path(
    from: &str,
    to: &str,
    graph: Option<PathBuf>,
    repo: Option<&str>,
) -> Result<()> {
    let kg = load_scoped_graph(&default_graph_path(graph), repo)?;
    let (Some(a), Some(b)) = (resolve(&kg, from), resolve(&kg, to)) else {
        println!("Could not resolve one or both endpoints.");
        return Ok(());
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
    let Some(id) = resolve(&kg, node) else {
        println!("Node not found: {node}");
        return Ok(());
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
) -> Result<()> {
    let kg = load_graph(&default_graph_path(graph))?;
    let Some(seed) = resolve_seed(&kg, node) else {
        println!("No unique node match for {node}");
        return Ok(());
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
    for h in &hits {
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
    Ok(())
}
