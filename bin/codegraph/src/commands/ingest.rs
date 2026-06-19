//! `ingest` command(s) split from main.rs.

use crate::cli::IngestSource;
use crate::commands::extract::write_outputs;
use anyhow::{Context, Result};
use codegraph_core::GraphData;
use codegraph_graph::{
    analyze, apply_communities, build_from_parts, cluster, BuildOptions, ClusterOptions,
};
use codegraph_ingest::{
    ingest_mcp_config, ingest_scip_json, ingest_url, introspect_cargo, Ingested,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
// `PathBuf` is used only by `ingested_dir`, which is gated on these features.
#[cfg(any(feature = "office", feature = "gws", feature = "media"))]
use std::path::PathBuf;

pub(crate) fn run_ingest(source: IngestSource) -> Result<()> {
    let root = std::env::current_dir().context("resolving current directory")?;
    let out_dir = root.join("codegraph-out");
    match source {
        IngestSource::Cargo { path } => {
            merge_into_graph(&out_dir, introspect_cargo(&path), "cargo")
        }
        IngestSource::Mcp { file } => merge_into_graph(&out_dir, ingest_mcp_config(&file), "mcp"),
        IngestSource::Scip { file } => {
            let text = fs::read_to_string(&file)
                .with_context(|| format!("reading SCIP index {}", file.display()))?;
            let doc: serde_json::Value = serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", file.display()))?;
            merge_into_graph(&out_dir, ingest_scip_json(&doc, "", "python"), "scip")
        }
        IngestSource::Pg { dsn } => run_ingest_pg(&out_dir, dsn),
        IngestSource::Url { url } => {
            let dir = out_dir.join("ingested");
            let p = ingest_url(&url, &dir).map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "Fetched into {}. Run `codegraph extract` (or `update`) to index it.",
                p.display()
            );
            Ok(())
        }
        IngestSource::Office { file } => run_ingest_office(&out_dir, &file),
        IngestSource::Gws { file } => run_ingest_gws(&out_dir, &file),
        IngestSource::Media { file } => run_ingest_media(&out_dir, &file),
    }
}

/// Converted-file ingest sources write a markdown document into
/// `codegraph-out/ingested/` for the next `extract`/`update` to index.
#[cfg(any(feature = "office", feature = "gws", feature = "media"))]
pub(crate) fn ingested_dir(out_dir: &Path) -> PathBuf {
    out_dir.join("ingested")
}

#[cfg(any(feature = "office", feature = "gws", feature = "media"))]
pub(crate) fn print_ingested(p: &Path) {
    println!(
        "Wrote {}. Run `codegraph extract` (or `update`) to index it.",
        p.display()
    );
}

#[cfg(feature = "office")]
pub(crate) fn run_ingest_office(out_dir: &Path, file: &Path) -> Result<()> {
    let md = codegraph_ingest::xlsx_to_markdown(file).map_err(|e| anyhow::anyhow!("{e}"))?;
    let dir = ingested_dir(out_dir);
    fs::create_dir_all(&dir)?;
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sheet".into());
    let body = format!(
        "---\ntitle: {}\ntype: spreadsheet\n---\n\n{md}\n",
        codegraph_ingest::yaml_str(&stem)
    );
    let p = dir.join(format!("{stem}.md"));
    fs::write(&p, body)?;
    print_ingested(&p);
    Ok(())
}

#[cfg(not(feature = "office"))]
pub(crate) fn run_ingest_office(_out_dir: &Path, _file: &Path) -> Result<()> {
    anyhow::bail!("office support is not built in; rebuild with `--features office`")
}

#[cfg(feature = "gws")]
pub(crate) fn run_ingest_gws(out_dir: &Path, file: &Path) -> Result<()> {
    let p = codegraph_ingest::ingest_gdoc(file, &ingested_dir(out_dir))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    print_ingested(&p);
    Ok(())
}

#[cfg(not(feature = "gws"))]
pub(crate) fn run_ingest_gws(_out_dir: &Path, _file: &Path) -> Result<()> {
    anyhow::bail!("Google-Workspace support is not built in; rebuild with `--features gws`")
}

#[cfg(feature = "media")]
pub(crate) fn run_ingest_media(out_dir: &Path, file: &Path) -> Result<()> {
    let p = codegraph_ingest::transcribe_media(file, &ingested_dir(out_dir))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    print_ingested(&p);
    Ok(())
}

#[cfg(not(feature = "media"))]
pub(crate) fn run_ingest_media(_out_dir: &Path, _file: &Path) -> Result<()> {
    anyhow::bail!("media transcription is not built in; rebuild with `--features media`")
}

/// Introspect a live Postgres database and merge its schema into the graph.
/// Only compiled with real behavior under `--features pg`; otherwise it errors
/// with a clear rebuild hint (the subcommand stays visible in `--help`).
#[cfg(feature = "pg")]
pub(crate) fn run_ingest_pg(out_dir: &Path, dsn: String) -> Result<()> {
    let ing = codegraph_ingest::introspect_postgres(&codegraph_ingest::SystemPostgres::new(dsn))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    merge_into_graph(out_dir, ing, "pg")
}

#[cfg(not(feature = "pg"))]
pub(crate) fn run_ingest_pg(_out_dir: &Path, _dsn: String) -> Result<()> {
    anyhow::bail!("postgres support is not built in; rebuild with `cargo install --features pg` (or `cargo build --features pg`)")
}

/// Merge ingested nodes/edges into the existing graph.json and rebuild outputs.
pub(crate) fn merge_into_graph(out_dir: &Path, ing: Ingested, label: &str) -> Result<()> {
    if ing.nodes.is_empty() {
        println!("{label}: no nodes produced (nothing to merge).");
        return Ok(());
    }
    let added_n = ing.nodes.len();
    let added_e = ing.edges.len();
    let graph_path = out_dir.join("graph.json");
    let existing: Option<GraphData> = fs::read_to_string(&graph_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    let (mut nodes, mut edges, hyper, directed) = match existing {
        Some(g) => (g.nodes, g.links, g.hyperedges, g.directed),
        None => (vec![], vec![], vec![], false),
    };
    nodes.extend(ing.nodes);
    edges.extend(ing.edges);

    let opts = BuildOptions {
        directed,
        root: Some(
            out_dir
                .parent()
                .unwrap_or(out_dir)
                .to_string_lossy()
                .into_owned(),
        ),
    };
    let mut kg = build_from_parts(nodes, edges, hyper, &opts);
    let communities = cluster(&kg, &ClusterOptions::default());
    apply_communities(&mut kg, &communities);
    let analysis = analyze(&kg, &communities, &BTreeMap::new());
    write_outputs(
        &kg,
        &analysis,
        &communities,
        &BTreeMap::new(),
        out_dir,
        false,
        false,
    )?;
    println!(
        "{label}: merged +{added_n} node(s), +{added_e} edge(s) → {} nodes, {} edges total",
        kg.node_count(),
        kg.edge_count()
    );
    Ok(())
}
