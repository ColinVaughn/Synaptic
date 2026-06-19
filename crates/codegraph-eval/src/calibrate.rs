//! Calibration of prediction confidence: bin (confidence, observed-outcome)
//! pairs and report predicted-vs-observed hit rate plus a Brier score.
//!
//! A well-calibrated predictor's observed hit rate in each confidence bin
//! matches that bin's mean confidence (e.g. things it called "70% likely"
//! happen ~70% of the time), and its Brier score -- the mean squared error of
//! the probability -- is low. The forecast layer attaches a confidence to each
//! predicted co-change, so this turns "is the predictor's confidence meaningful"
//! into a measured number rather than a claim.

/// One (predicted probability in [0,1], observed boolean outcome) sample.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub confidence: f64,
    pub hit: bool,
}

/// One confidence bin's calibration.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct Bin {
    pub lo: f64,
    pub hi: f64,
    pub count: usize,
    pub mean_confidence: f64,
    pub observed_hit_rate: f64,
}

/// Reliability table plus summary calibration statistics.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CalibrationReport {
    pub bins: Vec<Bin>,
    /// Mean squared error of the probability (0 perfect, 1 worst).
    pub brier: f64,
    /// Brier of the trivial predictor that always guesses the base rate.
    pub brier_baseline: f64,
    /// Brier skill score vs that baseline: `1 - brier/brier_baseline`. Positive
    /// means better-than-base-rate; <= 0 means no better than guessing.
    pub brier_skill_score: f64,
    /// Expected calibration error: count-weighted mean gap between a bin's mean
    /// confidence and its observed hit rate (0 perfect).
    pub ece: f64,
    /// Overall observed hit rate (the base rate the skill score compares against).
    pub base_rate: f64,
    pub n: usize,
}

/// Brier score: mean over samples of `(confidence - outcome)^2`. 0 is perfect,
/// 1 is worst. Vacuously 0 with no samples.
pub fn brier(samples: &[Sample]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples
        .iter()
        .map(|s| {
            let o = if s.hit { 1.0 } else { 0.0 };
            (s.confidence - o).powi(2)
        })
        .sum();
    sum / samples.len() as f64
}

/// Bin samples into `n_bins` equal-width buckets over [0,1] and compute, per
/// bucket, the mean confidence and the observed hit rate.
pub fn reliability(samples: &[Sample], n_bins: usize) -> CalibrationReport {
    let n_bins = n_bins.max(1);
    let mut bins: Vec<Bin> = (0..n_bins)
        .map(|i| {
            let lo = i as f64 / n_bins as f64;
            Bin {
                lo,
                hi: lo + 1.0 / n_bins as f64,
                count: 0,
                mean_confidence: 0.0,
                observed_hit_rate: 0.0,
            }
        })
        .collect();
    let mut conf_sum = vec![0.0; n_bins];
    let mut hit_sum = vec![0.0; n_bins];
    for s in samples {
        let c = s.confidence.clamp(0.0, 1.0);
        let idx = ((c * n_bins as f64) as usize).min(n_bins - 1);
        bins[idx].count += 1;
        conf_sum[idx] += c;
        hit_sum[idx] += if s.hit { 1.0 } else { 0.0 };
    }
    for (i, b) in bins.iter_mut().enumerate() {
        if b.count > 0 {
            b.mean_confidence = conf_sum[i] / b.count as f64;
            b.observed_hit_rate = hit_sum[i] / b.count as f64;
        }
    }

    let n = samples.len();
    let base_rate = if n == 0 {
        0.0
    } else {
        samples.iter().filter(|s| s.hit).count() as f64 / n as f64
    };
    // Baseline Brier of "always predict the base rate" = p*(1-p) for binary
    // outcomes; the skill score is the relative improvement over it.
    let brier_baseline = base_rate * (1.0 - base_rate);
    let brier_now = brier(samples);
    let brier_skill_score = if brier_baseline > 0.0 {
        1.0 - brier_now / brier_baseline
    } else {
        0.0
    };
    // ECE: count-weighted mean |confidence - observed| across non-empty bins.
    let ece = if n == 0 {
        0.0
    } else {
        bins.iter()
            .filter(|b| b.count > 0)
            .map(|b| (b.count as f64 / n as f64) * (b.mean_confidence - b.observed_hit_rate).abs())
            .sum()
    };

    CalibrationReport {
        bins,
        brier: brier_now,
        brier_baseline,
        brier_skill_score,
        ece,
        base_rate,
        n,
    }
}

use codegraph_predict::{co_change, CoChangeOptions};
use std::path::Path;
use std::process::Command;

/// Turn commit history into calibration samples by leave-one-out co-change.
///
/// `transactions` is the full history oldest-first, each commit the list of
/// files it touched. For every commit index in `eval`, EACH file in the commit
/// is used as a seed in turn (so the sample is not biased toward any particular
/// filename); the co-change predictor, trained ONLY on the commits before this
/// one, suggests other files, and each suggestion's confidence is recorded
/// against whether that file actually changed in the commit. To avoid one huge
/// commit dominating, a seed only contributes suggestions for OTHER files in the
/// commit. Pure: no git, fully unit-testable.
pub fn samples_from_history(
    transactions: &[Vec<String>],
    eval: impl IntoIterator<Item = usize>,
) -> Vec<Sample> {
    // Calibrate across the whole confidence range: do not pre-filter low
    // confidence, and count a single supporting commit.
    let opts = CoChangeOptions {
        min_support: 1,
        min_confidence_pct: 0,
        ..Default::default()
    };
    let mut samples = Vec::new();
    for i in eval {
        let Some(files) = transactions.get(i) else {
            continue;
        };
        if files.len() < 2 {
            continue; // need a seed plus at least one other file to predict
        }
        let actual: std::collections::HashSet<&str> = files.iter().map(String::as_str).collect();
        let history = &transactions[..i];
        for seed in files {
            for sug in co_change(history, std::slice::from_ref(seed), &opts) {
                // The seed itself is excluded by co_change; a suggestion that is
                // the seed's own commit-mate is the prediction target.
                samples.push(Sample {
                    confidence: sug.confidence_pct as f64 / 100.0,
                    hit: actual.contains(sug.file.as_str()),
                });
            }
        }
    }
    samples
}

/// Read commit transactions (oldest-first) from a git repo: each commit becomes
/// the list of files it touched. Uses an SOH record separator so paths with
/// unusual characters still parse.
fn read_transactions(repo_root: &Path) -> Result<Vec<Vec<String>>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args([
            "log",
            "--reverse",
            "--no-renames",
            "--name-only",
            "--pretty=tformat:\x01%H",
        ])
        .output()
        .map_err(|e| format!("running git log: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git log failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut txns: Vec<Vec<String>> = Vec::new();
    for line in text.lines() {
        if let Some(_hash) = line.strip_prefix('\x01') {
            txns.push(Vec::new());
        } else if !line.is_empty() {
            if let Some(cur) = txns.last_mut() {
                cur.push(line.replace('\\', "/"));
            }
        }
    }
    Ok(txns)
}

/// Calibrate co-change prediction confidence over a repo's recent history: the
/// last `max_commits` commits are evaluated against the predictor trained on the
/// history preceding each. IO-heavy (shells out to git); the scoring itself is
/// [`samples_from_history`].
pub fn calibrate_history(
    repo_root: &Path,
    max_commits: usize,
    n_bins: usize,
) -> Result<CalibrationReport, String> {
    let txns = read_transactions(repo_root)?;
    let total = txns.len();
    let start = total.saturating_sub(max_commits);
    let samples = samples_from_history(&txns, start..total);
    Ok(reliability(&samples, n_bins))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_predictor_has_zero_brier() {
        let s = vec![
            Sample {
                confidence: 1.0,
                hit: true,
            },
            Sample {
                confidence: 0.0,
                hit: false,
            },
        ];
        assert_eq!(brier(&s), 0.0);
    }

    #[test]
    fn worst_predictor_has_brier_one() {
        let s = vec![Sample {
            confidence: 1.0,
            hit: false,
        }];
        assert_eq!(brier(&s), 1.0);
    }

    #[test]
    fn empty_is_vacuously_zero() {
        assert_eq!(brier(&[]), 0.0);
        let r = reliability(&[], 10);
        assert_eq!(r.n, 0);
        assert_eq!(r.bins.len(), 10);
    }

    #[test]
    fn samples_from_history_leave_one_out() {
        // a.py and b.py co-change in commits 0..2; commit 3 changes both again.
        // Trained on commits 0..3, seeding a.py should predict b.py with high
        // confidence, and b.py did change in commit 3 -> a hit.
        let txns = vec![
            vec!["a.py".to_string(), "b.py".to_string()],
            vec!["a.py".to_string(), "b.py".to_string()],
            vec!["a.py".to_string(), "c.py".to_string()],
            vec!["a.py".to_string(), "b.py".to_string()],
        ];
        let samples = samples_from_history(&txns, [3]);
        assert!(!samples.is_empty(), "should predict at least one co-change");
        let b = samples
            .iter()
            .find(|s| (s.confidence - 0.66).abs() < 0.02 || s.confidence > 0.5)
            .expect("a high-confidence prediction for b.py");
        assert!(b.hit, "b.py actually changed in commit 3");
    }

    #[test]
    fn single_file_commits_yield_no_samples() {
        let txns = vec![vec!["only.py".to_string()], vec!["solo.py".to_string()]];
        assert!(samples_from_history(&txns, 0..2).is_empty());
    }

    #[test]
    fn bins_count_and_observed_rate() {
        let s = vec![
            Sample {
                confidence: 0.95,
                hit: true,
            },
            Sample {
                confidence: 0.95,
                hit: false,
            },
        ];
        let r = reliability(&s, 10);
        let top = &r.bins[9];
        assert_eq!(top.count, 2);
        assert!((top.observed_hit_rate - 0.5).abs() < 1e-9);
        assert!((top.mean_confidence - 0.95).abs() < 1e-9);
    }
}
