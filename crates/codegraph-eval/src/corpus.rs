//! Score CodeGraph's extracted graph against the hand-labeled corpus.
//!
//! Every metric is exact set-comparison against human-verified labels in each
//! fixture's `ground_truth.toml`. The oracle includes relationships the
//! extractor is NOT designed to resolve (e.g. cross-file calls), so the numbers
//! reflect the real graph rather than a self-fulfilling subset.

use std::collections::HashSet;
use std::path::Path;

use codegraph_core::{GraphData, NodeId};
use codegraph_graph::KnowledgeGraph;
use codegraph_incremental::{rebuild, ChangeSet, RebuildOptions};
use codegraph_query::{affected_nodes, DEFAULT_AFFECTED_RELATIONS};

use crate::groundtruth::{resolve_label, GroundTruth, Manifest};

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

/// Result of the blast-radius checks across a fixture: how many labeled
/// affected nodes the reverse-impact analysis missed (the false-negative rate).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct BlastScore {
    pub expected: usize,
    pub found: usize,
    pub missed: usize,
}

impl BlastScore {
    /// Percent of truly-affected nodes the analysis MISSED. Lower is better;
    /// vacuously 0 when nothing was expected.
    pub fn false_negative_pct(&self) -> u8 {
        if self.expected == 0 {
            0
        } else {
            ((self.missed * 100) / self.expected) as u8
        }
    }
}

/// For each labeled blast seed, run reverse-impact and measure how many labeled
/// affected nodes were missed. Depth is generous so reachability, not hop count,
/// is what is tested.
pub fn score_blast_radius(gd: &GraphData, gt: &GroundTruth) -> BlastScore {
    let kg = KnowledgeGraph::from_graph_data(gd.clone());
    let mut score = BlastScore::default();
    for b in &gt.blasts {
        let Some(seed) = resolve_label(gd, &b.seed) else {
            continue;
        };
        let reached: HashSet<String> = affected_nodes(&kg, &seed, DEFAULT_AFFECTED_RELATIONS, 64)
            .into_iter()
            .map(|h| h.node_id.0)
            .collect();
        for label in &b.affects {
            score.expected += 1;
            match resolve_label(gd, label) {
                Some(NodeId(id)) if reached.contains(&id) => score.found += 1,
                _ => score.missed += 1,
            }
        }
    }
    score
}

/// Recall of test->code linkage: of the tests labeled as covering a changed
/// symbol, how many does CodeGraph's reverse-impact surface from that symbol.
/// Reported as a PrF1 (true_positive = surfaced, false_negative = missed); there
/// is no false-positive notion here since we only ask whether each labeled test
/// is reachable from the code it covers.
pub fn score_affected_tests(gd: &GraphData, gt: &GroundTruth) -> PrF1 {
    let kg = KnowledgeGraph::from_graph_data(gd.clone());
    let mut pr = PrF1::default();
    for tl in &gt.test_links {
        let Some(test_id) = resolve_label(gd, &tl.test) else {
            continue;
        };
        for covered in &tl.covers {
            let Some(seed) = resolve_label(gd, covered) else {
                continue;
            };
            let reached: HashSet<String> = affected_nodes(&kg, &seed, DEFAULT_AFFECTED_RELATIONS, 64)
                .into_iter()
                .map(|h| h.node_id.0)
                .collect();
            if reached.contains(&test_id.0) {
                pr.true_positive += 1;
            } else {
                pr.false_negative += 1;
            }
        }
    }
    pr
}

/// Scores for one fixture across every metric.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FixtureReport {
    pub dir: String,
    pub family: String,
    pub call_edges: PrF1,
    pub affected_tests: PrF1,
    pub blast: BlastScore,
    pub cross_edges: PrF1,
}

/// Whole-corpus report: one entry per fixture in the manifest.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CorpusReport {
    pub fixtures: Vec<FixtureReport>,
}

impl CorpusReport {
    /// Pooled call-edge P/R/F1 across all fixtures (counts summed, then ratio'd).
    pub fn pooled_call_edges(&self) -> PrF1 {
        self.fixtures
            .iter()
            .fold(PrF1::default(), |mut acc, f| {
                acc.true_positive += f.call_edges.true_positive;
                acc.false_positive += f.call_edges.false_positive;
                acc.false_negative += f.call_edges.false_negative;
                acc
            })
    }
}

/// Score one fixture directory given its labels.
pub fn score_fixture(dir: &Path, name: &str, family: &str) -> Result<FixtureReport, String> {
    let gd = build_fixture(dir)?;
    let gt = GroundTruth::parse(
        &std::fs::read_to_string(dir.join("ground_truth.toml")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(FixtureReport {
        dir: name.to_string(),
        family: family.to_string(),
        call_edges: score_call_edges(&gd, &gt),
        affected_tests: score_affected_tests(&gd, &gt),
        blast: score_blast_radius(&gd, &gt),
        cross_edges: score_cross_edges(&gd, &gt),
    })
}

/// Score every fixture listed in `<corpus_root>/manifest.toml`.
pub fn run_corpus(corpus_root: &Path) -> Result<CorpusReport, String> {
    let manifest = Manifest::parse(
        &std::fs::read_to_string(corpus_root.join("manifest.toml")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let mut fixtures = Vec::new();
    for f in &manifest.fixtures {
        fixtures.push(score_fixture(&corpus_root.join(&f.dir), &f.dir, &f.family)?);
    }
    Ok(CorpusReport { fixtures })
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

    #[test]
    fn systems_rust_blast_radius_no_false_negatives() {
        let (gd, gt) = fixture("systems-rust");
        let score = score_blast_radius(&gd, &gt);
        // The labeled caller is reachable from the seed via the intra-file call
        // edge, so reverse-impact misses nothing here.
        assert!(score.expected > 0, "blast labels must resolve: {score:?}");
        assert_eq!(
            score.false_negative_pct(),
            0,
            "labeled caller must be in the blast radius: {score:?}"
        );
    }

    #[test]
    fn runs_full_corpus_from_manifest() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus");
        let report = run_corpus(&root).unwrap();
        assert!(!report.fixtures.is_empty(), "manifest lists fixtures");
        let rust = report
            .fixtures
            .iter()
            .find(|f| f.dir == "systems-rust")
            .expect("systems-rust scored");
        assert_eq!(rust.call_edges.true_positive, 1);
        assert_eq!(report.pooled_call_edges().true_positive, 1);
    }
}
