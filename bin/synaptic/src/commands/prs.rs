//! `prs` command(s) split from main.rs.

use crate::commands::common::{default_graph_path, load_graph};
use anyhow::Result;
use std::path::PathBuf;
use synaptic_prs::{
    compute_pr_impact, detect_default_branch, fetch_pr, fetch_pr_files, fetch_prs, fetch_worktrees,
    format_conflicts, format_pr_detail, format_prs_text, format_triage, select_actionable,
    today_epoch_days, ImpactIndex, SystemCommands,
};

pub(crate) fn run_prs(
    number: Option<u64>,
    repo: Option<String>,
    base: Option<String>,
    graph: Option<PathBuf>,
    triage: bool,
    conflicts: bool,
) -> Result<()> {
    let runner = SystemCommands;
    let now = today_epoch_days();
    let repo_ref = repo.as_deref();
    // Resolve the base once (avoids a second gh round-trip).
    let base_resolved = match base {
        Some(b) => b,
        None => detect_default_branch(&runner, repo_ref),
    };
    let graph_path = default_graph_path(graph);

    // Populate graph blast radius (communities + node count) on each PR from its
    // changed files; no-op when there's no graph.json. One gh round-trip per PR.
    let attach_impact = |prs: &mut [synaptic_prs::PrInfo]| {
        if let Ok(kg) = load_graph(&graph_path) {
            // Build the source_file -> impact index once, then reuse it for every
            // PR instead of rebuilding it per PR (H5).
            let impact =
                ImpactIndex::build(kg.nodes().map(|nd| (nd.source_file.as_str(), nd.community)));
            for p in prs.iter_mut() {
                p.files_changed = fetch_pr_files(&runner, p.number, repo_ref);
                let (comms, nodes) = impact.impact_for_files(&p.files_changed);
                p.communities_touched = comms;
                p.nodes_affected = nodes;
            }
        }
    };

    // `--conflicts` and `--triage` are dashboard-level views (take precedence over
    // a PR number, which selects the single-PR detail view).
    if conflicts {
        let mut prs = fetch_prs(&runner, repo_ref, Some(&base_resolved), 50)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        attach_impact(&mut prs);
        println!("{}", format_conflicts(&prs, &base_resolved, now));
        return Ok(());
    }
    if triage {
        let prs = fetch_prs(&runner, repo_ref, Some(&base_resolved), 50)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut actionable = select_actionable(prs, &base_resolved, now);
        attach_impact(&mut actionable);
        println!("{}", format_triage(&actionable, &base_resolved, now));
        return Ok(());
    }

    match number {
        None => {
            // Dashboard of open PRs (no per-PR diff fetch; that's the detail view).
            let prs = fetch_prs(&runner, repo_ref, Some(&base_resolved), 50)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("{}", format_prs_text(&prs, &base_resolved, now));
        }
        Some(n) => {
            // Detail: fetch the PR, attach its worktree, changed files, and graph
            // blast radius (when a graph.json is present).
            let Some(mut pr) = fetch_pr(&runner, n, repo_ref, &base_resolved) else {
                println!("PR #{n} not found (gh unavailable, unauthenticated, or no such PR).");
                return Ok(());
            };
            pr.worktree_path = fetch_worktrees(&runner).get(&pr.branch).cloned();
            pr.files_changed = fetch_pr_files(&runner, n, repo_ref);
            if let Ok(kg) = load_graph(&graph_path) {
                let (comms, nodes) = compute_pr_impact(
                    kg.nodes().map(|nd| (nd.source_file.as_str(), nd.community)),
                    &pr.files_changed,
                );
                pr.communities_touched = comms;
                pr.nodes_affected = nodes;
            }
            println!("{}", format_pr_detail(&pr, now, 20));
        }
    }
    Ok(())
}
