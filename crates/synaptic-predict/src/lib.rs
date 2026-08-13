//! Change forecasting for Synaptic.
//!
//! Given the set of files a change touches (or a `git diff`), this crate
//! composes existing primitives into a single `ChangeForecast`: which graph
//! nodes the change defines, the reverse-impact blast radius that depends on
//! them, which of the edited nodes are public API, and (when a time-travel diff
//! is supplied) the new import cycles, removed public APIs, and dependency
//! deltas a change introduces. Synaptic never edits source; the forecast is
//! data an AI agent reads before it edits.
#![forbid(unsafe_code)]

mod cochange;
mod edit;
mod editforecast;
mod forecast;
mod render;
mod risk;

pub use cochange::{CoChange, CoChangeOptions, co_change};
pub use edit::{EditDependent, EditImpact, EditKind, assess_edit};
pub use editforecast::{EditForecast, forecast_edit};
pub use forecast::{
    ChangeForecast, DepEdge, DependencyDelta, FORECAST_VERSION, ForecastFold, ForecastOptions,
    ImpactHit, NodeRef, VerifyStep, fold_diff_report, forecast_changes,
    forecast_changes_with_index, forecast_nodes, forecast_nodes_with_index, refine_risk,
    refresh_summary,
};
// `forecast_nodes_with_index` takes a `&ReverseImpactIndex` and the walk it does
// is fixed to the relations the index was built with, so a caller outside this
// crate needs both the type and that relation set. Re-exported here rather than
// forcing every caller to also depend on `synaptic-query`.
pub use render::{render_edit_markdown, render_markdown};
pub use risk::{RiskFactors, RiskScore, assess_risk};
pub use synaptic_query::{DEFAULT_AFFECTED_RELATIONS, ReverseImpactIndex};

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
