use serde::{Deserialize, Serialize};

use crate::{
    deterministic_branch, ApiChangeEvent, ApiRunRecord, RepairBrief, RepairOutcome, RunState,
    VerificationReport,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifiedRunHandoff {
    pub version: u32,
    pub engine_version: String,
    pub branch: String,
    pub run: ApiRunRecord,
    pub event: ApiChangeEvent,
    pub brief: RepairBrief,
    pub outcome: RepairOutcome,
    pub verification: VerificationReport,
    pub patch: String,
    pub patch_digest: String,
    pub bundle_digest: String,
}

impl VerifiedRunHandoff {
    pub fn new(
        run: ApiRunRecord,
        event: ApiChangeEvent,
        brief: RepairBrief,
        outcome: RepairOutcome,
        verification: VerificationReport,
        patch: String,
    ) -> Result<Self, HandoffError> {
        let branch = deterministic_branch(&event.vendor, &event.id)
            .map_err(|error| HandoffError::Integrity(error.to_string()))?;
        let patch_digest = blake3::hash(patch.as_bytes()).to_hex().to_string();
        let mut handoff = Self {
            version: 1,
            engine_version: env!("CARGO_PKG_VERSION").into(),
            branch,
            run,
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

    pub fn verify(&self) -> Result<(), HandoffError> {
        if self.version != 1 {
            return Err(HandoffError::UnsupportedVersion(self.version));
        }
        if self.engine_version != env!("CARGO_PKG_VERSION") {
            return Err(HandoffError::EngineVersion {
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
            return Err(HandoffError::NotVerified);
        }
        if self.run.id != self.brief.id
            || self.run.id != self.outcome.run_id
            || self.run.event_id != self.event.id
            || self.brief.event.id != self.event.id
            || self.run.base_sha != self.brief.base_sha
            || self.brief.event != self.event
        {
            return Err(HandoffError::Integrity(
                "run, event, brief, and outcome identities disagree".into(),
            ));
        }
        let expected_branch = deterministic_branch(&self.event.vendor, &self.event.id)
            .map_err(|error| HandoffError::Integrity(error.to_string()))?;
        if self.branch != expected_branch {
            return Err(HandoffError::Integrity(
                "deterministic branch mismatch".into(),
            ));
        }
        let expected_patch = self
            .outcome
            .final_patch
            .as_ref()
            .ok_or(HandoffError::NotVerified)?;
        if expected_patch.unified_diff != self.patch {
            return Err(HandoffError::Integrity(
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
            return Err(HandoffError::Integrity("patch digest mismatch".into()));
        }
        if self.bundle_digest != self.calculate_bundle_digest()? {
            return Err(HandoffError::Integrity("bundle digest mismatch".into()));
        }
        Ok(())
    }

    fn calculate_bundle_digest(&self) -> Result<String, HandoffError> {
        let material = serde_json::to_vec(&(
            self.version,
            &self.engine_version,
            &self.branch,
            &self.run,
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
pub enum HandoffError {
    #[error("unsupported verified-run handoff version {0}")]
    UnsupportedVersion(u32),
    #[error("verified-run handoff requires Synaptic {expected}, got {actual}")]
    EngineVersion { expected: String, actual: String },
    #[error("verified-run handoff is not conclusively verified")]
    NotVerified,
    #[error("verified-run handoff integrity error: {0}")]
    Integrity(String),
    #[error("verified-run handoff JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}
