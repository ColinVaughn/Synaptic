//! `export` command(s) split from main.rs.

use crate::commands::common::{load_scoped_graph, write_file};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use synaptic_graph::{analyze, apply_communities, cluster, ClusterOptions, KnowledgeGraph};
use synaptic_output::{
    to_cypher, to_dot, to_force3d, to_graphml, to_html, to_json, to_mermaid, to_obsidian, to_svg,
    to_tree_html, to_wiki,
};
use synaptic_report::write_report;

/// `synaptic export <format>`: regenerate one output from an existing
/// graph.json without re-extracting, or push it live to Neo4j/FalkorDB.
pub(crate) fn run_export(
    format: &str,
    graph: Option<PathBuf>,
    out: Option<PathBuf>,
    push: Option<String>,
    user: &str,
    password: Option<String>,
    repo: Option<String>,
) -> Result<()> {
    // `user`/`password` are only consumed by the (feature-gated) live-push arms.
    let _ = (&user, &password);
    let graph_path = graph.unwrap_or_else(|| PathBuf::from("synaptic-out").join("graph.json"));
    // `mut` so the `report` arm can apply communities before analysis.
    let mut kg = load_scoped_graph(&graph_path, repo.as_deref())?;
    let base = graph_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    // Default output path for a single-file format.
    let file_out = |name: &str| out.clone().unwrap_or_else(|| base.join(name));

    // Guard: `export json --repo X` with no --out would overwrite the source
    // graph.json with a repo-scoped subgraph (data loss). `json` is the only
    // format whose default output name collides with the source.
    if repo.is_some() && out.is_none() && format.eq_ignore_ascii_case("json") {
        anyhow::bail!(
            "`export json --repo …` would overwrite the source graph at {} with a scoped \
             subgraph — pass --out <path> to write the scoped graph elsewhere",
            graph_path.display()
        );
    }

    match format.to_lowercase().as_str() {
        "json" => write_file("graph.json", &file_out("graph.json"), |p| to_json(&kg, p)),
        "html" => write_file("graph.html", &file_out("graph.html"), |p| to_html(&kg, p)),
        "svg" => write_file("graph.svg", &file_out("graph.svg"), |p| to_svg(&kg, p)),
        "graphml" => write_file("graph.graphml", &file_out("graph.graphml"), |p| {
            to_graphml(&kg, p)
        }),
        "cypher" => write_file("graph.cypher", &file_out("graph.cypher"), |p| {
            to_cypher(&kg, p)
        }),
        "dot" => write_file("graph.dot", &file_out("graph.dot"), |p| to_dot(&kg, p)),
        "callflow" | "callflow-html" => {
            write_file("callflow.html", &file_out("callflow.html"), |p| {
                to_mermaid(&kg, p)
            })
        }
        "tree" => write_file("tree.html", &file_out("tree.html"), |p| {
            to_tree_html(&kg, p)
        }),
        "3d" | "force3d" => write_file("graph-3d.html", &file_out("graph-3d.html"), |p| {
            to_force3d(&kg, p)
        }),
        "obsidian" => {
            let dir = out.clone().unwrap_or_else(|| base.join("obsidian"));
            let n = to_obsidian(&kg, &BTreeMap::new(), &dir).context("writing Obsidian vault")?;
            println!("Wrote {} ({n} notes)", dir.display());
            Ok(())
        }
        "wiki" => {
            let dir = out.clone().unwrap_or_else(|| base.join("wiki"));
            let n = to_wiki(&kg, &BTreeMap::new(), &dir).context("writing wiki")?;
            println!("Wrote {} ({n} pages)", dir.display());
            Ok(())
        }
        "report" => {
            // The report needs communities + analysis, which aren't stored in
            // graph.json; recompute them from the loaded graph.
            let communities = cluster(&kg, &ClusterOptions::default());
            apply_communities(&mut kg, &communities);
            let analysis = analyze(&kg, &communities, &BTreeMap::new());
            let p = file_out("GRAPH_REPORT.md");
            write_report(&kg, &analysis, &communities, &BTreeMap::new(), &p)
                .context("writing GRAPH_REPORT.md")?;
            println!("Wrote {}", p.display());
            Ok(())
        }
        "neo4j" => export_neo4j(
            &kg,
            push.as_deref(),
            user,
            password.as_deref(),
            &file_out("graph.cypher"),
        ),
        "falkordb" => export_falkordb(
            &kg,
            push.as_deref(),
            password.as_deref(),
            &file_out("graph.cypher"),
        ),
        other => anyhow::bail!(
            "unknown export format '{other}' (expected one of: json, html, svg, graphml, cypher, \
             dot, callflow, tree, 3d, obsidian, wiki, report, neo4j, falkordb)"
        ),
    }
}

/// Neo4j export: live `--push` (feature `push`) else the cypher script.
pub(crate) fn export_neo4j(
    kg: &KnowledgeGraph,
    push: Option<&str>,
    user: &str,
    password: Option<&str>,
    cypher_path: &Path,
) -> Result<()> {
    let Some(uri) = push else {
        return write_file("graph.cypher", cypher_path, |p| to_cypher(kg, p));
    };
    #[cfg(feature = "push")]
    {
        let pw = password
            .map(str::to_string)
            .or_else(|| std::env::var("NEO4J_PASSWORD").ok())
            .context("--password (or NEO4J_PASSWORD) is required for --push to Neo4j")?;
        let n = synaptic_output::push::push_neo4j(kg, uri, user, &pw)
            .context("pushing to Neo4j via cypher-shell")?;
        println!("Pushed {n} statements to Neo4j at {uri}");
        Ok(())
    }
    #[cfg(not(feature = "push"))]
    {
        let _ = (uri, user, password);
        anyhow::bail!("live --push requires building Synaptic with `--features push`")
    }
}

/// FalkorDB export: live `--push` (feature `push`) else the cypher script.
pub(crate) fn export_falkordb(
    kg: &KnowledgeGraph,
    push: Option<&str>,
    password: Option<&str>,
    cypher_path: &Path,
) -> Result<()> {
    let Some(uri) = push else {
        return write_file("graph.cypher", cypher_path, |p| to_cypher(kg, p));
    };
    #[cfg(feature = "push")]
    {
        let pw = password
            .map(str::to_string)
            .or_else(|| std::env::var("FALKORDB_PASSWORD").ok());
        let n = synaptic_output::push::push_falkordb(kg, uri, "synaptic", pw.as_deref())
            .map_err(|e| anyhow::anyhow!("pushing to FalkorDB: {e}"))?;
        println!("Pushed {n} statements to FalkorDB at {uri}");
        Ok(())
    }
    #[cfg(not(feature = "push"))]
    {
        let _ = (uri, password);
        anyhow::bail!("live --push requires building Synaptic with `--features push`")
    }
}
