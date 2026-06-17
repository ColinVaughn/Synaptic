//! `eval` command: measure forecast quality by replaying history. Re-predicts
//! each commit from its parent-state graph and scores the prediction against git
//! ground truth, so prediction quality can be tracked and gated like any other
//! metric.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use codegraph_eval::{calibrate_cross_language, replay, ReplayOptions, ReplayReport};

use crate::cli::EvalAction;

pub(crate) fn run_eval(action: EvalAction) -> Result<()> {
    match action {
        EvalAction::Replay {
            from,
            root,
            depth,
            max_commits,
            directed,
            min_test_recall,
            out,
            json,
        } => run_replay(ReplayArgs {
            from,
            root,
            depth,
            max_commits,
            directed,
            min_test_recall,
            out,
            json,
        }),
        EvalAction::CrossLanguage { graph, json } => run_cross_language(graph, json),
    }
}

/// Calibrate the cross-language edge layer over a built graph.json.
fn run_cross_language(graph_path: PathBuf, json: bool) -> Result<()> {
    let bytes =
        std::fs::read(&graph_path).with_context(|| format!("reading {}", graph_path.display()))?;
    let graph: codegraph_core::GraphData = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", graph_path.display()))?;
    let report = calibrate_cross_language(&graph);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Cross-language calibration: {}", report.summary());
        for (rel, n) in &report.relation_counts {
            println!("  {rel}: {n}");
        }
    }
    Ok(())
}

struct ReplayArgs {
    from: String,
    root: PathBuf,
    depth: usize,
    max_commits: usize,
    directed: bool,
    min_test_recall: Option<u8>,
    out: Option<PathBuf>,
    json: bool,
}

fn run_replay(a: ReplayArgs) -> Result<()> {
    let opts = ReplayOptions {
        directed: a.directed,
        depth: a.depth,
        max_commits: a.max_commits,
    };
    let report =
        replay(&a.root, &a.from, &opts).map_err(|e| anyhow!("replaying {}..HEAD: {e}", a.from))?;

    if a.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let out_dir = a.out.unwrap_or_else(|| PathBuf::from("codegraph-out/eval"));
        write_report(&report, &out_dir)?;
        println!("Eval: {}", report.summary);
        println!("  report: {}", out_dir.join("report.json").display());
        println!("  guide:  {}", out_dir.join("report.md").display());
    }

    // The CI eval gate.
    if let Some(min) = a.min_test_recall {
        if report.test.relevant == 0 {
            println!(
                "Eval gate: no tests were edited in {}..HEAD; nothing to gate.",
                a.from
            );
        } else if report.meets_test_recall(min) {
            println!(
                "Eval gate passed: test-selection recall {}% >= {min}%.",
                report.test.recall_pct()
            );
        } else {
            bail!(
                "eval gate failed: test-selection recall {}% < {min}% (over {} relevant test(s))",
                report.test.recall_pct(),
                report.test.relevant
            );
        }
    }
    Ok(())
}

fn write_report(report: &ReplayReport, out_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let json_path = out_dir.join("report.json");
    std::fs::write(&json_path, serde_json::to_string_pretty(report)?)
        .with_context(|| format!("writing {}", json_path.display()))?;
    let md_path = out_dir.join("report.md");
    std::fs::write(&md_path, render_markdown(report))
        .with_context(|| format!("writing {}", md_path.display()))?;
    Ok(())
}

fn render_markdown(r: &ReplayReport) -> String {
    let mut s = String::new();
    s.push_str("# Forecast evaluation (replay)\n\n");
    s.push_str(&r.summary);
    s.push_str("\n\n## Pooled scores\n\n");
    s.push_str(&format!(
        "- co-edited test selection: recall {}% / precision {}% (over {} co-edited, pre-existing test(s))\n",
        r.test.recall_pct(),
        r.test.precision_pct(),
        r.test.relevant
    ));
    s.push_str(&format!(
        "- removed-API detection (lower bound; visibility-annotated languages only): recall {}% / precision {}% (over {} removed API(s))\n",
        r.api.recall_pct(),
        r.api.precision_pct(),
        r.api.relevant
    ));
    s.push_str(&format!(
        "- blast-radius selectivity: {}% of the graph flagged (pooled)\n",
        r.selectivity_pct
    ));
    if !r.commits.is_empty() {
        s.push_str("\n## Per commit\n\n");
        s.push_str("| commit | changed | tests hit/edited | blast/nodes |\n");
        s.push_str("| --- | --- | --- | --- |\n");
        for c in &r.commits {
            s.push_str(&format!(
                "| `{}` | {} | {}/{} | {}/{} |\n",
                short(&c.commit),
                c.changed_files.len(),
                c.test.hits,
                c.test.relevant,
                c.blast_total,
                c.graph_nodes
            ));
        }
    }
    s
}

fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}
