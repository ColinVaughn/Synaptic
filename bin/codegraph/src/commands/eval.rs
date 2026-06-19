//! `eval` command: measure forecast quality by replaying history. Re-predicts
//! each commit from its parent-state graph and scores the prediction against git
//! ground truth, so prediction quality can be tracked and gated like any other
//! metric.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use codegraph_eval::{
    calibrate_cross_language, calibrate_history, replay, run_corpus, run_scale, CalibrationReport,
    CorpusReport, ReplayOptions, ReplayReport, ScaleReport,
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
        EvalAction::Scale {
            manifest,
            tier,
            reps,
            cache,
            out,
            json,
        } => run_scale_cmd(manifest, tier, reps, cache, out, json),
    }
}

fn default_scale_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../crates/codegraph-eval/scale-corpus.toml")
}

fn run_scale_cmd(
    manifest: Option<PathBuf>,
    tier: Option<String>,
    reps: usize,
    cache: Option<PathBuf>,
    out: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    let manifest = manifest.unwrap_or_else(default_scale_manifest);
    if !manifest.exists() {
        bail!(
            "no scale manifest at {} (pass --manifest)",
            manifest.display()
        );
    }
    let cache = cache.unwrap_or_else(|| PathBuf::from("codegraph-out/bench"));
    let report = run_scale(&manifest, &cache, tier.as_deref(), reps)
        .map_err(|e| anyhow!("scale run: {e}"))?;
    let md = scale_markdown(&report);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let out_dir = out.unwrap_or_else(|| PathBuf::from("codegraph-out/eval/scale"));
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("creating {}", out_dir.display()))?;
        std::fs::write(out_dir.join("report.json"), serde_json::to_string_pretty(&report)?)?;
        std::fs::write(out_dir.join("report.md"), &md)?;
        print!("{md}");
        println!("  report: {}", out_dir.join("report.json").display());
    }
    if !report.skipped.is_empty() {
        for s in &report.skipped {
            eprintln!("SKIPPED {}: {}", s.url, s.reason);
        }
        eprintln!(
            "warning: {} repo(s) skipped; scale results are partial",
            report.skipped.len()
        );
    }
    Ok(())
}

fn scale_markdown(report: &ScaleReport) -> String {
    let mut s = String::from("# Extraction scale\n\n");
    let e = &report.env;
    s.push_str(&format!(
        "Environment: {} / {} / {} logical CPUs / codegraph {}. Median over {} rep(s); cold clears the AST cache first, warm is cache-hot, incremental re-extracts one file.\n\n",
        e.os,
        e.arch,
        e.logical_cpus,
        e.codegraph_version,
        report.results.first().map(|r| r.reps).unwrap_or(0),
    ));
    if report.results.is_empty() {
        s.push_str("No repositories measured (all skipped or filtered).\n");
    } else {
        s.push_str("| Repo | Family | Tier | Files | LOC | Nodes | Edges | Cold med/p95 (s) | Warm med/p95 (s) | Incr (s) | Files/s |\n");
        s.push_str("|---|---|---|--:|--:|--:|--:|--:|--:|--:|--:|\n");
        for r in &report.results {
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {:.2}/{:.2} | {:.2}/{:.2} | {:.3} | {:.0} |\n",
                r.name,
                r.family,
                r.tier,
                r.files,
                r.lines,
                r.nodes,
                r.edges,
                r.cold_secs_median,
                r.cold_secs_p95,
                r.warm_secs_median,
                r.warm_secs_p95,
                r.incremental_secs_median,
                r.warm_files_per_sec(),
            ));
        }
    }
    if !report.skipped.is_empty() {
        s.push_str(&format!(
            "\n**{} repo(s) skipped** (results partial):\n",
            report.skipped.len()
        ));
        for sk in &report.skipped {
            s.push_str(&format!("- {}: {}\n", sk.url, sk.reason));
        }
    }
    s
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
    if r.n == 0 {
        s.push_str("No multi-file commits in range, so there is nothing to calibrate.\n");
        return s;
    }
    s.push_str(&format!(
        "Over {} prediction(s); base rate {:.0}%.\n\n",
        r.n,
        r.base_rate * 100.0
    ));
    s.push_str(&format!(
        "- Brier score: **{:.3}** (0 perfect; baseline-at-base-rate is {:.3}).\n",
        r.brier, r.brier_baseline
    ));
    s.push_str(&format!(
        "- Brier skill score: **{:+.3}** vs always-guess-base-rate (>0 is better than guessing).\n",
        r.brier_skill_score
    ));
    s.push_str(&format!(
        "- Expected calibration error: **{:.3}** (0 means confidence matches reality).\n\n",
        r.ece
    ));
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
    // Preflight gate: a labeled symbol that does not resolve means the extractor
    // dropped a node the ground truth references. Fail loudly rather than let it
    // silently shrink a denominator (this is what makes the metrics trustworthy).
    let unresolved = report.unresolved();
    if !unresolved.is_empty() {
        for (fixture, label) in &unresolved {
            eprintln!("unresolved label: {fixture} :: {label}");
        }
        bail!(
            "{} labeled symbol(s) did not resolve; corpus metrics are not trustworthy until fixed",
            unresolved.len()
        );
    }
    Ok(())
}

fn corpus_markdown(report: &CorpusReport) -> String {
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

    let total_labels: usize = report.fixtures.iter().map(|f| f.resolution.total).sum();
    let unresolved = report.unresolved().len();

    let mut s = String::from("# Accuracy corpus\n\n");
    s.push_str(&format!(
        "Preflight: {}/{} labeled symbol(s) resolved{}.\n\n",
        total_labels - unresolved,
        total_labels,
        if unresolved == 0 {
            ""
        } else {
            " — UNRESOLVED LABELS PRESENT; metrics not trustworthy"
        }
    ));
    s.push_str("Exact set-comparison against hand-labeled ground truth. Call P/R/F1 over `calls` edges; affected-test recall over labeled test linkage; blast columns are recall / distractor-exclusion / avg impact-set size; cross P/R/F1 needs labeled non-couplings for precision (else recall only).\n\n");
    s.push_str("| Fixture | Family | Call P/R/F1 | Aff-test rec | Blast rec/excl/size | Cross P/R/F1 |\n");
    s.push_str("|---|---|---|---|---|---|\n");
    for f in &report.fixtures {
        let blast = if f.blast.expected == 0 && f.blast.distractors_total == 0 {
            "n/a".to_string()
        } else {
            format!(
                "{}%/{}%/{:.1}",
                f.blast.recall_pct(),
                f.blast.distractor_exclusion_pct(),
                f.blast.avg_predicted_size(),
            )
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
    let call = report.pooled_call_edges();
    let tests = report.pooled_affected_tests();
    let cross = report.pooled_cross_edges();
    s.push_str(&format!(
        "\nPooled call-edge: precision {}% / recall {}% / F1 {}% over {} labeled edge(s).\n",
        call.precision_pct(),
        call.recall_pct(),
        call.f1_pct(),
        call.true_positive + call.false_negative,
    ));
    if tests.true_positive + tests.false_negative > 0 {
        s.push_str(&format!(
            "Pooled affected-test recall: {}% over {} labeled test linkage(s).\n",
            tests.recall_pct(),
            tests.true_positive + tests.false_negative,
        ));
    }
    if cross.true_positive + cross.false_positive + cross.false_negative > 0 {
        s.push_str(&format!(
            "Pooled cross-language: precision {}% / recall {}% / F1 {}% ({} coupling(s), {} distractor false-positive(s)).\n",
            cross.precision_pct(),
            cross.recall_pct(),
            cross.f1_pct(),
            cross.true_positive + cross.false_negative,
            cross.false_positive,
        ));
    }
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
