//! Replay calibration harness. Walk a range of history; at each commit re-predict
//! the change from the PARENT-state graph and score the prediction against git
//! ground truth. Ground truth is a deterministic proxy (not CI-log or sandbox
//! outcomes): the tests CO-EDITED in the commit that existed at the parent (a
//! co-edited test stands in for a relevant one; co-edited != failed), and the
//! public APIs the time-travel diff reports as removed. Reports pooled recall,
//! precision, and selectivity so forecast quality can be regression-tested.
//!
//! The pure scoring core ([`score_commit`]) takes an already-built parent graph
//! and the ground-truth sets, so it is deterministic and unit-tested without any
//! extraction. The IO orchestration ([`replay`]) wires git + the time-travel
//! build/diff to that core.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use codegraph_core::is_test_path;
use codegraph_graph::KnowledgeGraph;
use codegraph_history::{build, diff, git, DiffOptions};
use codegraph_predict::{forecast_changes, ForecastOptions};

use crate::scoring::{aggregate, score_sets, Scores};
use crate::EvalError;

/// On-disk schema version for a replay report.
pub const REPLAY_VERSION: u32 = 1;

/// One commit's evaluation: how the forecast (from the parent graph) scored
/// against what the commit actually did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitEval {
    pub commit: String,
    pub parent: String,
    pub changed_files: Vec<String>,
    /// Nodes in the parent-state graph (denominator for selectivity).
    pub graph_nodes: usize,
    /// Dependents the forecast flagged (numerator for selectivity).
    pub blast_total: usize,
    /// Predicted at-risk tests vs the tests CO-EDITED in the commit that already
    /// existed at the parent. A deterministic proxy: a co-edited test is a
    /// reasonable stand-in for a relevant test, but co-edited != failed, and tests
    /// added in the same commit are excluded (they cannot be predicted from the
    /// parent graph).
    pub test: Scores,
    /// Predicted public-API risks vs the APIs the time-travel diff reports as
    /// removed. Has signal only on languages whose extractor records visibility
    /// (the forecast flags `public` symbols); on others the diff's export-surface
    /// heuristic can report removals the forecast cannot match, so treat this as a
    /// lower bound. Not used by the CI gate.
    pub api: Scores,
}

/// The aggregate replay result over a range of commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayReport {
    pub version: u32,
    pub commits: Vec<CommitEval>,
    /// Pooled predictive-test-selection scores.
    pub test: Scores,
    /// Pooled removed-API scores.
    pub api: Scores,
    /// Pooled blast-radius-as-fraction-of-graph (lower = more selective).
    pub selectivity_pct: u8,
    pub summary: String,
}

impl ReplayReport {
    /// The CI gate: did co-edited test recall reach at least `min_pct`? Vacuously
    /// true when nothing was scored (no pre-existing test was co-edited anywhere
    /// in the range). Caveat: because scoring is pooled over the whole range, a
    /// range that happens to co-edit no tests passes regardless of how the
    /// predictor would do on a risky change in it; gate over a range with real
    /// test activity for the check to carry signal.
    pub fn meets_test_recall(&self, min_pct: u8) -> bool {
        self.test.relevant == 0 || self.test.recall_pct() >= min_pct
    }
}

/// Options controlling a replay.
#[derive(Debug, Clone)]
pub struct ReplayOptions {
    /// Build directed graphs for each revision.
    pub directed: bool,
    /// Reverse-impact hop bound for the forecast.
    pub depth: usize,
    /// Cap on the number of commits replayed (newest-first selection, then
    /// evaluated oldest-first).
    pub max_commits: usize,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        ReplayOptions {
            directed: false,
            depth: 3,
            max_commits: 50,
        }
    }
}

/// Score one commit's prediction against ground truth, given the parent-state
/// graph. Pure: no git, no extraction. `nontest_changed` are the changed source
/// files the forecast predicts from; `edited_tests` and `removed_apis` are the
/// measured ground truth.
pub fn score_commit(
    commit: &str,
    parent: &str,
    graph_at_parent: &KnowledgeGraph,
    nontest_changed: &[String],
    edited_tests: &BTreeSet<String>,
    removed_apis: &BTreeSet<String>,
    depth: usize,
) -> CommitEval {
    let opts = ForecastOptions {
        depth: depth.clamp(1, 16),
        ..Default::default()
    };
    let forecast = forecast_changes(graph_at_parent, nontest_changed, &opts);

    let predicted_tests: BTreeSet<String> = forecast
        .at_risk_tests
        .iter()
        .map(|h| norm(&h.file))
        .collect();
    let predicted_apis: BTreeSet<String> = forecast
        .public_api_breaks
        .iter()
        .map(|n| api_key(&n.label, &n.file))
        .collect();

    // Only tests that already exist at the parent can be predicted from the
    // parent-state graph; a test ADDED in this commit is absent there, so scoring
    // it as a miss would be unfair. Restrict the recall denominator to the tests
    // that were predictable in principle.
    let parent_files: BTreeSet<String> = graph_at_parent
        .nodes()
        .map(|n| norm(&n.source_file))
        .collect();
    let predictable_tests: BTreeSet<String> = edited_tests
        .iter()
        .map(|t| norm(t))
        .filter(|t| parent_files.contains(t))
        .collect();

    let mut changed_files: Vec<String> = nontest_changed.to_vec();
    changed_files.extend(edited_tests.iter().cloned());
    changed_files.sort();
    changed_files.dedup();

    CommitEval {
        commit: commit.to_string(),
        parent: parent.to_string(),
        changed_files,
        graph_nodes: graph_at_parent.node_count(),
        blast_total: forecast.blast_radius_total,
        test: score_sets(&predicted_tests, &predictable_tests),
        api: score_sets(&predicted_apis, removed_apis),
    }
}

/// Repo-relative path with separators normalized, so a git path (always `/`)
/// matches a node `source_file` regardless of OS.
fn norm(p: &str) -> String {
    p.replace('\\', "/")
}

/// Key a public API by label AND file, so two distinct symbols sharing a name in
/// different files do not collide when scoring removed-API predictions.
fn api_key(label: &str, file: &str) -> String {
    format!("{label}\u{1f}{}", norm(file))
}

/// Assemble a report from per-commit evaluations (pooled aggregation).
fn assemble(commits: Vec<CommitEval>) -> ReplayReport {
    let test = aggregate(&commits.iter().map(|c| c.test.clone()).collect::<Vec<_>>());
    let api = aggregate(&commits.iter().map(|c| c.api.clone()).collect::<Vec<_>>());
    let total_blast: usize = commits.iter().map(|c| c.blast_total).sum();
    let total_nodes: usize = commits.iter().map(|c| c.graph_nodes).sum();
    let selectivity_pct = crate::scoring::pct(total_blast, total_nodes);
    let summary = format!(
        "{} commit(s); co-edited test recall {}% / precision {}%; removed-API recall {}% (lower bound); blast radius {}% of graph (pooled)",
        commits.len(),
        test.recall_pct(),
        test.precision_pct(),
        api.recall_pct(),
        selectivity_pct
    );
    ReplayReport {
        version: REPLAY_VERSION,
        commits,
        test,
        api,
        selectivity_pct,
        summary,
    }
}

/// Replay `from..HEAD`: for each non-merge commit, predict from its parent and
/// score against ground truth. IO-heavy (builds a graph per revision in a
/// worktree, cached per SHA); the per-commit scoring is delegated to the pure
/// [`score_commit`].
pub fn replay(
    repo_root: &Path,
    from: &str,
    opts: &ReplayOptions,
) -> Result<ReplayReport, EvalError> {
    let root = repo_root
        .canonicalize()
        .map_err(|e| EvalError::Git(format!("resolving {}: {e}", repo_root.display())))?;
    let commits = rev_list(&root, from)?;
    let take = commits.len().min(opts.max_commits);

    let mut evals = Vec::new();
    for commit in &commits[..take] {
        let parent = match git::rev_parse(&root, &format!("{commit}~1")) {
            Ok(p) => p,
            Err(_) => continue, // root commit: no parent to predict from
        };
        let ns = git::numstat(&root, &parent, Some(commit))
            .map_err(|e| EvalError::History(e.to_string()))?;
        let (nontest, tests) = split_changed(&ns);
        if nontest.is_empty() {
            continue; // no source change to forecast from
        }
        let graph = build::build_at_rev(&root, &parent, opts.directed, true)
            .map_err(|e| EvalError::History(e.to_string()))?;
        let removed = removed_apis(&root, &parent, commit, opts.directed)?;
        evals.push(score_commit(
            commit, &parent, &graph, &nontest, &tests, &removed, opts.depth,
        ));
    }
    Ok(assemble(evals))
}

/// Commit SHAs in `from..HEAD`, oldest first, excluding merges.
fn rev_list(root: &Path, from: &str) -> Result<Vec<String>, EvalError> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "rev-list",
            "--reverse",
            "--no-merges",
            &format!("{from}..HEAD"),
        ])
        .output()
        .map_err(|e| EvalError::Git(format!("spawning git rev-list: {e}")))?;
    if !out.status.success() {
        return Err(EvalError::Git(format!(
            "git rev-list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Split a numstat into (changed non-test files, edited test files).
fn split_changed(ns: &[(usize, usize, String)]) -> (Vec<String>, BTreeSet<String>) {
    let mut nontest = Vec::new();
    let mut tests = BTreeSet::new();
    for (_, _, path) in ns {
        if is_test_path(path) {
            tests.insert(path.clone());
        } else {
            nontest.push(path.clone());
        }
    }
    (nontest, tests)
}

/// Public-API labels the time-travel diff reports as removed between parent and
/// commit (the ground truth for the forecast's public-API-at-risk prediction).
fn removed_apis(
    root: &Path,
    parent: &str,
    commit: &str,
    directed: bool,
) -> Result<BTreeSet<String>, EvalError> {
    let opts = DiffOptions {
        directed,
        ..Default::default()
    };
    let report =
        diff(root, parent, Some(commit), &opts).map_err(|e| EvalError::History(e.to_string()))?;
    Ok(report
        .removed_apis
        .into_iter()
        .map(|a| api_key(&a.label, &a.source_file))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codegraph_core::{Confidence, Edge, FileType, GraphData, Node, NodeId, Visibility};
    use serde_json::Map;

    fn node(id: &str, label: &str, file: &str, vis: Option<Visibility>) -> Node {
        let mut n = Node {
            id: NodeId(id.into()),
            label: label.into(),
            file_type: FileType::Code,
            source_file: file.into(),
            source_location: Some("L1".into()),
            community: Some(0),
            repo: None,
            extra: Map::new(),
        };
        if let Some(v) = vis {
            n.set_visibility(v);
        }
        n
    }

    fn edge(s: &str, t: &str) -> Edge {
        Edge {
            source: NodeId(s.into()),
            target: NodeId(t.into()),
            relation: "calls".into(),
            confidence: Confidence::Extracted,
            source_file: "x".into(),
            source_location: None,
            confidence_score: None,
            weight: 1.0,
            context: None,
            cross_repo: false,
            extra: Map::new(),
        }
    }

    fn graph(nodes: Vec<Node>, edges: Vec<Edge>) -> KnowledgeGraph {
        KnowledgeGraph::from_graph_data(GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes,
            links: edges,
            hyperedges: vec![],
            built_at_commit: None,
        })
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// login (public) is called by test_login (a test). The forecast from this
    /// graph should predict the test and flag the public API.
    fn linked_graph() -> KnowledgeGraph {
        graph(
            vec![
                node("login", "login", "src/login.py", Some(Visibility::Public)),
                node("t", "test_login", "tests/test_login.py", None),
            ],
            vec![edge("t", "login")],
        )
    }

    #[test]
    fn predicts_the_edited_test_for_a_linked_change() {
        let eval = score_commit(
            "c1",
            "p1",
            &linked_graph(),
            &["src/login.py".to_string()],
            &set(&["tests/test_login.py"]),
            &set(&[]),
            3,
        );
        assert_eq!(eval.test.hits, 1);
        assert_eq!(eval.test.recall_pct(), 100, "the edited test was predicted");
        assert_eq!(eval.test.precision_pct(), 100);
        assert_eq!(eval.graph_nodes, 2);
    }

    #[test]
    fn scores_removed_api_against_the_public_flag() {
        // login is public and the commit removed it -> the forecast's public-API
        // flag is a true positive.
        let removed: BTreeSet<String> = [api_key("login", "src/login.py")].into_iter().collect();
        let eval = score_commit(
            "c1",
            "p1",
            &linked_graph(),
            &["src/login.py".to_string()],
            &set(&["tests/test_login.py"]),
            &removed,
            3,
        );
        assert_eq!(eval.api.recall_pct(), 100, "removed API was flagged");
    }

    #[test]
    fn a_test_added_in_the_commit_is_not_counted_against_recall() {
        // The forecast can only predict tests that existed at the parent. A test
        // file absent from the parent graph must not enter the recall denominator.
        let eval = score_commit(
            "c1",
            "p1",
            &linked_graph(), // parent graph has only test_login.py
            &["src/login.py".to_string()],
            &set(&["tests/test_login.py", "tests/test_brand_new.py"]),
            &set(&[]),
            3,
        );
        assert_eq!(
            eval.test.relevant, 1,
            "the brand-new test is excluded; only the pre-existing one is scored"
        );
        assert_eq!(eval.test.recall_pct(), 100);
    }

    #[test]
    fn a_disconnected_test_is_not_predicted() {
        // Same nodes but NO edge: the test does not depend on login, so the
        // forecast cannot (and should not) predict it -> recall 0. This is the
        // degraded case a CI gate floor would reject.
        let g = graph(
            vec![
                node("login", "login", "src/login.py", None),
                node("t", "test_login", "tests/test_login.py", None),
            ],
            vec![],
        );
        let eval = score_commit(
            "c1",
            "p1",
            &g,
            &["src/login.py".to_string()],
            &set(&["tests/test_login.py"]),
            &set(&[]),
            3,
        );
        assert_eq!(eval.test.recall_pct(), 0);
    }

    #[test]
    fn ci_gate_passes_good_fixture_and_rejects_degraded() {
        // A frozen "history" of linked commits: the forecast predicts the edited
        // tests, so pooled recall clears a high floor. The gate must accept it.
        let good = assemble(vec![
            score_commit(
                "c1",
                "p1",
                &linked_graph(),
                &["src/login.py".to_string()],
                &set(&["tests/test_login.py"]),
                &set(&[]),
                3,
            ),
            score_commit(
                "c2",
                "p2",
                &linked_graph(),
                &["src/login.py".to_string()],
                &set(&["tests/test_login.py"]),
                &set(&[]),
                3,
            ),
        ]);
        assert_eq!(good.test.recall_pct(), 100);
        assert!(good.meets_test_recall(80), "good fixture clears the floor");

        // A degraded predictor (disconnected graph) misses the tests -> the same
        // gate rejects it. Proves the gate bites.
        let disconnected = graph(
            vec![
                node("login", "login", "src/login.py", None),
                node("t", "test_login", "tests/test_login.py", None),
            ],
            vec![],
        );
        let bad = assemble(vec![score_commit(
            "c1",
            "p1",
            &disconnected,
            &["src/login.py".to_string()],
            &set(&["tests/test_login.py"]),
            &set(&[]),
            3,
        )]);
        assert_eq!(bad.test.recall_pct(), 0);
        assert!(
            !bad.meets_test_recall(80),
            "degraded predictor fails the gate"
        );
    }

    #[test]
    fn selectivity_is_pooled_blast_over_nodes() {
        // 1 dependent flagged out of 2 nodes -> 50%.
        let report = assemble(vec![score_commit(
            "c1",
            "p1",
            &linked_graph(),
            &["src/login.py".to_string()],
            &set(&[]),
            &set(&[]),
            3,
        )]);
        assert_eq!(report.selectivity_pct, 50, "{}", report.summary);
    }

    #[test]
    fn meets_floor_is_vacuously_true_with_no_truth() {
        let empty = assemble(vec![score_commit(
            "c1",
            "p1",
            &linked_graph(),
            &["src/login.py".to_string()],
            &set(&[]),
            &set(&[]),
            3,
        )]);
        assert_eq!(empty.test.relevant, 0);
        assert!(
            empty.meets_test_recall(90),
            "nothing to recall -> not a failure"
        );
    }
}
