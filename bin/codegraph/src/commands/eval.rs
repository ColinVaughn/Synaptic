//! `eval` command: measure forecast quality by replaying history. Re-predicts
//! each commit from its parent-state graph and scores the prediction against git
//! ground truth, so prediction quality can be tracked and gated like any other
//! metric.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use codegraph_eval::{
    calibrate_cross_language, calibrate_history, replay, run_corpus, CalibrationReport,
    CorpusReport, ReplayOptions, ReplayReport,
};

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
        EvalAction::Corpus { root, out, json } => run_corpus_cmd(root, out, json),
        EvalAction::Calibrate {
            root,
            max_commits,
            bins,
            out,
            json,
        } => run_calibrate_cmd(root, max_commits, bins, out, json),
    }
}

fn run_calibrate_cmd(
    root: PathBuf,
    max_commits: usize,
    bins: usize,
    out: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    let report =
        calibrate_history(&root, max_commits, bins).map_err(|e| anyhow!("calibrating: {e}"))?;
    let md = calibrate_markdown(&report);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let out_dir = out.unwrap_or_else(|| PathBuf::from("codegraph-out/eval/calibrate"));
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("creating {}", out_dir.display()))?;
        std::fs::write(out_dir.join("report.json"), serde_json::to_string_pretty(&report)?)?;
        std::fs::write(out_dir.join("report.md"), &md)?;
        print!("{md}");
        println!("  report: {}", out_dir.join("report.json").display());
    }
    Ok(())
}

fn calibrate_markdown(r: &CalibrationReport) -> String {
    let mut s = String::from("# Prediction calibration (co-change)\n\n");
    s.push_str(&format!(
        "Brier score: **{:.3}** over {} prediction(s). 0 is perfect; lower is better.\n\n",
        r.brier, r.n
    ));
    if r.n == 0 {
        s.push_str("No multi-file commits in range, so there is nothing to calibrate.\n");
        return s;
    }
    s.push_str("| Confidence bin | Predicted (mean) | Observed hit rate | Count |\n");
    s.push_str("|---|--:|--:|--:|\n");
    for b in &r.bins {
        if b.count == 0 {
            continue;
        }
        s.push_str(&format!(
            "| {:.0}-{:.0}% | {:.0}% | {:.0}% | {} |\n",
            b.lo * 100.0,
            b.hi * 100.0,
            b.mean_confidence * 100.0,
            b.observed_hit_rate * 100.0,
            b.count
        ));
    }
    s
}

/// Default corpus root: the in-tree corpus, located relative to this crate at
/// compile time. An installed binary run outside the repo must pass `--root`.
fn default_corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../crates/codegraph-eval/corpus")
}

fn run_corpus_cmd(root: Option<PathBuf>, out: Option<PathBuf>, json: bool) -> Result<()> {
    let root = root.unwrap_or_else(default_corpus_root);
    if !root.join("manifest.toml").exists() {
        bail!(
            "no manifest.toml under {} (pass --root to point at the corpus)",
            root.display()
        );
    }
    let report = run_corpus(&root).map_err(|e| anyhow!("scoring corpus: {e}"))?;
    let md = corpus_markdown(&report);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let out_dir = out.unwrap_or_else(|| PathBuf::from("codegraph-out/eval/corpus"));
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("creating {}", out_dir.display()))?;
        std::fs::write(out_dir.join("report.json"), serde_json::to_string_pretty(&report)?)?;
        std::fs::write(out_dir.join("report.md"), &md)?;
        print!("{md}");
        println!("  report: {}", out_dir.join("report.json").display());
    }
    Ok(())
}

fn corpus_markdown(report: &CorpusReport) -> String {
    let mut s = String::from("# Accuracy corpus\n\n");
    s.push_str("Exact set-comparison against hand-labeled ground truth. Call/cross P/R/F1 are percentages; blast FN is the percent of truly-affected nodes missed (lower is better).\n\n");
    s.push_str("| Fixture | Family | Call P/R/F1 | Aff-test recall | Blast FN | Cross P/R/F1 |\n");
    s.push_str("|---|---|---|---|---|---|\n");
    // A metric with no labels in a fixture renders "n/a" rather than a vacuous
    // 100%, so an empty set is never mistaken for a perfect score.
    let prf1 = |p: &codegraph_eval::PrF1| {
        if p.true_positive + p.false_positive + p.false_negative == 0 {
            "n/a".to_string()
        } else {
            format!("{}/{}/{}", p.precision_pct(), p.recall_pct(), p.f1_pct())
        }
    };
    let recall = |p: &codegraph_eval::PrF1| {
        if p.true_positive + p.false_negative == 0 {
            "n/a".to_string()
        } else {
            format!("{}%", p.recall_pct())
        }
    };
    for f in &report.fixtures {
        let blast = if f.blast.expected == 0 {
            "n/a".to_string()
        } else {
            format!("{}%", f.blast.false_negative_pct())
        };
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            f.dir,
            f.family,
            prf1(&f.call_edges),
            recall(&f.affected_tests),
            blast,
            prf1(&f.cross_edges),
        ));
    }
    let pooled = report.pooled_call_edges();
    s.push_str(&format!(
        "\nPooled call-edge: precision {}% / recall {}% / F1 {}% over {} labeled call edge(s).\n",
        pooled.precision_pct(),
        pooled.recall_pct(),
        pooled.f1_pct(),
        pooled.true_positive + pooled.false_negative,
    ));
    s
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
