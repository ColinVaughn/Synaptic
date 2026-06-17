//! `predict` command: forecast the consequences of a change before applying it.
//! Composes the reverse-impact blast radius (from the graph) with a time-travel
//! graph diff (base vs working tree) into a single forecast.json + forecast.md.
//! CodeGraph never edits source; the forecast is data an AI agent reads first.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use codegraph_graph::KnowledgeGraph;
use codegraph_history::{diff, DiffOptions, DiffReport};
use codegraph_predict::{
    co_change, fold_diff_report, forecast_changes, forecast_edit, refine_risk, refresh_summary,
    render_edit_markdown, render_markdown, ChangeForecast, CoChangeOptions, EditKind,
    ForecastOptions,
};
use codegraph_prs::{detect_default_branch, SystemCommands};

use crate::commands::common::{default_graph_path, load_scoped_graph};

pub(crate) struct PredictArgs {
    pub paths: Vec<PathBuf>,
    pub base: Option<String>,
    pub graph: Option<PathBuf>,
    pub root: PathBuf,
    pub depth: usize,
    pub max_hits: usize,
    pub no_diff: bool,
    pub gate: bool,
    pub edit: Option<String>,
    pub out: Option<PathBuf>,
    pub repo: Option<String>,
    pub json: bool,
}

pub(crate) fn run_predict(a: PredictArgs) -> Result<()> {
    let kg = load_scoped_graph(&default_graph_path(a.graph), a.repo.as_deref())?;

    // Analytic mode: forecast a described edit (delete/signature/visibility) on a
    // symbol, before any code is written. Pure graph -- no base, diff, or git.
    if let Some(spec) = a.edit.as_deref() {
        return run_edit_forecast(&kg, spec, a.depth, a.json, a.out);
    }

    // The base is needed only to derive changed files from git (no explicit
    // paths) or to run the time-travel diff. Resolve it lazily, defaulting to the
    // detected default branch like `working_changes_impact`, so the forecast
    // covers committed + uncommitted branch work (the set a PR would have).
    // The gate needs the time-travel diff (cycles / removed APIs), which needs a base.
    let want_diff = !a.no_diff || a.gate;
    let base: Option<String> = if a.paths.is_empty() || want_diff {
        Some(
            a.base
                .clone()
                .unwrap_or_else(|| detect_default_branch(&SystemCommands, None)),
        )
    } else {
        None
    };

    // The change's files: explicit paths, else `git diff --name-only <base>`.
    let changed_files: Vec<String> = if a.paths.is_empty() {
        let base = base.as_deref().expect("base resolved when no paths given");
        changed_files_from_git(&a.root, base)
            .context("listing changed files (pass explicit paths if not in a git repo)")?
    } else {
        a.paths
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect()
    };

    if changed_files.is_empty() {
        let base = base.as_deref().unwrap_or("the base");
        println!("No changed files vs {base} (nothing to forecast).");
        return Ok(());
    }

    let opts = ForecastOptions {
        depth: a.depth,
        max_hits: a.max_hits,
        ..Default::default()
    };
    let mut forecast = forecast_changes(&kg, &changed_files, &opts);
    // `base` is Some exactly when it was used as the reference point (git file
    // derivation and/or the time-travel diff), so this labels the forecast
    // correctly even under `--no-diff`.
    forecast.base = base.clone();

    // Git-derived enrichment (best effort; skipped outside a git repo): refine
    // the risk score with churn + history, and mine co-change suggestions from
    // recent history. Gated on having resolved a base (i.e. git is in play).
    if let Some(base) = base.as_deref() {
        if let Some((lines, commits)) = git_change_stats(&a.root, base, &changed_files) {
            refine_risk(&mut forecast, lines, commits);
        }
        let transactions = git_transactions(&a.root, 2000);
        if !transactions.is_empty() {
            forecast.co_change_suggestions =
                co_change(&transactions, &changed_files, &CoChangeOptions::default());
            refresh_summary(&mut forecast);
        }
    }

    // Fold in the time-travel diff (base vs working tree) unless skipped. Best
    // effort: a missing git repo or build failure degrades to the pure-graph
    // forecast rather than failing the command -- EXCEPT under `--gate`, where a
    // diff failure must fail the gate (a gate that passes on its own error is
    // worse than no gate).
    let mut diff_ok = false;
    if want_diff {
        let base = base.as_deref().expect("base resolved when diffing");
        match run_history_diff(&a.root, base) {
            Ok(report) => {
                fold_diff_report(&mut forecast, &report);
                diff_ok = true;
            }
            Err(e) => eprintln!(
                "[codegraph] time-travel diff skipped ({e}); reporting pure-graph forecast"
            ),
        }
    }

    if a.json {
        println!("{}", serde_json::to_string_pretty(&forecast)?);
    } else {
        let out_dir = a
            .out
            .unwrap_or_else(|| PathBuf::from("codegraph-out/predict"));
        write_forecast(&forecast, &out_dir)?;
        print_summary(&forecast, &out_dir);
    }

    if a.gate {
        return gate_verdict(&forecast, diff_ok);
    }
    Ok(())
}

/// Analytic edit forecast (`predict --edit "<kind>:<symbol>"`): predict the graph
/// delta of a described edit without touching git or applying anything.
fn run_edit_forecast(
    kg: &KnowledgeGraph,
    spec: &str,
    depth: usize,
    json: bool,
    out: Option<PathBuf>,
) -> Result<()> {
    let (kind, symbol) = parse_edit_spec(spec)?;
    let forecast = forecast_edit(kg, symbol, kind, depth).ok_or_else(|| {
        // Strip any existing `@hint` so the suggested example is well-formed.
        let base = symbol.split('@').next().unwrap_or(symbol).trim();
        anyhow!(
            "no unique node matches '{symbol}'. If the name is shared by several files, qualify it with a (more specific) file: \"<kind>:{base}@<file-substring>\" (e.g. --edit \"{}:{base}@path/to/file\"), or pass an exact node id.",
            kind.as_str()
        )
    })?;

    if json {
        println!("{}", serde_json::to_string_pretty(&forecast)?);
    } else {
        let out_dir = out.unwrap_or_else(|| PathBuf::from("codegraph-out/predict"));
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("creating {}", out_dir.display()))?;
        let json_path = out_dir.join("editforecast.json");
        std::fs::write(&json_path, serde_json::to_string_pretty(&forecast)?)
            .with_context(|| format!("writing {}", json_path.display()))?;
        let md_path = out_dir.join("editforecast.md");
        std::fs::write(&md_path, render_edit_markdown(&forecast))
            .with_context(|| format!("writing {}", md_path.display()))?;
        println!("Edit forecast: {}", forecast.summary);
        println!("  forecast: {}", json_path.display());
        println!("  guide:    {}", md_path.display());
    }
    Ok(())
}

/// Parse the `--edit` spec `"<kind>:<symbol>"`.
fn parse_edit_spec(spec: &str) -> Result<(EditKind, &str)> {
    let (kind_s, symbol) = spec.split_once(':').ok_or_else(|| {
        anyhow!("--edit must be \"<kind>:<symbol>\" (kind = delete|signature|visibility)")
    })?;
    let kind = EditKind::parse(kind_s.trim()).ok_or_else(|| {
        anyhow!(
            "unknown edit kind '{}' (use delete|signature|visibility)",
            kind_s.trim()
        )
    })?;
    let symbol = symbol.trim();
    if symbol.is_empty() {
        bail!("--edit needs a symbol after the ':'");
    }
    Ok((kind, symbol))
}

/// The pre-commit / CI gate: fail when the change introduces a new import cycle
/// or removes a public API. Fails CLOSED -- if the time-travel diff could not run
/// (no base graph, git unavailable, build error) the gate cannot verify the
/// change, so it fails rather than passing blind.
fn gate_verdict(forecast: &ChangeForecast, diff_ok: bool) -> Result<()> {
    if !diff_ok {
        bail!("gate failed: could not run the time-travel diff to verify the change (no base / git unavailable / build error)");
    }
    let cycles = forecast.new_cycles.len();
    let apis = forecast.removed_apis.len();
    if cycles == 0 && apis == 0 {
        println!("Gate passed: no new import cycles or removed public APIs.");
        return Ok(());
    }
    for c in &forecast.new_cycles {
        eprintln!("  new cycle: {}", c.join(" -> "));
    }
    for api in &forecast.removed_apis {
        eprintln!("  removed API: {api}");
    }
    bail!("gate failed: {cycles} new import cycle(s), {apis} removed public API(s)");
}

fn run_history_diff(root: &Path, base: &str) -> Result<DiffReport> {
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;
    diff(&root, base, None, &DiffOptions::default()).map_err(|e| anyhow!("{e}"))
}

/// Best-effort git churn + history for the changed files: (added+removed lines
/// vs `base`, commits in history touching those files). `None` if git is
/// unavailable; either component degrades to 0 on its own failure.
fn git_change_stats(root: &Path, base: &str, files: &[String]) -> Option<(usize, usize)> {
    let run = |args: &[&str]| -> Option<std::process::Output> {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .ok()?;
        out.status.success().then_some(out)
    };
    // Churn from numstat ("added\tremoved\tpath"; binary files show '-').
    let mut diff_args = vec!["diff", "--numstat", base, "--"];
    diff_args.extend(files.iter().map(String::as_str));
    let lines = run(&diff_args).map_or(0, |out| {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| {
                let mut it = l.split('\t');
                let a = it.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                let d = it.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
                a + d
            })
            .sum()
    });
    // History: commits that have touched these files.
    let mut rev_args = vec!["rev-list", "--count", "HEAD", "--"];
    rev_args.extend(files.iter().map(String::as_str));
    let commits = run(&rev_args).map_or(0, |out| {
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<usize>()
            .unwrap_or(0)
    });
    Some((lines, commits))
}

/// Recent commit transactions for co-change mining: each commit's changed-file
/// list, newest first, bounded to `limit` commits. Empty if git is unavailable.
/// Uses a record-separator format so filenames with spaces survive intact.
fn git_transactions(root: &Path, limit: usize) -> Vec<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "log",
            "--name-only",
            "--no-merges",
            "-M",            // detect renames so a moved file is one path, not add+delete
            "--format=%x1e", // record separator before each commit's file list
            &format!("-n{limit}"),
        ])
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .split('\u{1e}')
        .filter_map(|record| {
            let files: Vec<String> = record
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect();
            (!files.is_empty()).then_some(files)
        })
        .collect()
}

fn changed_files_from_git(root: &Path, base: &str) -> Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--name-only", base])
        .output()
        .context("running git diff")?;
    if !out.status.success() {
        bail!(
            "git diff failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

fn write_forecast(forecast: &ChangeForecast, out_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let json_path = out_dir.join("forecast.json");
    std::fs::write(&json_path, serde_json::to_string_pretty(forecast)?)
        .with_context(|| format!("writing {}", json_path.display()))?;
    let md_path = out_dir.join("forecast.md");
    std::fs::write(&md_path, render_markdown(forecast))
        .with_context(|| format!("writing {}", md_path.display()))?;
    Ok(())
}

fn print_summary(forecast: &ChangeForecast, out_dir: &Path) {
    println!("Forecast: {}", forecast.summary);
    if let Some(r) = &forecast.risk {
        println!("  risk: {} ({}/100)", r.level, r.score);
    }
    if !forecast.at_risk_tests.is_empty() {
        println!("  {} test(s) to run", forecast.at_risk_tests.len());
    }
    if !forecast.co_change_suggestions.is_empty() {
        println!(
            "  {} co-change suggestion(s)",
            forecast.co_change_suggestions.len()
        );
    }
    if !forecast.public_api_breaks.is_empty() {
        println!(
            "  WARNING: {} public API(s) at risk",
            forecast.public_api_breaks.len()
        );
    }
    if !forecast.new_cycles.is_empty() {
        println!(
            "  WARNING: {} new import cycle(s)",
            forecast.new_cycles.len()
        );
    }
    println!("  forecast: {}", out_dir.join("forecast.json").display());
    println!("  guide: {}", out_dir.join("forecast.md").display());
}
