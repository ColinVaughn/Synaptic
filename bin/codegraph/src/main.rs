//! CodeGraph CLI: build a code knowledge graph and query it. Offline, no API key.

mod cli;
mod commands;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Cmd};
use codegraph_incremental::run_merge_driver;
use commands::cache::run_cache;
use commands::export::run_export;
use commands::extract::run_extract;
use commands::global::run_global;
use commands::hook::run_hook;
use commands::ingest::run_ingest;
use commands::install::{run_install, run_uninstall};
use commands::merge::run_merge_graphs;
use commands::prs::run_prs;
use commands::query::{run_affected, run_explain, run_path, run_query};
use commands::serve::run_serve;
use commands::skill::run_skill;
use commands::update::run_update;
use commands::watch::run_watch;
use commands::workspace::run_workspace;

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Extract {
            path,
            directed,
            obsidian,
            wiki,
            semantic,
        } => run_extract(&path, directed, obsidian, wiki, semantic),
        Cmd::Query {
            text,
            graph,
            max_nodes,
            repo,
            dfs,
        } => run_query(&text, graph, max_nodes, repo.as_deref(), dfs),
        Cmd::Update {
            paths,
            full,
            directed,
            force,
        } => run_update(paths, full, directed, force),
        Cmd::Watch { directed, force } => run_watch(directed, force),
        Cmd::Path {
            from,
            to,
            graph,
            repo,
        } => run_path(&from, &to, graph, repo.as_deref()),
        Cmd::Explain { node, graph, repo } => run_explain(&node, graph, repo.as_deref()),
        Cmd::Affected {
            node,
            graph,
            depth,
            relations,
        } => run_affected(&node, graph, depth, relations),
        Cmd::MergeDriver {
            base: _,
            current,
            other,
        } => {
            let n = run_merge_driver(&current, &other)
                .map_err(|e| anyhow::anyhow!("merge-driver: {e}"))?;
            eprintln!("[codegraph] merge-driver: unioned graph.json → {n} nodes");
            Ok(())
        }
        Cmd::Hook { action } => run_hook(action),
        Cmd::Export {
            format,
            graph,
            out,
            repo,
            push,
            user,
            password,
        } => run_export(&format, graph, out, push, &user, password, repo),
        Cmd::Ingest { source } => run_ingest(source),
        Cmd::Install { platform, global } => run_install(&platform, global),
        Cmd::Uninstall {
            platform,
            all,
            global,
        } => run_uninstall(&platform, all, global),
        Cmd::Serve {
            graph,
            http,
            api_key,
            source_root,
        } => run_serve(graph, http, api_key, source_root),
        Cmd::Prs {
            number,
            repo,
            base,
            graph,
            triage,
            conflicts,
        } => run_prs(number, repo, base, graph, triage, conflicts),
        Cmd::Skill { action } => run_skill(action),
        Cmd::Workspace { action } => run_workspace(action),
        Cmd::Global { action } => run_global(action),
        Cmd::MergeGraphs { graphs, out } => run_merge_graphs(graphs, out),
        Cmd::Cache { action } => run_cache(action),
    }
}
