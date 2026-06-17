//! `speculate` command: evaluate a proposed change for real. It applies the
//! change in a throwaway git worktree, runs the forecast's at-risk tests plus a
//! build/type-check, and reports the actual pass/fail outcome -- the ground-truth
//! half of the prediction system. This is an opt-in CLI, never an MCP tool: it
//! runs commands, which would break the server's read-only invariant.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use codegraph_predict::{forecast_changes, ForecastOptions};
use codegraph_prs::{detect_default_branch, SystemCommands};
use codegraph_sandbox::{render_markdown, speculate, Change, Outcome, SpeculateOptions};

use crate::commands::common::{default_graph_path, load_scoped_graph};

pub(crate) struct SpeculateArgs {
    pub paths: Vec<PathBuf>,
    pub base: Option<String>,
    pub graph: Option<PathBuf>,
    pub root: PathBuf,
    pub patch: Option<PathBuf>,
    pub test_cmd: Option<String>,
    pub check_cmd: Option<String>,
    pub no_detect: bool,
    pub depth: usize,
    pub timeout: u64,
    pub max_tests: usize,
    pub fail_fast: bool,
    pub out: Option<PathBuf>,
    pub repo: Option<String>,
    pub json: bool,
}

pub(crate) fn run_speculate(a: SpeculateArgs) -> Result<()> {
    let kg = load_scoped_graph(&default_graph_path(a.graph), a.repo.as_deref())?;

    // A patch is applied onto the current commit by default; working-tree changes
    // are measured against the branch base (the set a PR would carry), matching
    // `predict`. The base also checks out the worktree the change is applied to.
    let base = a.base.clone().unwrap_or_else(|| {
        if a.patch.is_some() {
            "HEAD".to_string()
        } else {
            detect_default_branch(&SystemCommands, None)
        }
    });

    // The change's files: explicit paths win; otherwise derive them from the
    // patch (its `+++ b/` headers) or from `git diff --name-only <base>`.
    let patch_text = match &a.patch {
        Some(p) => {
            Some(std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?)
        }
        None => None,
    };
    // Explicit paths scope both the at-risk test selection and (in working-tree
    // mode) the applied diff, so the two halves agree on the file set.
    let explicit: Vec<String> = a
        .paths
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    let changed_files: Vec<String> = if !explicit.is_empty() {
        explicit.clone()
    } else if let Some(text) = &patch_text {
        files_from_patch(text)
    } else {
        changed_files_from_git(&a.root, &base)
            .context("listing changed files (pass explicit paths if not in a git repo)")?
    };

    if changed_files.is_empty() {
        println!("No changed files vs {base} (nothing to speculate).");
        return Ok(());
    }

    // The forecast tells us which tests are at risk; the sandbox runs exactly
    // those, highest-impact first.
    let opts = ForecastOptions {
        depth: a.depth,
        ..Default::default()
    };
    let forecast = forecast_changes(&kg, &changed_files, &opts);
    let test_files = unique_test_files(&forecast.at_risk_tests);

    let change = match patch_text {
        Some(diff) => Change::Patch { base, diff },
        None => {
            warn_on_untracked(&a.root);
            Change::WorkingTree {
                base,
                paths: explicit,
            }
        }
    };
    let sopts = SpeculateOptions {
        test_cmd: a.test_cmd,
        check_cmd: a.check_cmd,
        test_files,
        auto_detect: !a.no_detect,
        timeout: Duration::from_secs(a.timeout),
        max_tests: a.max_tests,
        fail_fast: a.fail_fast,
        ..Default::default()
    };

    let report = speculate(&a.root, &change, &sopts)?;

    if a.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let out_dir = a
            .out
            .unwrap_or_else(|| PathBuf::from("codegraph-out/speculate"));
        write_report(&report, &out_dir)?;
        println!("Speculate: {}", report.summary);
        let note = test_files_note(&report);
        if !note.is_empty() {
            println!("{note}");
        }
        println!("  report: {}", out_dir.join("report.json").display());
        println!("  guide:  {}", out_dir.join("report.md").display());
    }

    // A real failure exits non-zero so the command can gate CI / an agent loop.
    if report.outcome == Outcome::Failed {
        bail!("speculation failed: the change broke the build or an at-risk test");
    }
    Ok(())
}

fn unique_test_files(hits: &[codegraph_predict::ImpactHit]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for h in hits {
        if seen.insert(h.file.clone()) {
            files.push(h.file.clone());
        }
    }
    files
}

fn test_files_note(report: &codegraph_sandbox::SpeculateReport) -> String {
    if report.tests_total_at_risk == 0 {
        return String::new();
    }
    let ran = report
        .tests
        .iter()
        .filter(|t| t.status != codegraph_sandbox::CommandStatus::Skipped)
        .count();
    if ran == 0 {
        // No test ran (no test command available, or a failed check skipped them).
        format!(
            "  {} at-risk test(s) identified; none run (pass --test-cmd, or drop --no-detect)",
            report.tests_total_at_risk
        )
    } else if report.tests_scoped {
        format!(
            "  ran {ran} of {} at-risk test(s)",
            report.tests_total_at_risk
        )
    } else {
        format!(
            "  ran the whole test suite ({} at-risk test(s) identified)",
            report.tests_total_at_risk
        )
    }
}

/// Best-effort warning: working-tree mode captures the diff with `git diff`,
/// which omits untracked files, so a change that adds a new file would not be
/// speculated. Tell the user; suggest --patch (which can include new files).
fn warn_on_untracked(root: &Path) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            let n = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| l.starts_with("??"))
                .count();
            if n > 0 {
                eprintln!(
                    "[codegraph] note: {n} untracked file(s) are not included in working-tree speculation; use --patch to include new files"
                );
            }
        }
    }
}

/// Repo-relative paths a unified diff touches, from its `+++ b/<path>` headers.
fn files_from_patch(text: &str) -> Vec<String> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let path = rest.trim().trim_start_matches("b/");
            if path == "/dev/null" || path.is_empty() {
                continue;
            }
            if seen.insert(path.to_string()) {
                files.push(path.to_string());
            }
        }
    }
    files
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

fn write_report(report: &codegraph_sandbox::SpeculateReport, out_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let json_path = out_dir.join("report.json");
    std::fs::write(&json_path, serde_json::to_string_pretty(report)?)
        .with_context(|| format!("writing {}", json_path.display()))?;
    let md_path = out_dir.join("report.md");
    std::fs::write(&md_path, render_markdown(report))
        .with_context(|| format!("writing {}", md_path.display()))?;
    Ok(())
}
