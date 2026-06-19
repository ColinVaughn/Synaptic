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

/// Reliability table plus the overall Brier score.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CalibrationReport {
    pub bins: Vec<Bin>,
    pub brier: f64,
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
    CalibrationReport {
        bins,
        brier: brier(samples),
        n: samples.len(),
    }
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
