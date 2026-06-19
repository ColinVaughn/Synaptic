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
        (self.true_positive * 100).checked_div(denom).unwrap_or(100) as u8
    }

    /// Percent of expected items that were found.
    pub fn recall_pct(&self) -> u8 {
        let denom = self.true_positive + self.false_negative;
        (self.true_positive * 100).checked_div(denom).unwrap_or(100) as u8
    }

    pub fn f1_pct(&self) -> u8 {
        let (p, r) = (self.precision_pct() as u32, self.recall_pct() as u32);
        (2 * p * r).checked_div(p + r).unwrap_or(0) as u8
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

/// Which labeled symbols failed to resolve to a node. An empty `unresolved` is a
/// precondition for trustworthy metrics: a label that does not resolve means the
/// extractor dropped a node the human verified exists, which must be a loud
/// failure rather than a silently smaller denominator.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ResolutionReport {
    pub total: usize,
    pub unresolved: Vec<String>,
}

/// Resolve every label the ground truth references and report which ones fail.
pub fn resolution_coverage(gd: &GraphData, gt: &GroundTruth) -> ResolutionReport {
    let labels = gt.all_labels();
    let unresolved: Vec<String> = labels
        .iter()
        .filter(|l| resolve_label(gd, l).is_none())
        .cloned()
        .collect();
    ResolutionReport {
        total: labels.len(),
        unresolved,
    }
}

/// Score extracted `calls` edges against the labeled call-edge set.
///
/// The recall denominator is EVERY labeled call edge, so a labeled endpoint that
/// fails to resolve (a dropped node) counts as a false negative instead of
/// vanishing from the denominator.
pub fn score_call_edges(gd: &GraphData, gt: &GroundTruth) -> PrF1 {
    let expected = resolved_pairs(
        gd,
        gt.call_edges
            .iter()
            .map(|c| (c.from.as_str(), c.to.as_str())),
    );
    let extracted: HashSet<(String, String)> = gd
        .links
        .iter()
        .filter(|e| e.relation == "calls")
        .map(|e| (e.source.0.clone(), e.target.0.clone()))
        .collect();
    let tp = expected.intersection(&extracted).count();
    PrF1 {
        true_positive: tp,
        false_positive: extracted.len() - tp,
        // Unresolved labeled edges are excluded from `expected` (a resolved
        // pair); counting against the full labeled total turns them into FNs.
        false_negative: gt.call_edges.len() - tp,
    }
}

/// The relations that carry a cross-language coupling. A client call connects to
/// a server handler through a path-keyed route node (`calls_service` then
/// `handled_by`); FFI/subprocess couplings use the others. Reachability over
/// this set, not a single direct edge, is what links the two language sides.
const CROSS_LANGUAGE_RELATIONS: &[&str] = &[
    "calls_service",
    "handled_by",
    "invokes",
    "binds_native",
    "calls_native",
];

/// Can `to` be reached from `from` by following cross-language relations forward
/// (bounded depth)? This is the question a cross-language coupling answers: does
/// the client side connect to the server/native side.
fn cross_reachable(
    fwd: &std::collections::HashMap<&str, Vec<&str>>,
    from: &str,
    to: &str,
    depth: usize,
) -> bool {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut frontier = vec![from];
    seen.insert(from);
    for _ in 0..depth {
        let mut next = Vec::new();
        for node in frontier.drain(..) {
            for &t in fwd.get(node).into_iter().flatten() {
                if t == to {
                    return true;
                }
                if seen.insert(t) {
                    next.push(t);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    false
}

/// Score labeled cross-language couplings as a real precision/recall by adding
/// distractor non-couplings.
///
/// Positives (`cross_edge`) MUST connect: each labeled coupling that the graph
/// connects is a true positive; one it fails to connect (or whose endpoint did
/// not resolve) is a false negative -> recall. Negatives (`cross_nonedge`) must
/// NOT connect: a distractor the graph DOES connect (a look-alike route, a
/// method mismatch, a client call with no server) is a false positive ->
/// precision. Without negatives precision is structurally 1.0, which is why a
/// fixture that only labels positives reports recall, not P/R/F1.
pub fn score_cross_edges(gd: &GraphData, gt: &GroundTruth) -> PrF1 {
    if gt.cross_edges.is_empty() && gt.cross_nonedges.is_empty() {
        return PrF1::default();
    }
    let allow: HashSet<&str> = CROSS_LANGUAGE_RELATIONS.iter().copied().collect();
    let mut fwd: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for e in &gd.links {
        if allow.contains(e.relation.as_str()) {
            fwd.entry(e.source.0.as_str())
                .or_default()
                .push(e.target.0.as_str());
        }
    }
    let connected = |from: &str, to: &str| -> bool {
        match (resolve_label(gd, from), resolve_label(gd, to)) {
            (Some(f), Some(t)) => cross_reachable(&fwd, &f.0, &t.0, 6),
            _ => false, // an unresolved endpoint cannot be connected
        }
    };
    let mut pr = PrF1::default();
    for c in &gt.cross_edges {
        if connected(&c.from, &c.to) {
            pr.true_positive += 1;
        } else {
            pr.false_negative += 1;
        }
    }
    for c in &gt.cross_nonedges {
        if connected(&c.from, &c.to) {
            pr.false_positive += 1; // a coupling that should not exist
        }
    }
    pr
}

/// Result of the blast-radius checks across a fixture. Tracks both misses
/// (recall) and noise: `distractors_total` are nodes labeled as definitely NOT
/// affected, `distractors_hit` are those the analysis wrongly reported -- so a
/// blast that returns the whole graph scores 0% false-negatives but leaks every
/// distractor. `predicted_total` / `seed_count` give the average impact-set size
/// to compare against the true set.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct BlastScore {
    pub expected: usize,
    pub found: usize,
    pub missed: usize,
    pub distractors_total: usize,
    pub distractors_hit: usize,
    pub predicted_total: usize,
    pub seed_count: usize,
}

impl BlastScore {
    /// Percent of truly-affected nodes the analysis MISSED. Lower is better;
    /// vacuously 0 when nothing was expected.
    pub fn false_negative_pct(&self) -> u8 {
        (self.missed * 100).checked_div(self.expected).unwrap_or(0) as u8
    }

    /// Recall of truly-affected nodes (vacuously 100 when nothing expected).
    pub fn recall_pct(&self) -> u8 {
        (self.found * 100).checked_div(self.expected).unwrap_or(100) as u8
    }

    /// Precision against distractors: percent of labeled not-affected nodes the
    /// analysis correctly EXCLUDED. 100 means no distractor leaked into the blast
    /// radius; vacuously 100 when no distractors were labeled.
    pub fn distractor_exclusion_pct(&self) -> u8 {
        let kept_out = self.distractors_total - self.distractors_hit;
        (kept_out * 100)
            .checked_div(self.distractors_total)
            .unwrap_or(100) as u8
    }

    /// Average reported impact-set size per seed (the noise dimension; compare to
    /// the true affected-set size).
    pub fn avg_predicted_size(&self) -> f64 {
        if self.seed_count == 0 {
            0.0
        } else {
            self.predicted_total as f64 / self.seed_count as f64
        }
    }
}

/// For each labeled blast seed, run reverse-impact and measure both misses
/// (labeled affected nodes not reached) and noise (labeled not-affected
/// distractors that WERE reached, plus the reported set size). Depth is generous
/// so reachability, not hop count, is what is tested.
pub fn score_blast_radius(gd: &GraphData, gt: &GroundTruth) -> BlastScore {
    let kg = KnowledgeGraph::from_graph_data(gd.clone());
    let mut score = BlastScore::default();
    for b in &gt.blasts {
        let Some(seed) = resolve_label(gd, &b.seed) else {
            // An unresolved seed is a dropped node: every node it should have
            // reached is a miss, and distractors cannot be excluded by a walk
            // that never ran. Preflight fails the run before this matters, but
            // score conservatively regardless.
            score.expected += b.affects.len();
            score.missed += b.affects.len();
            score.distractors_total += b.not_affected.len();
            score.seed_count += 1;
            continue;
        };
        let reached: HashSet<String> = affected_nodes(&kg, &seed, DEFAULT_AFFECTED_RELATIONS, 64)
            .into_iter()
            .map(|h| h.node_id.0)
            .collect();
        score.seed_count += 1;
        score.predicted_total += reached.len();
        for label in &b.affects {
            score.expected += 1;
            match resolve_label(gd, label) {
                Some(NodeId(id)) if reached.contains(&id) => score.found += 1,
                _ => score.missed += 1,
            }
        }
        for label in &b.not_affected {
            score.distractors_total += 1;
            if let Some(NodeId(id)) = resolve_label(gd, label) {
                if reached.contains(&id) {
                    score.distractors_hit += 1;
                }
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
    // Cache the reverse-impact set per covered symbol (a label may appear in both
    // positive and negative linkages).
    let reached_from = |covered: &str| -> HashSet<String> {
        match resolve_label(gd, covered) {
            Some(seed) => affected_nodes(&kg, &seed, DEFAULT_AFFECTED_RELATIONS, 64)
                .into_iter()
                .map(|h| h.node_id.0)
                .collect(),
            None => HashSet::new(),
        }
    };
    let mut pr = PrF1::default();
    // Positives: each labeled (test, covered) MUST be selected; unresolved or
    // unreached counts as a miss (false negative) -> recall.
    for tl in &gt.test_links {
        let test_id = resolve_label(gd, &tl.test);
        for covered in &tl.covers {
            let reached = reached_from(covered);
            match &test_id {
                Some(t) if reached.contains(&t.0) => pr.true_positive += 1,
                _ => pr.false_negative += 1,
            }
        }
    }
    // Negatives: each labeled (test, covered) must NOT be selected; a selected
    // non-link is a false positive -> precision.
    for tl in &gt.test_nonlinks {
        let test_id = resolve_label(gd, &tl.test);
        for covered in &tl.covers {
            let reached = reached_from(covered);
            if let Some(t) = &test_id {
                if reached.contains(&t.0) {
                    pr.false_positive += 1;
                }
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
    pub resolution: ResolutionReport,
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
    fn pooled<F: Fn(&FixtureReport) -> &PrF1>(&self, pick: F) -> PrF1 {
        self.fixtures.iter().fold(PrF1::default(), |mut acc, f| {
            let p = pick(f);
            acc.true_positive += p.true_positive;
            acc.false_positive += p.false_positive;
            acc.false_negative += p.false_negative;
            acc
        })
    }

    /// Pooled call-edge P/R/F1 across all fixtures (counts summed, then ratio'd).
    pub fn pooled_call_edges(&self) -> PrF1 {
        self.pooled(|f| &f.call_edges)
    }

    /// Pooled cross-language P/R/F1 across all fixtures.
    pub fn pooled_cross_edges(&self) -> PrF1 {
        self.pooled(|f| &f.cross_edges)
    }

    /// Pooled affected-test recall across all fixtures.
    pub fn pooled_affected_tests(&self) -> PrF1 {
        self.pooled(|f| &f.affected_tests)
    }

    /// Every (fixture, label) that failed to resolve. An empty result is the
    /// precondition for trusting any other number in the report.
    pub fn unresolved(&self) -> Vec<(String, String)> {
        self.fixtures
            .iter()
            .flat_map(|f| {
                f.resolution
                    .unresolved
                    .iter()
                    .map(move |l| (f.dir.clone(), l.clone()))
            })
            .collect()
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
        resolution: resolution_coverage(&gd, &gt),
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
        assert_eq!(
            pr.recall_pct(),
            50,
            "cross-file call is a known miss: {pr:?}"
        );
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
    }

    /// The preflight: EVERY labeled symbol across the whole corpus must resolve
    /// to a real node. A failure here means the extractor dropped a node the
    /// ground truth references, which would otherwise silently shrink a metric's
    /// denominator instead of scoring as a miss.
    #[test]
    fn every_label_resolves() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus");
        let report = run_corpus(&root).unwrap();
        let unresolved = report.unresolved();
        assert!(
            unresolved.is_empty(),
            "unresolved labels (extractor dropped nodes the corpus references): {unresolved:?}"
        );
        // And the coverage count is non-trivial, so the preflight is actually
        // exercising labels rather than passing vacuously.
        let total: usize = report.fixtures.iter().map(|f| f.resolution.total).sum();
        assert!(
            total >= 12,
            "expected a meaningful label count, got {total}"
        );
    }

    /// Per-fixture baselines measured 2026-06-19. These lock in current
    /// extraction quality so a regression fails CI; if extraction IMPROVES
    /// (e.g. Rust resolves cross-file calls), update the affected baseline
    /// upward deliberately. `(call precision, call recall)`.
    #[test]
    fn per_fixture_baselines_hold() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus");
        let report = run_corpus(&root).unwrap();
        let expect = |dir: &str| -> (u8, u8) {
            match dir {
                "systems-rust" => (100, 50),
                "scripting-python" => (100, 100),
                "web-ts" => (100, 100),
                "oo-java" => (100, 100),
                "systems-go" => (100, 100),
                "deep-python" => (100, 100),
                // The cross-lang fixture labels only cross-language couplings, so
                // it has no call edges (vacuous 100/100); its real assertion is
                // the cross-edge recall check below.
                "cross-lang-ts-rust" => (100, 100),
                other => panic!("no baseline recorded for fixture {other}"),
            }
        };
        for f in &report.fixtures {
            let (p, r) = expect(&f.dir);
            assert_eq!(
                (f.call_edges.precision_pct(), f.call_edges.recall_pct()),
                (p, r),
                "{} call-edge baseline drifted: {:?}",
                f.dir,
                f.call_edges
            );
            // No labeled affected node is ever missed, and no labeled distractor
            // ever leaks into the blast radius.
            assert_eq!(
                f.blast.false_negative_pct(),
                0,
                "{} blast radius regressed (a true affected node was missed): {:?}",
                f.dir,
                f.blast
            );
            assert_eq!(
                f.blast.distractor_exclusion_pct(),
                100,
                "{} blast radius leaked a distractor: {:?}",
                f.dir,
                f.blast
            );
        }
        // Pooled precision stays perfect; pooled recall stays at/above today's value.
        let pooled = report.pooled_call_edges();
        assert_eq!(
            pooled.precision_pct(),
            100,
            "pooled call precision regressed"
        );
        assert!(
            pooled.recall_pct() >= 88,
            "pooled call recall regressed: {pooled:?}"
        );

        // The TS->Rust HTTP coupling is connected end to end, and the labeled
        // distractors (look-alike path, wrong handler) are NOT coupled -- so the
        // cross-language precision is earned, not structural.
        let xlang = report
            .fixtures
            .iter()
            .find(|f| f.dir == "cross-lang-ts-rust")
            .expect("cross-lang fixture scored");
        assert_eq!(
            xlang.cross_edges.recall_pct(),
            100,
            "cross-language coupling not connected: {:?}",
            xlang.cross_edges
        );
        assert_eq!(
            xlang.cross_edges.false_positive, 0,
            "a distractor coupling was wrongly connected: {:?}",
            xlang.cross_edges
        );
        assert_eq!(
            xlang.cross_edges.precision_pct(),
            100,
            "cross-language precision regressed: {:?}",
            xlang.cross_edges
        );

        // Multi-hop affected-test selection: the 3-layer fixture selects the
        // related test (recall) and excludes the unrelated one (precision).
        let deep = report
            .fixtures
            .iter()
            .find(|f| f.dir == "deep-python")
            .expect("deep-python scored");
        assert_eq!(
            deep.affected_tests.recall_pct(),
            100,
            "multi-hop test selection missed the related test: {:?}",
            deep.affected_tests
        );
        assert_eq!(
            deep.affected_tests.false_positive, 0,
            "multi-hop test selection picked an unrelated test: {:?}",
            deep.affected_tests
        );

        // Whole-corpus preflight: every labeled symbol resolves.
        assert!(
            report.unresolved().is_empty(),
            "unresolved labels: {:?}",
            report.unresolved()
        );
    }
}
