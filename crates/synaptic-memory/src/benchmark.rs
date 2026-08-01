use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{MemoryKind, MemoryPrincipal, MemoryQuery, MemoryStore};

const SCHEMA: &str = "synaptic.memory-benchmark/v1";
const MAX_CASES: usize = 10_000;

#[derive(Debug, Deserialize)]
struct BenchmarkManifest {
    schema: String,
    cases: Vec<BenchmarkCase>,
}

#[derive(Debug, Deserialize)]
struct BenchmarkCase {
    name: String,
    #[serde(default)]
    query: String,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    kinds: Vec<MemoryKind>,
    expected_sources: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkCaseResult {
    pub name: String,
    pub first_relevant_rank: Option<usize>,
    pub candidate_records: usize,
    pub total_records: usize,
    pub candidate_fraction: f64,
    pub retrieved_sources: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub schema: String,
    pub cases: usize,
    pub recall_at_1: f64,
    pub recall_at_5: f64,
    pub mean_reciprocal_rank: f64,
    pub mean_candidate_fraction: f64,
    pub misses: Vec<String>,
    pub results: Vec<BenchmarkCaseResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkGate {
    pub min_recall_at_5: f64,
    pub min_mean_reciprocal_rank: f64,
    pub max_mean_candidate_fraction: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum BenchmarkError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("benchmark schema must be {SCHEMA:?}, got {0:?}")]
    InvalidSchema(String),
    #[error("benchmark exceeds the {MAX_CASES} case safety limit")]
    TooManyCases,
    #[error("benchmark case {0:?} must contain an expected source")]
    MissingExpectation(String),
    #[error(transparent)]
    Memory(#[from] crate::MemoryError),
}

#[derive(Debug, thiserror::Error)]
#[error("repository-memory benchmark gate failed: {reasons}")]
pub struct BenchmarkGateFailure {
    reasons: String,
}

/// Run a deterministic top-1/top-5 localization evaluation over source
/// artifacts. Cases can be generated from historical bugs, commits, or a
/// SWE-bench-style task set without coupling the library to one corpus.
pub fn run_benchmark_file(
    store: &MemoryStore,
    path: &Path,
    principal: &MemoryPrincipal,
) -> Result<BenchmarkReport, BenchmarkError> {
    let manifest: BenchmarkManifest = serde_json::from_slice(&std::fs::read(path)?)?;
    if manifest.schema != SCHEMA {
        return Err(BenchmarkError::InvalidSchema(manifest.schema));
    }
    if manifest.cases.len() > MAX_CASES {
        return Err(BenchmarkError::TooManyCases);
    }
    let mut results = Vec::with_capacity(manifest.cases.len());
    let mut at_1 = 0usize;
    let mut at_5 = 0usize;
    let mut reciprocal_rank = 0.0;
    let mut candidate_fraction = 0.0;
    let mut misses = Vec::new();
    for case in manifest.cases {
        if case.expected_sources.is_empty() {
            return Err(BenchmarkError::MissingExpectation(case.name));
        }
        let search = store.search_with_diagnostics_authorized(
            &MemoryQuery {
                text: case.query,
                symbol: case.symbol,
                kinds: case.kinds,
                limit: 5,
                ..MemoryQuery::default()
            },
            principal,
        )?;
        let rank = search.hits.iter().position(|hit| {
            hit.record.sources.iter().any(|source| {
                case.expected_sources
                    .iter()
                    .any(|expected| expected == &source.uri)
            })
        });
        let one_based = rank.map(|rank| rank + 1);
        if one_based == Some(1) {
            at_1 += 1;
        }
        if one_based.is_some_and(|rank| rank <= 5) {
            at_5 += 1;
        } else {
            misses.push(case.name.clone());
        }
        if let Some(rank) = one_based {
            reciprocal_rank += 1.0 / rank as f64;
        }
        let fraction = if search.diagnostics.total_records == 0 {
            0.0
        } else {
            search.diagnostics.candidate_records as f64 / search.diagnostics.total_records as f64
        };
        candidate_fraction += fraction;
        let retrieved_sources = search
            .hits
            .iter()
            .flat_map(|hit| hit.record.sources.iter().map(|source| source.uri.clone()))
            .collect();
        results.push(BenchmarkCaseResult {
            name: case.name,
            first_relevant_rank: one_based,
            candidate_records: search.diagnostics.candidate_records,
            total_records: search.diagnostics.total_records,
            candidate_fraction: fraction,
            retrieved_sources,
        });
    }
    let count = results.len();
    let denominator = count.max(1) as f64;
    Ok(BenchmarkReport {
        schema: "synaptic.memory-benchmark-report/v1".into(),
        cases: count,
        recall_at_1: at_1 as f64 / denominator,
        recall_at_5: at_5 as f64 / denominator,
        mean_reciprocal_rank: reciprocal_rank / denominator,
        mean_candidate_fraction: candidate_fraction / denominator,
        misses,
        results,
    })
}

pub fn enforce_benchmark_gate(
    report: &BenchmarkReport,
    gate: BenchmarkGate,
) -> Result<(), BenchmarkGateFailure> {
    let mut reasons = Vec::new();
    if report.recall_at_5 < gate.min_recall_at_5 {
        reasons.push(format!(
            "recall@5 {:.4} < {:.4}",
            report.recall_at_5, gate.min_recall_at_5
        ));
    }
    if report.mean_reciprocal_rank < gate.min_mean_reciprocal_rank {
        reasons.push(format!(
            "MRR {:.4} < {:.4}",
            report.mean_reciprocal_rank, gate.min_mean_reciprocal_rank
        ));
    }
    if report.mean_candidate_fraction > gate.max_mean_candidate_fraction {
        reasons.push(format!(
            "candidate fraction {:.4} > {:.4}",
            report.mean_candidate_fraction, gate.max_mean_candidate_fraction
        ));
    }
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(BenchmarkGateFailure {
            reasons: reasons.join("; "),
        })
    }
}
