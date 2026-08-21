//! `eval` command: measure forecast quality by replaying history. Re-predicts
//! each commit from its parent-state graph and scores the prediction against git
//! ground truth, so prediction quality can be tracked and gated like any other
//! metric.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};

use synaptic_eval::{
    Baselines, CalibrationReport, CorpusManifest, CorpusRepo, CorpusReport, FixtureReport,
    LanguageQuality, Manifest, OracleOutcome, PrF1, QualityFilter, QualityReport, ReplayOptions,
    ReplayReport, ScaleReport, baselines, calibrate_cross_language, calibrate_history,
    pin_manifest, replay, run_corpus, run_quality, run_scale, score_graph,
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
        EvalAction::HeadToHead {
            projects,
            graphify_root,
            graphify_python,
            root,
            fixture,
            manifest,
            cache,
            repo,
            reps,
            out,
            json,
            allow_skips,
        } => {
            if projects {
                run_project_head_to_head_cmd(ProjectHeadToHeadArgs {
                    graphify_root,
                    graphify_python,
                    manifest,
                    cache,
                    repo,
                    reps,
                    out,
                    json,
                    allow_skips,
                })
            } else {
                run_head_to_head_cmd(
                    graphify_root,
                    graphify_python,
                    root,
                    fixture,
                    reps,
                    out,
                    json,
                )
            }
        }
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
            allow_skips,
        } => run_scale_cmd(manifest, tier, reps, cache, out, json, allow_skips),
        EvalAction::Quality {
            manifest,
            baselines,
            language,
            repo,
            skip_oracle,
            cache,
            out,
            json,
            allow_skips,
            pin,
            update_baselines,
            allow_regression,
        } => run_quality_cmd(QualityArgs {
            manifest,
            baselines,
            language,
            repo,
            skip_oracle,
            cache,
            out,
            json,
            allow_skips,
            pin,
            update_baselines,
            allow_regression,
        }),
    }
}

fn default_scale_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../crates/synaptic-eval/repo-corpus.toml")
}

fn default_baselines() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/synaptic-eval/quality-baselines.toml")
}

struct QualityArgs {
    manifest: Option<PathBuf>,
    baselines: Option<PathBuf>,
    language: Option<String>,
    repo: Option<String>,
    skip_oracle: bool,
    cache: Option<PathBuf>,
    out: Option<PathBuf>,
    json: bool,
    allow_skips: bool,
    pin: bool,
    update_baselines: bool,
    allow_regression: bool,
}

fn run_quality_cmd(args: QualityArgs) -> Result<()> {
    let manifest = args.manifest.unwrap_or_else(default_scale_manifest);
    if !manifest.exists() {
        bail!(
            "no corpus manifest at {} (pass --manifest)",
            manifest.display()
        );
    }

    if args.pin {
        let (pinned, failures) = pin_manifest(&manifest).map_err(|e| anyhow!("pinning: {e}"))?;
        println!("pinned {pinned} repositories in {}", manifest.display());
        for f in &failures {
            eprintln!("UNRESOLVED {f}");
        }
        if !failures.is_empty() {
            bail!(
                "{} repository URL(s) could not be resolved; their existing pins were kept",
                failures.len()
            );
        }
        return Ok(());
    }

    let cache = args
        .cache
        .unwrap_or_else(|| PathBuf::from("synaptic-out/bench"));
    let filter = QualityFilter {
        language: args.language,
        repo: args.repo,
        skip_oracle: args.skip_oracle,
    };
    let report =
        run_quality(&manifest, &cache, &filter).map_err(|e| anyhow!("quality run: {e}"))?;

    let md = quality_markdown(&report);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let out_dir = args
            .out
            .unwrap_or_else(|| PathBuf::from("synaptic-out/eval/quality"));
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("creating {}", out_dir.display()))?;
        std::fs::write(
            out_dir.join("report.json"),
            serde_json::to_string_pretty(&report)?,
        )?;
        std::fs::write(out_dir.join("report.md"), &md)?;
        print!("{md}");
        println!("  report: {}", out_dir.join("report.json").display());
    }

    let baselines_path = args.baselines.unwrap_or_else(default_baselines);
    let existing = if baselines_path.exists() {
        Baselines::parse(&std::fs::read_to_string(&baselines_path)?)
            .map_err(|e| anyhow!("parsing {}: {e}", baselines_path.display()))?
    } else {
        Baselines::default()
    };

    // The gate is evaluated against whatever the baselines are AFTER an update,
    // so a run that just pinned 53 repositories does not then report all 53 as
    // unpinned.
    let mut effective = existing.clone();
    if args.update_baselines {
        match baselines::ratchet(&existing, &report.results, args.allow_regression) {
            Ok(next) => {
                std::fs::write(&baselines_path, next.render())?;
                println!("  baselines updated: {}", baselines_path.display());
                effective = next;
            }
            Err(loosened) => {
                for l in &loosened {
                    eprintln!("REFUSED {l}");
                }
                bail!(
                    "refusing to loosen {} baseline bound(s); pass --allow-regression to record \
                     the regression deliberately",
                    loosened.len()
                );
            }
        }
    }

    // Self-consistency has a principled correct answer, so it hard-fails
    // regardless of any baseline.
    let inconsistent: Vec<&synaptic_eval::RepoQuality> = report
        .results
        .iter()
        .filter(|r| !r.consistency.deterministic || !r.consistency.incremental_equivalent)
        .collect();
    for r in &inconsistent {
        eprintln!(
            "INCONSISTENT {}: {}",
            r.name,
            r.consistency.detail.as_deref().unwrap_or("(no detail)")
        );
    }

    let (breaches, unpinned) = baselines::check(&effective, &report.results);
    for b in &breaches {
        eprintln!("REGRESSION {b}");
    }
    if !unpinned.is_empty() {
        eprintln!(
            "note: {} repo(s) carry no baseline yet ({}); run --update-baselines to pin them",
            unpinned.len(),
            unpinned.join(", ")
        );
    }

    if !report.skipped.is_empty() {
        for s in &report.skipped {
            eprintln!("SKIPPED {}: {}", s.url, s.reason);
        }
        eprintln!(
            "warning: {} repo(s) skipped; quality results are partial",
            report.skipped.len()
        );
    }

    if !inconsistent.is_empty() {
        bail!(
            "{} repo(s) failed a self-consistency assertion",
            inconsistent.len()
        );
    }
    if !breaches.is_empty() && !args.update_baselines {
        bail!("{} baseline bound(s) breached", breaches.len());
    }
    if !report.skipped.is_empty() && !args.allow_skips {
        bail!("incomplete quality run (pass --allow-skips for exploratory runs)");
    }
    Ok(())
}

fn quality_markdown(report: &QualityReport) -> String {
    let mut s = String::from("# Extraction quality at scale\n\n");
    let e = &report.env;
    s.push_str(&format!(
        "{} repositories measured on {}/{} ({} logical CPUs), Synaptic {}.\n",
        report.results.len(),
        e.os,
        e.arch,
        e.logical_cpus,
        e.synaptic_version
    ));
    if let Some(rev) = &e.source_revision {
        s.push_str(&format!(
            "Source revision `{rev}`{}.\n",
            match e.source_dirty {
                Some(true) => " with a dirty working tree",
                _ => "",
            }
        ));
    }
    if !report.oracle_available {
        s.push_str(&format!(
            "\n> **Oracle stage did not run.** {}\n> The anchor, parse-health and self-consistency \
             measurements below are unaffected.\n",
            report
                .oracle_unavailable_reason
                .as_deref()
                .unwrap_or("reason not recorded")
        ));
    }

    s.push_str("\n## Per language (pooled)\n\n");
    s.push_str(
        "| Language | Files | Anchors ok/checked | Exact | via annot. | Parse err | Zero-decl | Recovered ok/checked |\n\
         |---|--:|--:|--:|--:|--:|--:|--:|\n",
    );
    for l in report.pooled_by_language() {
        s.push_str(&format!(
            "| {} | {} | {}/{} | {:.2}% | {} | {:.2}% | {:.2}% | {}/{} |\n",
            l.language,
            l.files,
            l.anchors_exact,
            l.anchors_checked,
            l.anchor_exactness() * 100.0,
            l.anchors_via_leading_matter,
            l.parse_error_rate() * 100.0,
            l.zero_decl_file_rate() * 100.0,
            l.recovered_exact,
            l.recovered_checked,
        ));
    }
    s.push_str(
        "\n`via annot.` counts anchors that land on the head of the declaration's \
         annotation/attribute block rather than on its signature line. Those are correct -- an \
         annotated declaration's syntax node starts at its first annotation -- but they are \
         reported separately rather than folded in silently.\n",
    );

    s.push_str("\n## Per repository\n\n");
    s.push_str(
        "| Repo | Family | Files | Nodes | Anchors ok/checked | Exact | Determinism | Incremental |\n\
         |---|---|--:|--:|--:|--:|:-:|:-:|\n",
    );
    for r in &report.results {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {}/{} | {:.2}% | {} | {} |\n",
            r.name,
            r.family,
            r.files,
            r.nodes,
            r.anchors_exact,
            r.anchors_checked,
            r.anchor_exactness() * 100.0,
            if r.consistency.deterministic {
                "pass"
            } else {
                "FAIL"
            },
            if r.consistency.incremental_equivalent {
                "pass"
            } else {
                "FAIL"
            },
        ));
    }

    let checked: usize = report.results.iter().map(|r| r.anchors_checked).sum();
    s.push_str(&format!(
        "\nPooled anchor exactness: **{:.4}%** over {} checked declarations.\n",
        report.pooled_anchor_exactness() * 100.0,
        checked
    ));

    if report.oracle_available {
        s.push_str("\n## Independent oracle (universal-ctags)\n\n");
        s.push_str("| Repo | Language | Agree | ctags-only | synaptic-only | Missed |\n|---|---|--:|--:|--:|--:|\n");
        for r in &report.results {
            for l in &r.oracle.per_language {
                s.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {:.2}% |\n",
                    r.name,
                    l.language,
                    l.agreement,
                    l.ctags_only,
                    l.synaptic_only,
                    l.missed_rate() * 100.0
                ));
            }
        }
        s.push_str(
            "\n`ctags-only` is the actionable column. `synaptic-only` is expected: Synaptic models \
             methods, framework constructs and cross-file structure that ctags never emits.\n",
        );
    }

    if !report.skipped.is_empty() {
        s.push_str("\n## Skipped\n\n");
        for k in &report.skipped {
            s.push_str(&format!("- `{}`: {}\n", k.url, k.reason));
        }
    }
    s
}

fn run_scale_cmd(
    manifest: Option<PathBuf>,
    tier: Option<String>,
    reps: usize,
    cache: Option<PathBuf>,
    out: Option<PathBuf>,
    json: bool,
    allow_skips: bool,
) -> Result<()> {
    let manifest = manifest.unwrap_or_else(default_scale_manifest);
    if !manifest.exists() {
        bail!(
            "no scale manifest at {} (pass --manifest)",
            manifest.display()
        );
    }
    let cache = cache.unwrap_or_else(|| PathBuf::from("synaptic-out/bench"));
    let report = run_scale(&manifest, &cache, tier.as_deref(), reps)
        .map_err(|e| anyhow!("scale run: {e}"))?;
    let md = scale_markdown(&report);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let out_dir = out.unwrap_or_else(|| PathBuf::from("synaptic-out/eval/scale"));
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("creating {}", out_dir.display()))?;
        std::fs::write(
            out_dir.join("report.json"),
            serde_json::to_string_pretty(&report)?,
        )?;
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
        if !allow_skips {
            bail!("incomplete scale run (pass --allow-skips for exploratory runs)");
        }
    }
    Ok(())
}

fn scale_markdown(report: &ScaleReport) -> String {
    let mut s = String::from("# Extraction scale\n\n");
    let e = &report.env;
    let reps = report.results.first().map(|r| r.reps).unwrap_or(0);
    let tail = if reps < 20 { "max" } else { "p95" };
    s.push_str(&format!(
        "Environment: {} / {} / {} logical CPUs / synaptic {} / source {}{}. Median over {} rep(s); tail is {}; cold clears the AST cache first, warm is cache-hot, incremental re-extracts one file.\n\n",
        e.os,
        e.arch,
        e.logical_cpus,
        e.synaptic_version,
        e.source_revision.as_deref().unwrap_or("unknown"),
        if e.source_dirty == Some(true) { " (dirty)" } else { "" },
        reps,
        tail,
    ));
    if report.results.is_empty() {
        s.push_str("No repositories measured (all skipped or filtered).\n");
    } else {
        s.push_str(&format!("| Repo | SHA | Family | Tier | Files | LOC | Nodes | Edges | Cold med/{tail} (s) | Warm med/{tail} (s) | Unchanged-file incr (s) | LOC/s |\n"));
        s.push_str("|---|---|---|---|--:|--:|--:|--:|--:|--:|--:|--:|\n");
        for r in &report.results {
            s.push_str(&format!(
                "| {} | `{}` | {} | {} | {} | {} | {} | {} | {:.2}/{:.2} | {:.2}/{:.2} | {:.3} | {:.0} |\n",
                r.name,
                &r.sha[..r.sha.len().min(12)],
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
                r.warm_loc_per_sec(),
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
        let out_dir = out.unwrap_or_else(|| PathBuf::from("synaptic-out/eval/calibrate"));
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("creating {}", out_dir.display()))?;
        std::fs::write(
            out_dir.join("report.json"),
            serde_json::to_string_pretty(&report)?,
        )?;
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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../crates/synaptic-eval/corpus")
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
        let out_dir = out.unwrap_or_else(|| PathBuf::from("synaptic-out/eval/corpus"));
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("creating {}", out_dir.display()))?;
        std::fs::write(
            out_dir.join("report.json"),
            serde_json::to_string_pretty(&report)?,
        )?;
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
    let prf1 = |p: &synaptic_eval::PrF1| {
        if p.true_positive + p.false_positive + p.false_negative == 0 {
            "n/a".to_string()
        } else {
            format!("{}/{}/{}", p.precision_pct(), p.recall_pct(), p.f1_pct())
        }
    };
    let recall = |p: &synaptic_eval::PrF1| {
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
    s.push_str(
        "| Fixture | Family | Call P/R/F1 | Aff-test rec | Blast rec/excl/size | Cross P/R/F1 |\n",
    );
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

#[derive(serde::Serialize)]
struct HeadToHeadReport {
    schema: &'static str,
    generated_at_unix: u64,
    corpus_root: String,
    repetitions: usize,
    synaptic_revision: String,
    graphify_revision: String,
    tools: Vec<ToolBenchmark>,
}

#[derive(serde::Serialize)]
struct ToolBenchmark {
    name: String,
    summary: Headline,
    fixtures: Vec<FixtureBenchmark>,
}

#[derive(serde::Serialize)]
struct FixtureBenchmark {
    fixture: String,
    family: String,
    nodes: usize,
    edges: usize,
    times_ms: Vec<f64>,
    scores: Headline,
    metrics: FixtureReport,
}

#[derive(Debug, serde::Serialize)]
struct Headline {
    /// Micro-F1 pooled across calls, affected tests, blast radius, and
    /// cross-language coupling labels.
    quality_f1_pct: f64,
    /// Set/Jaccard accuracy: TP / (TP + FP + FN). There is no invented
    /// true-negative universe for graph extraction.
    accuracy_pct: f64,
    precision_pct: f64,
    recall_pct: f64,
    label_resolution_pct: f64,
    /// Median fresh-build time for a fixture, or the sum of fixture medians for
    /// a whole-tool summary.
    cold_ms: f64,
}

struct ProjectHeadToHeadArgs {
    graphify_root: PathBuf,
    graphify_python: Option<PathBuf>,
    manifest: Option<PathBuf>,
    cache: Option<PathBuf>,
    repo: Option<String>,
    reps: usize,
    out: Option<PathBuf>,
    json: bool,
    allow_skips: bool,
}

#[derive(serde::Serialize)]
struct ProjectHeadToHeadReport {
    schema: &'static str,
    generated_at_unix: u64,
    repetitions: usize,
    manifest: String,
    synaptic_revision: String,
    graphify_revision: String,
    tools: Vec<ProjectToolBenchmark>,
    skipped: Vec<ProjectSkip>,
}

#[derive(serde::Serialize)]
struct ProjectToolBenchmark {
    name: String,
    summary: ProjectHeadline,
    counts: ProjectCounts,
    projects: Vec<ProjectBenchmark>,
}

#[derive(serde::Serialize)]
struct ProjectBenchmark {
    project: String,
    url: String,
    sha: String,
    family: String,
    tier: String,
    nodes: usize,
    edges: usize,
    times_ms: Vec<f64>,
    scores: ProjectHeadline,
    counts: ProjectCounts,
    per_language: Vec<LanguageQuality>,
    oracle: OracleOutcome,
}

#[derive(Debug, serde::Serialize)]
struct ProjectHeadline {
    /// Universal Ctags is an independent agreement proxy, not exhaustive truth.
    quality_f1_pct: f64,
    accuracy_pct: f64,
    precision_pct: f64,
    recall_pct: f64,
    anchor_exactness_pct: f64,
    parse_error_file_pct: f64,
    zero_declaration_file_pct: f64,
    cold_ms: f64,
}

#[derive(Clone, Default, serde::Serialize)]
struct ProjectCounts {
    oracle_agreement: usize,
    oracle_only: usize,
    tool_only: usize,
    anchors_exact: usize,
    anchors_checked: usize,
    files: usize,
    parse_error_files: usize,
    zero_declaration_files: usize,
}

impl ProjectCounts {
    fn add(&mut self, other: &Self) {
        self.oracle_agreement += other.oracle_agreement;
        self.oracle_only += other.oracle_only;
        self.tool_only += other.tool_only;
        self.anchors_exact += other.anchors_exact;
        self.anchors_checked += other.anchors_checked;
        self.files += other.files;
        self.parse_error_files += other.parse_error_files;
        self.zero_declaration_files += other.zero_declaration_files;
    }
}

#[derive(serde::Serialize)]
struct ProjectSkip {
    url: String,
    reason: String,
}

#[derive(Default)]
struct Counts {
    tp: usize,
    fp: usize,
    r#fn: usize,
}

impl Counts {
    fn add(&mut self, score: &PrF1) {
        self.tp += score.true_positive;
        self.fp += score.false_positive;
        self.r#fn += score.false_negative;
    }
}

enum BenchTool<'a> {
    Synaptic(&'a Path),
    Graphify { python: &'a Path, root: &'a Path },
}

#[allow(clippy::too_many_arguments)]
fn run_head_to_head_cmd(
    graphify_root: PathBuf,
    graphify_python: Option<PathBuf>,
    root: Option<PathBuf>,
    fixture: Option<String>,
    reps: usize,
    out: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    if reps == 0 {
        bail!("--reps must be at least 1");
    }
    let corpus_root = root
        .unwrap_or_else(default_corpus_root)
        .canonicalize()
        .context("resolving corpus root")?;
    let (graphify_root, graphify_python) = graphify_paths(graphify_root, graphify_python)?;

    let mut manifest = Manifest::parse(
        &std::fs::read_to_string(corpus_root.join("manifest.toml"))
            .context("reading corpus manifest")?,
    )
    .context("parsing corpus manifest")?;
    if let Some(name) = fixture {
        manifest.fixtures.retain(|f| f.dir == name);
        if manifest.fixtures.is_empty() {
            bail!("fixture {name:?} is not in the corpus manifest");
        }
    }
    if manifest.fixtures.is_empty() {
        bail!("corpus manifest contains no fixtures");
    }

    let synaptic_exe = std::env::current_exe().context("locating the Synaptic executable")?;
    let synaptic = benchmark_tool(
        "synaptic",
        BenchTool::Synaptic(&synaptic_exe),
        &corpus_root,
        &manifest,
        reps,
    )?;
    let graphify = benchmark_tool(
        "graphify",
        BenchTool::Graphify {
            python: &graphify_python,
            root: &graphify_root,
        },
        &corpus_root,
        &manifest,
        reps,
    )?;
    let report = HeadToHeadReport {
        schema: "synaptic.head-to-head/v1",
        generated_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        corpus_root: corpus_root.display().to_string(),
        repetitions: reps,
        synaptic_revision: git_revision(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")),
        graphify_revision: git_revision(&graphify_root),
        tools: vec![synaptic, graphify],
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let out = out.unwrap_or_else(|| PathBuf::from("synaptic-out/eval/head-to-head"));
        std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
        let markdown = head_to_head_markdown(&report);
        std::fs::write(
            out.join("report.json"),
            serde_json::to_string_pretty(&report)?,
        )?;
        std::fs::write(out.join("report.md"), &markdown)?;
        print!("{markdown}");
        println!("  report: {}", out.join("report.json").display());
    }
    Ok(())
}

fn graphify_paths(root: PathBuf, python: Option<PathBuf>) -> Result<(PathBuf, PathBuf)> {
    let root = root
        .canonicalize()
        .context("resolving Graphify checkout (pass --graphify-root)")?;
    let python = python.unwrap_or_else(|| {
        if cfg!(windows) {
            root.join(".venv/Scripts/python.exe")
        } else {
            root.join(".venv/bin/python")
        }
    });
    if !python.is_file() {
        bail!(
            "Graphify Python not found at {} (pass --graphify-python)",
            python.display()
        );
    }
    Ok((root, python))
}

fn run_project_head_to_head_cmd(args: ProjectHeadToHeadArgs) -> Result<()> {
    if args.reps == 0 {
        bail!("--reps must be at least 1");
    }
    synaptic_eval::oracle::probe()
        .map_err(|e| anyhow!("project precision/recall needs Universal Ctags: {e}"))?;
    let manifest_path = args.manifest.unwrap_or_else(default_scale_manifest);
    let manifest = CorpusManifest::parse(
        &std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?,
    )
    .context("parsing project manifest")?;
    let repos: Vec<CorpusRepo> = manifest
        .in_suite(synaptic_eval::repo_corpus::SUITE_SCALE)
        .filter(|r| args.repo.as_deref().is_none_or(|name| r.name() == name))
        .cloned()
        .collect();
    if repos.is_empty() {
        bail!("no matching projects in the manifest's scale suite");
    }

    let cache = args
        .cache
        .unwrap_or_else(|| PathBuf::from("synaptic-out/bench"));
    let (graphify_root, graphify_python) =
        graphify_paths(args.graphify_root, args.graphify_python)?;
    let synaptic_exe = std::env::current_exe().context("locating the Synaptic executable")?;
    let mut synaptic = Vec::new();
    let mut graphify = Vec::new();
    let mut skipped = Vec::new();

    for repo in &repos {
        eprintln!("project: {} ({})", repo.name(), repo.sha);
        let checkout = match synaptic_eval::scale::ensure_checkout(&cache, repo)
            .and_then(|path| path.canonicalize().map_err(|err| err.to_string()))
        {
            Ok(path) => path,
            Err(reason) => {
                skipped.push(ProjectSkip {
                    url: repo.url.clone(),
                    reason,
                });
                continue;
            }
        };
        let syn = benchmark_project(
            "synaptic",
            BenchTool::Synaptic(&synaptic_exe),
            &checkout,
            repo,
            args.reps,
        );
        let graph = benchmark_project(
            "graphify",
            BenchTool::Graphify {
                python: &graphify_python,
                root: &graphify_root,
            },
            &checkout,
            repo,
            args.reps,
        );
        match (syn, graph) {
            (Ok(s), Ok(g)) => {
                synaptic.push(s);
                graphify.push(g);
            }
            (s, g) => skipped.push(ProjectSkip {
                url: repo.url.clone(),
                reason: format!(
                    "synaptic: {}; graphify: {}",
                    s.err()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "ok".into()),
                    g.err()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "ok".into()),
                ),
            }),
        }
    }

    let report = ProjectHeadToHeadReport {
        schema: "synaptic.project-head-to-head/v1",
        generated_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        repetitions: args.reps,
        manifest: manifest_path.display().to_string(),
        synaptic_revision: git_revision(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")),
        graphify_revision: git_revision(&graphify_root),
        tools: vec![
            finish_project_tool("synaptic", synaptic),
            finish_project_tool("graphify", graphify),
        ],
        skipped,
    };
    if report.tools[0].projects.is_empty() {
        bail!(
            "no projects completed for both tools: {}",
            report
                .skipped
                .iter()
                .map(|s| format!("{}: {}", s.url, s.reason))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let out = args
            .out
            .unwrap_or_else(|| PathBuf::from("synaptic-out/eval/head-to-head-projects"));
        std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
        let markdown = project_head_to_head_markdown(&report);
        std::fs::write(
            out.join("report.json"),
            serde_json::to_string_pretty(&report)?,
        )?;
        std::fs::write(out.join("report.md"), &markdown)?;
        print!("{markdown}");
        println!("  report: {}", out.join("report.json").display());
    }
    if !report.skipped.is_empty() && !args.allow_skips {
        bail!(
            "{} project(s) skipped (pass --allow-skips for an exploratory run)",
            report.skipped.len()
        );
    }
    Ok(())
}

fn benchmark_project(
    name: &str,
    tool: BenchTool<'_>,
    checkout: &Path,
    repo: &CorpusRepo,
    reps: usize,
) -> Result<ProjectBenchmark> {
    let mut times_ms = Vec::with_capacity(reps);
    let mut measured_graph = None;
    for run in 1..=reps {
        eprintln!("  {name}: run {run}/{reps}");
        clear_project_outputs(checkout)?;
        let temp = tempfile::tempdir().context("creating benchmark directory")?;
        let started = Instant::now();
        let graph_path = run_tool(&tool, checkout, temp.path(), true)?;
        times_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        measured_graph = Some(read_graph(&graph_path)?);
    }
    clear_project_outputs(checkout)?;
    let graph = measured_graph.expect("at least one repetition");
    let per_language = synaptic_eval::quality::score_graph(checkout, &graph);
    let oracle = synaptic_eval::oracle::compare(checkout, &graph);
    if !oracle.available {
        bail!(
            "ctags unavailable: {}",
            oracle.reason.as_deref().unwrap_or("unknown reason")
        );
    }
    let counts = project_counts(&per_language, &oracle);
    let scores = project_headline(&counts, median(&times_ms));
    Ok(ProjectBenchmark {
        project: repo.name().to_string(),
        url: repo.url.clone(),
        sha: repo.sha.clone(),
        family: repo.family.clone(),
        tier: repo.tier.clone(),
        nodes: graph.nodes.len(),
        edges: graph.links.len(),
        times_ms,
        scores,
        counts,
        per_language,
        oracle,
    })
}

fn clear_project_outputs(root: &Path) -> Result<()> {
    for name in ["synaptic-out", "graphify-out"] {
        let path = root.join(name);
        if path.exists() {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("removing stale {}", path.display()))?;
        }
    }
    Ok(())
}

fn project_counts(languages: &[LanguageQuality], oracle: &OracleOutcome) -> ProjectCounts {
    ProjectCounts {
        oracle_agreement: oracle.per_language.iter().map(|l| l.agreement).sum(),
        oracle_only: oracle.per_language.iter().map(|l| l.ctags_only).sum(),
        tool_only: oracle.per_language.iter().map(|l| l.synaptic_only).sum(),
        anchors_exact: languages.iter().map(|l| l.anchors_exact).sum(),
        anchors_checked: languages.iter().map(|l| l.anchors_checked).sum(),
        files: languages.iter().map(|l| l.files).sum(),
        parse_error_files: languages.iter().map(|l| l.parse_error_files).sum(),
        zero_declaration_files: languages.iter().map(|l| l.zero_decl_files).sum(),
    }
}

fn project_headline(counts: &ProjectCounts, cold_ms: f64) -> ProjectHeadline {
    let tp = counts.oracle_agreement;
    let fp = counts.tool_only;
    let r#fn = counts.oracle_only;
    ProjectHeadline {
        quality_f1_pct: percent(2 * tp, 2 * tp + fp + r#fn, 0.0),
        accuracy_pct: percent(tp, tp + fp + r#fn, 100.0),
        precision_pct: percent(tp, tp + fp, 100.0),
        recall_pct: percent(tp, tp + r#fn, 100.0),
        anchor_exactness_pct: percent(counts.anchors_exact, counts.anchors_checked, 100.0),
        parse_error_file_pct: percent(counts.parse_error_files, counts.files, 0.0),
        zero_declaration_file_pct: percent(counts.zero_declaration_files, counts.files, 0.0),
        cold_ms: (cold_ms * 100.0).round() / 100.0,
    }
}

fn finish_project_tool(name: &str, projects: Vec<ProjectBenchmark>) -> ProjectToolBenchmark {
    let mut counts = ProjectCounts::default();
    for project in &projects {
        counts.add(&project.counts);
    }
    let cold_ms = projects.iter().map(|p| p.scores.cold_ms).sum();
    ProjectToolBenchmark {
        name: name.to_string(),
        summary: project_headline(&counts, cold_ms),
        counts,
        projects,
    }
}

fn benchmark_tool(
    name: &str,
    tool: BenchTool<'_>,
    corpus_root: &Path,
    manifest: &Manifest,
    reps: usize,
) -> Result<ToolBenchmark> {
    let mut fixtures = Vec::new();
    for fixture in &manifest.fixtures {
        let original = corpus_root.join(&fixture.dir);
        let mut times_ms = Vec::with_capacity(reps);
        let mut measured_graph = None;
        for run in 1..=reps {
            eprintln!("{name}: {} run {run}/{reps}", fixture.dir);
            let temp = tempfile::tempdir().context("creating benchmark directory")?;
            let source = temp.path().join("fixture");
            copy_fixture(&original, &source)?;
            let started = Instant::now();
            let graph_path = run_tool(&tool, &source, temp.path(), false)?;
            times_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            measured_graph = Some(read_graph(&graph_path)?);
        }
        let graph = measured_graph.expect("at least one repetition");
        let metrics = score_graph(&original, &fixture.dir, &fixture.family, &graph)
            .map_err(|e| anyhow!("scoring {name} on {}: {e}", fixture.dir))?;
        let cold_ms = median(&times_ms);
        let scores = summarize(&[&metrics], cold_ms);
        fixtures.push(FixtureBenchmark {
            fixture: fixture.dir.clone(),
            family: fixture.family.clone(),
            nodes: graph.nodes.len(),
            edges: graph.links.len(),
            times_ms,
            scores,
            metrics,
        });
    }
    Ok(ToolBenchmark {
        name: name.to_string(),
        summary: summarize(
            &fixtures.iter().map(|f| &f.metrics).collect::<Vec<_>>(),
            fixtures.iter().map(|f| f.scores.cold_ms).sum(),
        ),
        fixtures,
    })
}

fn run_tool(tool: &BenchTool<'_>, source: &Path, scratch: &Path, project: bool) -> Result<PathBuf> {
    let (output, graph) = match tool {
        BenchTool::Synaptic(exe) => {
            let mut command = Command::new(exe);
            command
                .arg("extract")
                .arg(source)
                .args(["--directed", "--no-store"]);
            if project {
                command.arg("--no-resources");
            }
            (
                command.output().context("starting Synaptic")?,
                source.join("synaptic-out/graph.json"),
            )
        }
        BenchTool::Graphify { python, root } => {
            let out = scratch.join("graphify");
            (
                Command::new(python)
                    .current_dir(root)
                    .args(["-m", "graphify", "extract"])
                    .arg(source)
                    .arg("--out")
                    .arg(&out)
                    .args(["--code-only", "--force"])
                    .output()
                    .context("starting Graphify")?,
                out.join("graphify-out/graph.json"),
            )
        }
    };
    require_success(output)?;
    if !graph.is_file() {
        bail!("tool succeeded but did not write {}", graph.display());
    }
    Ok(graph)
}

fn require_success(output: Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "tool exited with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn read_graph(path: &Path) -> Result<synaptic_core::GraphData> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

fn copy_fixture(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "synaptic-out" || name == "graphify-out" {
            continue;
        }
        let target = to.join(&name);
        if entry.file_type()?.is_dir() {
            copy_fixture(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn summarize(reports: &[&FixtureReport], cold_ms: f64) -> Headline {
    let mut counts = Counts::default();
    let mut labels = 0usize;
    let mut unresolved = 0usize;
    for report in reports {
        counts.add(&report.call_edges);
        counts.add(&report.affected_tests);
        counts.add(&report.cross_edges);
        counts.tp += report.blast.found;
        counts.fp += report.blast.distractors_hit;
        counts.r#fn += report.blast.missed;
        labels += report.resolution.total;
        unresolved += report.resolution.unresolved.len();
    }
    Headline {
        quality_f1_pct: percent(2 * counts.tp, 2 * counts.tp + counts.fp + counts.r#fn, 0.0),
        accuracy_pct: percent(counts.tp, counts.tp + counts.fp + counts.r#fn, 100.0),
        precision_pct: percent(counts.tp, counts.tp + counts.fp, 100.0),
        recall_pct: percent(counts.tp, counts.tp + counts.r#fn, 100.0),
        label_resolution_pct: percent(labels - unresolved, labels, 100.0),
        cold_ms: (cold_ms * 100.0).round() / 100.0,
    }
}

fn percent(numerator: usize, denominator: usize, empty: f64) -> f64 {
    if denominator == 0 {
        empty
    } else {
        (numerator as f64 * 10_000.0 / denominator as f64).round() / 100.0
    }
}

fn median(values: &[f64]) -> f64 {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2.0
    }
}

fn git_revision(root: &Path) -> String {
    let head = Command::new("git")
        .args(["-C"])
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = Command::new("git")
        .args(["-C"])
        .arg(root)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .is_some_and(|o| o.status.success() && !o.stdout.is_empty());
    if dirty { format!("{head}+dirty") } else { head }
}

fn head_to_head_markdown(report: &HeadToHeadReport) -> String {
    let mut out = String::from("# Synaptic vs Graphify\n\n");
    out.push_str(&format!(
        "Synaptic `{}` vs Graphify `{}`; {} fresh run(s) per fixture. Quality is pooled relation F1; accuracy is set/Jaccard accuracy (`TP / (TP + FP + FN)`); cold time is the sum of per-fixture medians.\n\n",
        short_revision(&report.synaptic_revision),
        short_revision(&report.graphify_revision),
        report.repetitions,
    ));
    out.push_str(
        "| Tool | Quality F1 | Accuracy | Precision | Recall | Labels resolved | Cold corpus |\n",
    );
    out.push_str("|---|---:|---:|---:|---:|---:|---:|\n");
    for tool in &report.tools {
        let s = &tool.summary;
        out.push_str(&format!(
            "| {} | {:.2}% | {:.2}% | {:.2}% | {:.2}% | {:.2}% | {:.2} ms |\n",
            tool.name,
            s.quality_f1_pct,
            s.accuracy_pct,
            s.precision_pct,
            s.recall_pct,
            s.label_resolution_pct,
            s.cold_ms,
        ));
    }
    out.push_str(
        "\n| Fixture | Tool | Nodes/edges | Quality F1 | Accuracy | Precision | Cold median |\n",
    );
    out.push_str("|---|---|---:|---:|---:|---:|---:|\n");
    for tool in &report.tools {
        for fixture in &tool.fixtures {
            let s = &fixture.scores;
            out.push_str(&format!(
                "| {} | {} | {}/{} | {:.2}% | {:.2}% | {:.2}% | {:.2} ms |\n",
                fixture.fixture,
                tool.name,
                fixture.nodes,
                fixture.edges,
                s.quality_f1_pct,
                s.accuracy_pct,
                s.precision_pct,
                s.cold_ms,
            ));
        }
    }
    out
}

fn project_head_to_head_markdown(report: &ProjectHeadToHeadReport) -> String {
    let mut out = String::from("# Synaptic vs Graphify on pinned projects\n\n");
    out.push_str(&format!(
        "Synaptic `{}` vs Graphify `{}`; {} fresh run(s) per project. Quality, accuracy, precision, and recall are agreement proxies against Universal Ctags over the same detector-selected code files and structural declaration kinds; anchor exactness is checked directly against source lines.\n\n",
        short_revision(&report.synaptic_revision),
        short_revision(&report.graphify_revision),
        report.repetitions,
    ));
    out.push_str("| Tool | Projects | Quality F1 | Accuracy | Precision | Recall | Anchor exact | Parse errors | Cold total |\n");
    out.push_str("|---|---:|---:|---:|---:|---:|---:|---:|---:|\n");
    for tool in &report.tools {
        let s = &tool.summary;
        out.push_str(&format!(
            "| {} | {} | {:.2}% | {:.2}% | {:.2}% | {:.2}% | {:.2}% | {:.2}% | {:.2} s |\n",
            tool.name,
            tool.projects.len(),
            s.quality_f1_pct,
            s.accuracy_pct,
            s.precision_pct,
            s.recall_pct,
            s.anchor_exactness_pct,
            s.parse_error_file_pct,
            s.cold_ms / 1000.0,
        ));
    }
    out.push_str("\n| Project | Tool | Nodes/edges | Quality F1 | Accuracy | Precision | Anchor exact | Cold median |\n");
    out.push_str("|---|---|---:|---:|---:|---:|---:|---:|\n");
    for tool in &report.tools {
        for project in &tool.projects {
            let s = &project.scores;
            out.push_str(&format!(
                "| {} | {} | {}/{} | {:.2}% | {:.2}% | {:.2}% | {:.2}% | {:.2} s |\n",
                project.project,
                tool.name,
                project.nodes,
                project.edges,
                s.quality_f1_pct,
                s.accuracy_pct,
                s.precision_pct,
                s.anchor_exactness_pct,
                s.cold_ms / 1000.0,
            ));
        }
    }
    if !report.skipped.is_empty() {
        out.push_str("\n## Skipped\n\n");
        for skip in &report.skipped {
            out.push_str(&format!("- {}: {}\n", skip.url, skip.reason));
        }
    }
    out
}

fn short_revision(revision: &str) -> String {
    let dirty = revision.ends_with("+dirty");
    format!(
        "{}{}",
        revision.chars().take(8).collect::<String>(),
        if dirty { "+dirty" } else { "" }
    )
}

/// Calibrate the cross-language edge layer over a built graph.json.
fn run_cross_language(graph_path: PathBuf, json: bool) -> Result<()> {
    let bytes =
        std::fs::read(&graph_path).with_context(|| format!("reading {}", graph_path.display()))?;
    let graph: synaptic_core::GraphData = serde_json::from_slice(&bytes)
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
        let out_dir = a.out.unwrap_or_else(|| PathBuf::from("synaptic-out/eval"));
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

#[cfg(test)]
mod head_to_head_tests {
    use super::*;
    use synaptic_eval::{BlastScore, ResolutionReport};

    #[test]
    fn headline_pools_raw_counts_and_uses_the_median() {
        let fixture = FixtureReport {
            dir: "fixture".into(),
            family: "test".into(),
            resolution: ResolutionReport {
                total: 4,
                unresolved: vec!["missing".into()],
            },
            call_edges: PrF1 {
                true_positive: 2,
                false_positive: 1,
                false_negative: 1,
            },
            affected_tests: PrF1::default(),
            blast: BlastScore {
                found: 1,
                missed: 1,
                distractors_hit: 1,
                ..Default::default()
            },
            cross_edges: PrF1::default(),
        };
        let score = summarize(&[&fixture], median(&[30.0, 10.0, 20.0]));
        assert_eq!(
            (
                score.quality_f1_pct,
                score.accuracy_pct,
                score.precision_pct,
                score.recall_pct,
                score.label_resolution_pct,
                score.cold_ms,
            ),
            (60.0, 42.86, 60.0, 60.0, 75.0, 20.0)
        );
    }

    #[test]
    fn project_headline_keeps_oracle_and_anchor_denominators_separate() {
        let score = project_headline(
            &ProjectCounts {
                oracle_agreement: 8,
                oracle_only: 2,
                tool_only: 6,
                anchors_exact: 9,
                anchors_checked: 10,
                files: 20,
                parse_error_files: 1,
                zero_declaration_files: 2,
            },
            1250.0,
        );
        assert_eq!(
            (
                score.quality_f1_pct,
                score.accuracy_pct,
                score.precision_pct,
                score.recall_pct,
                score.anchor_exactness_pct,
                score.parse_error_file_pct,
                score.zero_declaration_file_pct,
                score.cold_ms,
            ),
            (66.67, 50.0, 57.14, 80.0, 90.0, 5.0, 10.0, 1250.0)
        );
    }
}
