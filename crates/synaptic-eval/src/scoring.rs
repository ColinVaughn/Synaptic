//! Set-based precision / recall / F1 scoring. Kept pure and count-based so that
//! aggregation across many commits is exact (sum the raw counts, then derive the
//! percentages once) rather than averaging per-commit percentages.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Raw comparison counts for one prediction-vs-truth pair, plus derived rates.
/// Percentages are computed on demand from the counts so a vector of `Scores`
/// can be aggregated by summing counts without compounding rounding error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scores {
    /// How many items the predictor flagged.
    pub predicted: usize,
    /// How many items were actually relevant (ground truth).
    pub relevant: usize,
    /// Flagged items that were relevant (true positives).
    pub hits: usize,
}

/// A percentage `num/den` rounded to the nearest integer and clamped to 100.
/// Returns 0 when `den == 0`. Computed in u64 so a long range of large graphs
/// cannot overflow the intermediate `num * 200` on a 32-bit `usize`.
pub(crate) fn pct(num: usize, den: usize) -> u8 {
    if den == 0 {
        return 0;
    }
    let (num, den) = (num as u64, den as u64);
    (((num * 200 + den) / (den * 2)).min(100)) as u8
}

impl Scores {
    /// Of the items flagged, the fraction that were relevant. 0 when nothing was
    /// flagged.
    pub fn precision_pct(&self) -> u8 {
        pct(self.hits, self.predicted)
    }

    /// Of the relevant items, the fraction that were flagged. 0 when there were
    /// no relevant items.
    pub fn recall_pct(&self) -> u8 {
        pct(self.hits, self.relevant)
    }

    /// Harmonic mean of precision and recall. Derived directly from counts:
    /// `2*hits / (predicted + relevant)` equals `2PR/(P+R)` exactly.
    pub fn f1_pct(&self) -> u8 {
        pct(2 * self.hits, self.predicted + self.relevant)
    }

    /// True when there is nothing to score (no prediction and no truth).
    pub fn is_empty(&self) -> bool {
        self.predicted == 0 && self.relevant == 0
    }
}

/// Score a predicted set against a ground-truth set.
pub fn score_sets(predicted: &BTreeSet<String>, relevant: &BTreeSet<String>) -> Scores {
    let hits = predicted.iter().filter(|p| relevant.contains(*p)).count();
    Scores {
        predicted: predicted.len(),
        relevant: relevant.len(),
        hits,
    }
}

/// Sum the raw counts across many `Scores` (exact aggregation).
pub fn aggregate(scores: &[Scores]) -> Scores {
    let mut agg = Scores::default();
    for s in scores {
        agg.predicted += s.predicted;
        agg.relevant += s.relevant;
        agg.hits += s.hits;
    }
    agg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn perfect_prediction_scores_full() {
        let s = score_sets(&set(&["a", "b"]), &set(&["a", "b"]));
        assert_eq!(s.precision_pct(), 100);
        assert_eq!(s.recall_pct(), 100);
        assert_eq!(s.f1_pct(), 100);
    }

    #[test]
    fn partial_overlap_scores_proportionally() {
        // pred {a,b,c} vs truth {a,b,d}: 2 hits, 3 predicted, 3 relevant.
        let s = score_sets(&set(&["a", "b", "c"]), &set(&["a", "b", "d"]));
        assert_eq!(s.hits, 2);
        assert_eq!(s.precision_pct(), 67, "2/3");
        assert_eq!(s.recall_pct(), 67, "2/3");
        assert_eq!(s.f1_pct(), 67);
    }

    #[test]
    fn missing_everything_is_zero_recall() {
        let s = score_sets(&set(&[]), &set(&["a", "b"]));
        assert_eq!(s.hits, 0);
        assert_eq!(s.recall_pct(), 0);
        assert_eq!(s.precision_pct(), 0, "nothing flagged");
    }

    #[test]
    fn flagging_with_no_truth_is_empty_safe() {
        let s = score_sets(&set(&["a"]), &set(&[]));
        assert_eq!(s.precision_pct(), 0);
        assert_eq!(s.recall_pct(), 0);
        assert!(!s.is_empty(), "something was predicted");
        assert!(score_sets(&set(&[]), &set(&[])).is_empty());
    }

    #[test]
    fn aggregate_sums_counts_not_percentages() {
        // Commit 1: 1/1 recall. Commit 2: 0/3 recall. Averaging percentages would
        // give 50%; the correct pooled recall is 1/4 = 25%.
        let a = score_sets(&set(&["x"]), &set(&["x"]));
        let b = score_sets(&set(&[]), &set(&["p", "q", "r"]));
        let agg = aggregate(&[a, b]);
        assert_eq!(agg.hits, 1);
        assert_eq!(agg.relevant, 4);
        assert_eq!(agg.recall_pct(), 25, "pooled, not averaged");
    }

    #[test]
    fn rounding_is_nearest() {
        // 2/3 = 66.67 -> 67.
        assert_eq!(pct(2, 3), 67);
        // 1/3 = 33.33 -> 33.
        assert_eq!(pct(1, 3), 33);
        // 1/2 = 50.
        assert_eq!(pct(1, 2), 50);
    }
}
