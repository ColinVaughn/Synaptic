//! Evaluation and calibration for Synaptic's change forecasts.
//!
//! Two pieces. The **ledger** records each forecast and (later) its real outcome,
//! keyed by commit, so predictions can be audited and replayed. The **replay
//! harness** walks a range of history, re-predicts each commit's change from the
//! parent-state graph, and scores the prediction against git ground truth (the
//! tests actually edited in the commit, and the time-travel diff's removed APIs).
//! It reports the STARTS / Meta-PTS shape: recall (did we catch the real signal),
//! precision (how noisy), and selectivity (how much we narrowed the graph).
//!
//! Calibration is advisory: the harness measures, it does not silently retune.
#![forbid(unsafe_code)]

mod calibrate;
mod corpus;
mod cross_language;
pub mod groundtruth;
mod ledger;
mod replay;
mod scale;
mod scoring;

pub use calibrate::{
    Bin, CalibrationReport, Sample, brier, calibrate_history, reliability, samples_from_history,
};
pub use corpus::{
    BlastScore, CorpusReport, FixtureReport, PrF1, ResolutionReport, build_fixture,
    resolution_coverage, run_corpus, score_fixture,
};
pub use cross_language::{CrossLanguageReport, calibrate_cross_language};
pub use groundtruth::{GroundTruth, Manifest};
pub use ledger::{Ledger, PredictionRecord};
pub use replay::{CommitEval, ReplayOptions, ReplayReport, replay, score_commit};
pub use scale::{ScaleEnv, ScaleManifest, ScaleRepo, ScaleReport, ScaleResult, Skip, run_scale};
pub use scoring::{Scores, aggregate, score_sets};

/// Errors the evaluation pipeline can surface.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("history error: {0}")]
    History(String),
    #[error("git error: {0}")]
    Git(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}
