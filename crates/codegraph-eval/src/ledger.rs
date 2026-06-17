//! The prediction ledger: one record per commit capturing what a forecast
//! predicted and (optionally) what actually happened. Stored as JSON under
//! `codegraph-out/predictions/<commit>.json` so predictions can be audited later
//! and the replay harness can compare prediction to outcome.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use codegraph_predict::ChangeForecast;

use crate::EvalError;

/// On-disk schema version for a ledger record.
pub const LEDGER_VERSION: u32 = 1;

/// One commit's prediction and (optionally) its measured outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictionRecord {
    pub version: u32,
    /// The commit the prediction is for.
    pub commit: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub base: Option<String>,
    pub changed_files: Vec<String>,
    /// Test files the forecast flagged as at-risk (the prediction).
    pub predicted_tests: Vec<String>,
    /// Public symbols the forecast flagged as at-risk (labels).
    pub predicted_public_apis: Vec<String>,
    pub blast_radius_total: usize,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub risk_score: Option<u8>,
    /// Tests actually changed in the commit (ground truth), once measured.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub actual_tests: Option<Vec<String>>,
    /// Public APIs actually removed in the commit (ground truth), once measured.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub actual_removed_apis: Option<Vec<String>>,
}

fn unique_test_files(forecast: &ChangeForecast) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut files = Vec::new();
    for h in &forecast.at_risk_tests {
        if seen.insert(h.file.clone()) {
            files.push(h.file.clone());
        }
    }
    files
}

impl PredictionRecord {
    /// Build a record from a forecast (the outcome fields are left unmeasured).
    pub fn from_forecast(commit: &str, forecast: &ChangeForecast) -> PredictionRecord {
        PredictionRecord {
            version: LEDGER_VERSION,
            commit: commit.to_string(),
            base: forecast.base.clone(),
            changed_files: forecast.changed_files.clone(),
            predicted_tests: unique_test_files(forecast),
            predicted_public_apis: forecast
                .public_api_breaks
                .iter()
                .map(|n| n.label.clone())
                .collect(),
            blast_radius_total: forecast.blast_radius_total,
            risk_score: forecast.risk.as_ref().map(|r| r.score),
            actual_tests: None,
            actual_removed_apis: None,
        }
    }

    /// Attach the measured ground truth.
    pub fn with_outcome(
        mut self,
        actual_tests: Vec<String>,
        actual_removed_apis: Vec<String>,
    ) -> PredictionRecord {
        self.actual_tests = Some(actual_tests);
        self.actual_removed_apis = Some(actual_removed_apis);
        self
    }
}

/// A directory-backed store of `PredictionRecord`s.
pub struct Ledger {
    dir: PathBuf,
}

impl Ledger {
    /// A ledger rooted at `dir` (typically `codegraph-out/predictions`).
    pub fn new(dir: PathBuf) -> Ledger {
        Ledger { dir }
    }

    /// The default ledger location under a repo's `codegraph-out`.
    pub fn under(repo_out: &Path) -> Ledger {
        Ledger::new(repo_out.join("predictions"))
    }

    /// Persist a record as `<commit>.json` (overwrites any prior record for it).
    pub fn record(&self, rec: &PredictionRecord) -> Result<PathBuf, EvalError> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{}.json", safe_name(&rec.commit)));
        std::fs::write(&path, serde_json::to_string_pretty(rec)?)?;
        Ok(path)
    }

    /// Load the record for a commit, if present.
    pub fn load(&self, commit: &str) -> Option<PredictionRecord> {
        let path = self.dir.join(format!("{}.json", safe_name(commit)));
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// All records, sorted by commit (deterministic).
    pub fn all(&self) -> Vec<PredictionRecord> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for entry in rd.flatten() {
                if entry.path().extension().is_some_and(|e| e == "json") {
                    if let Ok(text) = std::fs::read_to_string(entry.path()) {
                        if let Ok(rec) = serde_json::from_str::<PredictionRecord>(&text) {
                            out.push(rec);
                        }
                    }
                }
            }
        }
        out.sort_by(|a, b| a.commit.cmp(&b.commit));
        out
    }
}

/// Keep a commit-ish string safe as a filename (SHAs and refs are fine; guard
/// against a stray separator).
fn safe_name(commit: &str) -> String {
    commit.replace(['/', '\\', ':'], "_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use codegraph_predict::{forecast_changes, ForecastOptions};

    use codegraph_core::{Confidence, Edge, FileType, GraphData, Node, NodeId, Visibility};
    use codegraph_graph::KnowledgeGraph;
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

    fn forecast() -> ChangeForecast {
        let gd = GraphData {
            directed: true,
            multigraph: false,
            graph: Map::new(),
            nodes: vec![
                node("login", "login", "src/login.py", Some(Visibility::Public)),
                node("t", "test_login", "tests/test_login.py", None),
            ],
            links: vec![edge("t", "login")],
            hyperedges: vec![],
            built_at_commit: None,
        };
        let kg = KnowledgeGraph::from_graph_data(gd);
        forecast_changes(
            &kg,
            &["src/login.py".to_string()],
            &ForecastOptions::default(),
        )
    }

    #[test]
    fn record_captures_forecast_predictions() {
        let rec = PredictionRecord::from_forecast("abc123", &forecast());
        assert_eq!(rec.commit, "abc123");
        assert_eq!(rec.predicted_tests, vec!["tests/test_login.py"]);
        assert_eq!(rec.predicted_public_apis, vec!["login"]);
        assert!(rec.risk_score.is_some());
        assert!(rec.actual_tests.is_none(), "outcome not measured yet");
    }

    #[test]
    fn ledger_round_trips_a_record() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::new(tmp.path().to_path_buf());
        let rec = PredictionRecord::from_forecast("deadbeef", &forecast())
            .with_outcome(vec!["tests/test_login.py".into()], vec![]);
        ledger.record(&rec).unwrap();
        let back = ledger.load("deadbeef").expect("record loads");
        assert_eq!(back, rec);
        assert_eq!(back.actual_tests.unwrap(), vec!["tests/test_login.py"]);
    }

    #[test]
    fn ledger_lists_all_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::new(tmp.path().to_path_buf());
        for c in ["c", "a", "b"] {
            ledger
                .record(&PredictionRecord::from_forecast(c, &forecast()))
                .unwrap();
        }
        let commits: Vec<String> = ledger.all().into_iter().map(|r| r.commit).collect();
        assert_eq!(commits, vec!["a", "b", "c"], "sorted, all present");
    }

    #[test]
    fn missing_record_loads_none() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::new(tmp.path().to_path_buf());
        assert!(ledger.load("nope").is_none());
    }
}
