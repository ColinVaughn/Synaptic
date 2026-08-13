use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{PatchInspection, PatchPolicy, RepairBrief, validate_patch};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedPatch {
    pub unified_diff: String,
    pub rationale: String,
}

pub trait PatchGenerator: Send + Sync {
    fn generate(
        &self,
        brief: &RepairBrief,
        worktree: &Path,
    ) -> Result<GeneratedPatch, PatchGenerationError>;

    /// A retry receives no broader repository context: only the immutable
    /// brief, the previous patch, and its bounded failure report.
    fn retry(
        &self,
        brief: &RepairBrief,
        worktree: &Path,
        _prior_patch: &GeneratedPatch,
        _failure: &RepairFailure,
    ) -> Result<GeneratedPatch, PatchGenerationError> {
        self.generate(brief, worktree)
    }
}

pub trait PatchVerifier: Send + Sync {
    fn verify(
        &self,
        worktree: &Path,
        patch: &GeneratedPatch,
        inspection: &PatchInspection,
    ) -> VerificationReport;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateOutcome {
    Passed,
    Failed,
    Inconclusive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateResult {
    pub gate: String,
    pub outcome: GateOutcome,
    pub detail: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationReport {
    pub version: u32,
    pub verified: bool,
    pub gates: Vec<GateResult>,
}

impl VerificationReport {
    pub fn from_gates(gates: Vec<GateResult>) -> Self {
        let verified =
            !gates.is_empty() && gates.iter().all(|gate| gate.outcome == GateOutcome::Passed);
        Self {
            version: 1,
            verified,
            gates,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepairFailure {
    PatchPolicy { detail: String },
    Verification { report: VerificationReport },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairAttempt {
    pub number: usize,
    pub patch_digest: String,
    pub rationale: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub inspection: Option<PatchInspection>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub verification: Option<VerificationReport>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub failure: Option<RepairFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairOutcome {
    pub version: u32,
    pub run_id: String,
    pub verified: bool,
    pub attempts: Vec<RepairAttempt>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub final_patch: Option<GeneratedPatch>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub final_verification: Option<VerificationReport>,
}

/// Produce bounded learning evidence for every failed attempt, including failures
/// that preceded an ultimately successful retry.
pub fn failed_attempt_summary(outcome: &RepairOutcome) -> Option<String> {
    let mut entries = Vec::new();
    for attempt in &outcome.attempts {
        let Some(failure) = &attempt.failure else {
            continue;
        };
        let detail = match failure {
            RepairFailure::PatchPolicy { detail } => format!("patch policy: {detail}"),
            RepairFailure::Verification { report } => {
                let gates = report
                    .gates
                    .iter()
                    .filter(|gate| gate.outcome != GateOutcome::Passed)
                    .map(|gate| format!("{}={:?}: {}", gate.gate, gate.outcome, gate.detail))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("verification: {gates}")
            }
        };
        entries.push(format!("attempt {} {detail}", attempt.number));
    }
    if entries.is_empty() {
        return None;
    }
    let summary = crate::redaction::redact_sensitive_text(&entries.join("; "));
    Some(summary.chars().take(4_000).collect())
}

pub fn run_repair_attempts(
    brief: &RepairBrief,
    worktree: &Path,
    policy: &PatchPolicy,
    generator: &dyn PatchGenerator,
    verifier: &dyn PatchVerifier,
    max_attempts: usize,
) -> Result<RepairOutcome, RepairError> {
    if max_attempts == 0 || max_attempts > 3 {
        return Err(RepairError::InvalidAttempts(max_attempts));
    }
    let mut patch = generator.generate(brief, worktree)?;
    let mut attempts = Vec::new();
    for number in 1..=max_attempts {
        let patch_digest = blake3::hash(patch.unified_diff.as_bytes())
            .to_hex()
            .to_string();
        match validate_patch(worktree, &patch.unified_diff, policy) {
            Err(error) => {
                let failure = RepairFailure::PatchPolicy {
                    detail: error.to_string(),
                };
                attempts.push(RepairAttempt {
                    number,
                    patch_digest,
                    rationale: patch.rationale.clone(),
                    inspection: None,
                    verification: None,
                    failure: Some(failure.clone()),
                });
                if number == max_attempts {
                    break;
                }
                patch = generator.retry(brief, worktree, &patch, &failure)?;
            }
            Ok(inspection) => {
                let verification = verifier.verify(worktree, &patch, &inspection);
                if verification.verified {
                    attempts.push(RepairAttempt {
                        number,
                        patch_digest,
                        rationale: patch.rationale.clone(),
                        inspection: Some(inspection),
                        verification: Some(verification.clone()),
                        failure: None,
                    });
                    return Ok(RepairOutcome {
                        version: 1,
                        run_id: brief.id.clone(),
                        verified: true,
                        attempts,
                        final_patch: Some(patch),
                        final_verification: Some(verification),
                    });
                }
                let failure = RepairFailure::Verification {
                    report: verification.clone(),
                };
                attempts.push(RepairAttempt {
                    number,
                    patch_digest,
                    rationale: patch.rationale.clone(),
                    inspection: Some(inspection),
                    verification: Some(verification),
                    failure: Some(failure.clone()),
                });
                if number == max_attempts {
                    break;
                }
                patch = generator.retry(brief, worktree, &patch, &failure)?;
            }
        }
    }
    let final_verification = attempts
        .last()
        .and_then(|attempt| attempt.verification.clone());
    Ok(RepairOutcome {
        version: 1,
        run_id: brief.id.clone(),
        verified: false,
        attempts,
        final_patch: None,
        final_verification,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum PatchGenerationError {
    #[error("patch generator failed: {0}")]
    Failed(String),
    #[error("patch generator output is invalid: {0}")]
    InvalidOutput(String),
    #[error("patch generator I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("patch generator JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum RepairError {
    #[error("max_attempts must be between one and three, got {0}")]
    InvalidAttempts(usize),
    #[error(transparent)]
    Generation(#[from] PatchGenerationError),
}
