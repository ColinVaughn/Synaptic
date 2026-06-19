//! Score CodeGraph's extracted graph against the hand-labeled corpus.
//!
//! Every metric is exact set-comparison against human-verified labels in each
//! fixture's `ground_truth.toml`. The oracle includes relationships the
//! extractor is NOT designed to resolve (e.g. cross-file calls), so the numbers
//! reflect the real graph rather than a self-fulfilling subset.

use std::collections::HashSet;
use std::path::Path;

use codegraph_core::GraphData;
use codegraph_incremental::{rebuild, ChangeSet, RebuildOptions};

use crate::groundtruth::{resolve_label, GroundTruth};

/// Precision / recall / F1 from set-comparison counts.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct PrF1 {
    pub true_positive: usize,
    pub false_positive: usize,
    pub false_negative: usize,
}

impl PrF1 {
    /// Percent of extracted items that are correct. Vacuously 100 when nothing
    /// was extracted and nothing was expected.
    pub fn precision_pct(&self) -> u8 {
        let denom = self.true_positive + self.false_positive;
        if denom == 0 {
            100
        } else {
            ((self.true_positive * 100) / denom) as u8
        }
    }

    /// Percent of expected items that were found.
    pub fn recall_pct(&self) -> u8 {
        let denom = self.true_positive + self.false_negative;
        if denom == 0 {
            100
        } else {
            ((self.true_positive * 100) / denom) as u8
        }
    }

    pub fn f1_pct(&self) -> u8 {
        let (p, r) = (self.precision_pct() as u32, self.recall_pct() as u32);
        if p + r == 0 {
            0
        } else {
            ((2 * p * r) / (p + r)) as u8
        }
    }
}

/// Build a fixture directory into a GraphData. Deterministic, no git: the same
/// full rebuild the incremental engine runs for a fresh tree.
pub fn build_fixture(dir: &Path) -> Result<GraphData, String> {
    let out = rebuild(
        &RebuildOptions {
            root: dir.to_path_buf(),
            directed: true,
            force: true,
        },
        &ChangeSet::Full,
        None,
    )
    .map_err(|e| e.to_string())?;
    Ok(out.kg.to_graph_data())
}

/// The (from_id, to_id) pairs of a labeled edge set that resolve to real nodes.
fn resolved_pairs<'a>(
    gd: &GraphData,
    edges: impl Iterator<Item = (&'a str, &'a str)>,
) -> HashSet<(String, String)> {
    let mut set = HashSet::new();
    for (from, to) in edges {
        if let (Some(f), Some(t)) = (resolve_label(gd, from), resolve_label(gd, to)) {
            set.insert((f.0, t.0));
        }
    }
    set
}

/// Score extracted `calls` edges against the labeled call-edge set.
pub fn score_call_edges(gd: &GraphData, gt: &GroundTruth) -> PrF1 {
    let expected = resolved_pairs(
        gd,
        gt.call_edges.iter().map(|c| (c.from.as_str(), c.to.as_str())),
    );
    let extracted: HashSet<(String, String)> = gd
        .links
        .iter()
        .filter(|e| e.relation == "calls")
        .map(|e| (e.source.0.clone(), e.target.0.clone()))
        .collect();
    score_sets(&expected, &extracted)
}

/// Score extracted cross-language edges against the labeled cross-edge set.
/// Cross-language edges use several relation names, so a labeled (from,to) pair
/// counts as found regardless of the relation string.
pub fn score_cross_edges(gd: &GraphData, gt: &GroundTruth) -> PrF1 {
    let expected = resolved_pairs(
        gd,
        gt.cross_edges.iter().map(|c| (c.from.as_str(), c.to.as_str())),
    );
    if expected.is_empty() {
        return PrF1::default();
    }
    // Restrict the extracted set to edges between the labeled endpoints so an
    // unrelated `contains` edge is never counted as a cross-language hit; the
    // metric here is recall of the labeled cross-language couplings.
    let endpoints: HashSet<&String> = expected.iter().flat_map(|(a, b)| [a, b]).collect();
    let extracted: HashSet<(String, String)> = gd
        .links
        .iter()
        .filter(|e| endpoints.contains(&e.source.0) && endpoints.contains(&e.target.0))
        .map(|e| (e.source.0.clone(), e.target.0.clone()))
        .collect();
    let found = expected.intersection(&extracted).count();
    PrF1 {
        true_positive: found,
        false_positive: 0,
        false_negative: expected.len() - found,
    }
}

/// Generic precision/recall over an expected and an extracted set.
fn score_sets(expected: &HashSet<(String, String)>, extracted: &HashSet<(String, String)>) -> PrF1 {
    let tp = expected.intersection(extracted).count();
    PrF1 {
        true_positive: tp,
        false_positive: extracted.len() - tp,
        false_negative: expected.len() - tp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::groundtruth::GroundTruth;
    use std::path::PathBuf;

    fn fixture(name: &str) -> (GraphData, GroundTruth) {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("corpus")
            .join(name);
        let gd = build_fixture(&dir).unwrap();
        let gt =
            GroundTruth::parse(&std::fs::read_to_string(dir.join("ground_truth.toml")).unwrap())
                .unwrap();
        (gd, gt)
    }

    #[test]
    fn systems_rust_call_edges() {
        let (gd, gt) = fixture("systems-rust");
        let pr = score_call_edges(&gd, &gt);
        // Baseline measured 2026-06-19. The intra-file call resolves (TP); the
        // cross-file module-qualified call is a known false negative. Precision
        // is full (no spurious calls). Update intentionally if extraction
        // improves cross-file call resolution.
        assert_eq!(pr.true_positive, 1, "intra-file call must be found: {pr:?}");
        assert_eq!(pr.recall_pct(), 50, "cross-file call is a known miss: {pr:?}");
        assert_eq!(pr.precision_pct(), 100, "no spurious call edges: {pr:?}");
    }
}
