//! Change forecasting for CodeGraph.
//!
//! Given the set of files a change touches (or a `git diff`), this crate
//! composes existing primitives into a single `ChangeForecast`: which graph
//! nodes the change defines, the reverse-impact blast radius that depends on
//! them, which of the edited nodes are public API, and (when a time-travel diff
//! is supplied) the new import cycles, removed public APIs, and dependency
//! deltas a change introduces. CodeGraph never edits source; the forecast is
//! data an AI agent reads before it edits.
#![forbid(unsafe_code)]

mod cochange;
mod edit;
mod editforecast;
mod forecast;
mod render;
mod risk;

pub use cochange::{co_change, CoChange, CoChangeOptions};
pub use edit::{assess_edit, EditDependent, EditImpact, EditKind};
pub use editforecast::{forecast_edit, EditForecast};
pub use forecast::{
    fold_diff_report, forecast_changes, forecast_changes_with_index, refine_risk, refresh_summary,
    ChangeForecast, DepEdge, DependencyDelta, ForecastOptions, ImpactHit, NodeRef, VerifyStep,
    FORECAST_VERSION,
};
pub use render::{render_edit_markdown, render_markdown};
pub use risk::{assess_risk, RiskFactors, RiskScore};

/// Errors the prediction pipeline can surface.
#[derive(Debug, thiserror::Error)]
pub enum PredictError {
    #[error("history error: {0}")]
    History(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}
