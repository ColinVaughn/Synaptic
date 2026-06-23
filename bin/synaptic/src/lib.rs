//! Synaptic CLI: build a code knowledge graph and query it. Offline, no API key.
//!
//! This is the CLI library crate. The `synaptic` and `syn` binaries (in
//! `src/bin/`) are thin wrappers that call [`run_cli`].

mod cli;
mod commands;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Cmd};
use commands::cache::run_cache;
use commands::diff::run_diff;
use commands::eval::run_eval;
use commands::export::run_export;
use commands::extract::run_extract;
use commands::global::run_global;
use commands::hook::run_hook;
use commands::ingest::run_ingest;
use commands::install::{run_install, run_uninstall};
use commands::merge::run_merge_graphs;
use commands::predict::run_predict;
use commands::prs::run_prs;
use commands::query::{run_affected, run_explain, run_hazards, run_path, run_query};
use commands::refactor::run_refactor;
use commands::search::run_search;
use commands::self_update::run_self_update;
use commands::serve::run_serve;
use commands::skill::run_skill;
use commands::speculate::run_speculate;
use commands::update::run_update;
use commands::watch::run_watch;
use commands::workspace::run_workspace;
use synaptic_incremental::run_merge_driver;

/// Entry point shared by the `synaptic` and `syn` binaries.
pub fn run_cli() -> Result<()> {
    // Windows defaults the main thread to a 1 MB stack; building or loading the
    // graph for a large repo recurses deeper than that (worse in debug, where
    // frames are larger), so the CLI would overflow on Windows while running
    // fine on the 8 MB Linux/macOS default. Run the work on a worker thread with
    // a generous stack so behavior is identical across platforms.
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(run)
        .expect("spawn cli worker thread")
        .join()
        .expect("cli worker thread panicked")
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    // Opt-in background update notice (off by default; throttled to once a day;
    // swallows all errors). Skipped for `self-update` itself so the check can't
    // nag mid-update.
    if !matches!(cli.cmd, Cmd::SelfUpdate { .. }) {
        if let Some(note) = synaptic_upgrade::check::maybe_notify(env!("CARGO_PKG_VERSION")) {
            eprintln!("{note}");
        }
    }
    match cli.cmd {
        Cmd::Extract {
            path,
            directed,
            obsidian,
            wiki,
            semantic,
            no_columns,
        } => run_extract(&path, directed, obsidian, wiki, semantic, no_columns),
        Cmd::Query {
            text,
            graph,
            max_nodes,
            repo,
            dfs,
            since,
            seed_changed,
        } => run_query(
            &text,
            graph,
            max_nodes,
            repo.as_deref(),
            dfs,
            since.as_deref(),
            seed_changed,
        ),
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
            limit,
            verbose,
        } => run_affected(&node, graph, depth, relations, limit, verbose),
        Cmd::Hazards {
            graph,
            repo,
            kind,
            limit,
        } => run_hazards(graph, repo, kind, limit),
        Cmd::MergeDriver {
            base: _,
            current,
            other,
        } => {
            let n = run_merge_driver(&current, &other)
                .map_err(|e| anyhow::anyhow!("merge-driver: {e}"))?;
            eprintln!("[synaptic] merge-driver: unioned graph.json → {n} nodes");
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
        Cmd::Install {
            platform,
            global,
            refresh,
        } => run_install(&platform, global, refresh),
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
            allow_exec,
        } => run_serve(graph, http, api_key, source_root, allow_exec),
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
        Cmd::Diff {
            rev1,
            rev2,
            since,
            root,
            directed,
            scope,
            top,
            module_depth,
            json,
            report,
            html,
            no_cache,
        } => run_diff(commands::diff::DiffArgs {
            rev1,
            rev2,
            since,
            root,
            directed,
            scope,
            top,
            module_depth,
            json,
            report_path: report,
            html_path: html,
            no_cache,
        }),
        Cmd::Search {
            query,
            pattern,
            list_patterns,
            explain,
            save,
            saved,
            list_saved,
            graph,
            repo,
            json,
            limit,
        } => run_search(commands::search::SearchArgs {
            query,
            pattern,
            list_patterns,
            explain,
            save,
            saved,
            list_saved,
            graph,
            repo,
            json,
            limit,
        }),
        Cmd::Refactor { action } => run_refactor(action),
        Cmd::Predict {
            paths,
            base,
            graph,
            root,
            depth,
            max_hits,
            no_diff,
            gate,
            edit,
            out,
            repo,
            json,
        } => run_predict(commands::predict::PredictArgs {
            paths,
            base,
            graph,
            root,
            depth,
            max_hits,
            no_diff,
            gate,
            edit,
            out,
            repo,
            json,
        }),
        Cmd::Speculate {
            paths,
            base,
            graph,
            root,
            patch,
            test_cmd,
            check_cmd,
            no_detect,
            depth,
            timeout,
            max_tests,
            fail_fast,
            out,
            repo,
            json,
        } => run_speculate(commands::speculate::SpeculateArgs {
            paths,
            base,
            graph,
            root,
            patch,
            test_cmd,
            check_cmd,
            no_detect,
            depth,
            timeout,
            max_tests,
            fail_fast,
            out,
            repo,
            json,
        }),
        Cmd::Eval { action } => run_eval(action),
        Cmd::Sql { action } => commands::sql::run_sql(action),
        Cmd::SelfUpdate {
            enable,
            disable,
            check,
            yes,
        } => run_self_update(enable, disable, check, yes),
    }
}
