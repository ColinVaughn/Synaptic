use serde::{Deserialize, Serialize};
use synaptic_api::{
    deterministic_vulnerability_branch, ApiChangeEvent, ApiRunRecord, RepairBrief, RepairOutcome,
    RunState, VerificationReport,
};

use crate::FindingRecord;
use crate::{repair_inputs, FindingState};

/// Credential-separated handoff for one conclusively verified vulnerability repair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifiedVulnerabilityRunHandoff {
    pub version: u32,
    pub engine_version: String,
    pub branch: String,
    pub run: ApiRunRecord,
    pub finding: FindingRecord,
    pub event: ApiChangeEvent,
    pub brief: RepairBrief,
    pub outcome: RepairOutcome,
    pub verification: VerificationReport,
    pub patch: String,
    pub patch_digest: String,
    pub bundle_digest: String,
}

impl VerifiedVulnerabilityRunHandoff {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        run: ApiRunRecord,
        finding: FindingRecord,
        event: ApiChangeEvent,
        brief: RepairBrief,
        outcome: RepairOutcome,
        verification: VerificationReport,
        patch: String,
    ) -> Result<Self, VulnerabilityHandoffError> {
        let branch = deterministic_vulnerability_branch(&finding.id)
            .map_err(|error| VulnerabilityHandoffError::Integrity(error.to_string()))?;
        let patch_digest = blake3::hash(patch.as_bytes()).to_hex().to_string();
        let mut handoff = Self {
            version: 1,
            engine_version: env!("CARGO_PKG_VERSION").into(),
            branch,
            run,
            finding,
            event,
            brief,
            outcome,
            verification,
            patch,
            patch_digest,
            bundle_digest: String::new(),
        };
        handoff.bundle_digest = handoff.calculate_bundle_digest()?;
        handoff.verify()?;
        Ok(handoff)
    }

    pub fn verify(&self) -> Result<(), VulnerabilityHandoffError> {
        if self.version != 1 {
            return Err(VulnerabilityHandoffError::UnsupportedVersion(self.version));
        }
        if self.engine_version != env!("CARGO_PKG_VERSION") {
            return Err(VulnerabilityHandoffError::EngineVersion {
                expected: env!("CARGO_PKG_VERSION").into(),
                actual: self.engine_version.clone(),
            });
        }
        if self.run.state != RunState::Verified
            || !self.verification.verified
            || !self.outcome.verified
            || self.run.verification.as_ref() != Some(&self.verification)
            || self.outcome.final_verification.as_ref() != Some(&self.verification)
        {
            return Err(VulnerabilityHandoffError::NotVerified);
        }
        if self.run.id != self.brief.id
            || self.run.id != self.outcome.run_id
            || self.run.event_id != self.finding.id
            || self.event.id != self.finding.id
            || self.brief.event.id != self.finding.id
            || self.run.base_sha != self.brief.base_sha
            || self.finding.base_sha != self.run.base_sha
            || self.run.policy_digest != self.finding.policy_digest
            || self.brief.event != self.event
            || self.finding.id != self.finding.finding.id
        {
            return Err(VulnerabilityHandoffError::Integrity(
                "finding, run, event, brief, and outcome identities disagree".into(),
            ));
        }
        if self.finding.state != FindingState::Verified {
            return Err(VulnerabilityHandoffError::NotVerified);
        }
        if repair_inputs(&self.finding.finding, self.finding.created_at)
            .map(|inputs| inputs.event)
            .as_ref()
            != Some(&self.event)
        {
            return Err(VulnerabilityHandoffError::Integrity(
                "repair event is not the canonical event for the finding".into(),
            ));
        }
        let expected_branch = deterministic_vulnerability_branch(&self.finding.id)
            .map_err(|error| VulnerabilityHandoffError::Integrity(error.to_string()))?;
        if self.branch != expected_branch {
            return Err(VulnerabilityHandoffError::Integrity(
                "deterministic vulnerability branch mismatch".into(),
            ));
        }
        let expected_patch = self
            .outcome
            .final_patch
            .as_ref()
            .ok_or(VulnerabilityHandoffError::NotVerified)?;
        if expected_patch.unified_diff != self.patch {
            return Err(VulnerabilityHandoffError::Integrity(
                "handoff patch differs from the verified outcome".into(),
            ));
        }
        let patch_digest = blake3::hash(self.patch.as_bytes()).to_hex().to_string();
        if self.patch_digest != patch_digest
            || self
                .outcome
                .attempts
                .last()
                .map(|attempt| attempt.patch_digest.as_str())
                != Some(patch_digest.as_str())
        {
            return Err(VulnerabilityHandoffError::Integrity(
                "patch digest mismatch".into(),
            ));
        }
        if self.bundle_digest != self.calculate_bundle_digest()? {
            return Err(VulnerabilityHandoffError::Integrity(
                "bundle digest mismatch".into(),
            ));
        }
        Ok(())
    }

    fn calculate_bundle_digest(&self) -> Result<String, VulnerabilityHandoffError> {
        let material = serde_json::to_vec(&(
            self.version,
            &self.engine_version,
            &self.branch,
            &self.run,
            &self.finding,
            &self.event,
            &self.brief,
            &self.outcome,
            &self.verification,
            &self.patch,
            &self.patch_digest,
        ))?;
        Ok(blake3::hash(&material).to_hex().to_string())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VulnerabilityHandoffError {
    #[error("unsupported vulnerability handoff version {0}")]
    UnsupportedVersion(u32),
    #[error("vulnerability handoff requires Synaptic {expected}, got {actual}")]
    EngineVersion { expected: String, actual: String },
    #[error("vulnerability handoff is not conclusively verified")]
    NotVerified,
    #[error("vulnerability handoff integrity error: {0}")]
    Integrity(String),
    #[error("vulnerability handoff JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}
