use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::finding::Finding;

/// Lifecycle state of a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingState {
    /// Detected and awaiting triage or remediation.
    Open,
    /// Suppressed by an unexpired policy exception.
    Accepted,
    /// A remediation patch is being generated or verified.
    Remediating,
    /// A remediation passed every verification gate.
    Verified,
    /// A draft pull request is open for the remediation.
    PullRequestOpen,
    /// No longer detected: the dependency was upgraded, replaced or removed.
    Resolved,
}

/// What happened to a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Detected,
    Redetected,
    ExceptionGranted,
    ExceptionExpired,
    RemediationPlanned,
    RemediationVerified,
    RemediationFailed,
    PullRequestOpened,
    Resolved,
}

/// One immutable entry in a finding's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub at: i64,
    pub kind: DecisionKind,
    pub actor: String,
    pub detail: String,
}

/// The auditable record for one finding.
///
/// Decisions are only ever appended. An accepted risk keeps its justification,
/// approver and expiry permanently, and a later reversal is a new entry rather
/// than an edit to the old one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FindingRecord {
    pub version: u32,
    pub id: String,
    pub repository_identity: String,
    pub base_sha: String,
    pub policy_digest: String,
    pub state: FindingState,
    pub finding: Finding,
    pub decisions: Vec<Decision>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl FindingRecord {
    pub const VERSION: u32 = 1;
}

/// Append-only store of finding records under `.synaptic/vuln/findings`.
#[derive(Debug, Clone)]
pub struct FindingStore {
    root: PathBuf,
}

impl FindingStore {
    pub fn new(repository_root: &Path) -> Self {
        Self {
            root: repository_root.join(".synaptic/vuln/findings"),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.root
    }

    /// Record a finding, appending a decision rather than overwriting history.
    pub fn upsert(
        &self,
        finding: &Finding,
        repository_identity: &str,
        base_sha: &str,
        policy_digest: &str,
        decision: Decision,
    ) -> Result<FindingRecord, LedgerError> {
        let path = record_path(&self.root, &finding.id)?;
        let now = unix_timestamp();
        let mut record = match self.get(&finding.id)? {
            Some(mut existing) => {
                // The finding's analysis is refreshed, its history is not.
                existing.finding = finding.clone();
                existing.base_sha = base_sha.to_string();
                existing.policy_digest = policy_digest.to_string();
                existing.updated_at = now;
                existing
            }
            None => FindingRecord {
                version: FindingRecord::VERSION,
                id: finding.id.clone(),
                repository_identity: repository_identity.to_string(),
                base_sha: base_sha.to_string(),
                policy_digest: policy_digest.to_string(),
                state: FindingState::Open,
                finding: finding.clone(),
                decisions: Vec::new(),
                created_at: now,
                updated_at: now,
            },
        };
        record.decisions.push(decision);
        write_record(&path, &record)?;
        Ok(record)
    }

    /// Read one record.
    pub fn get(&self, id: &str) -> Result<Option<FindingRecord>, LedgerError> {
        let path = record_path(&self.root, id)?;
        if !path.exists() {
            return Ok(None);
        }
        let body = fs::read_to_string(&path).map_err(|source| LedgerError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(Some(serde_json::from_str(&body)?))
    }

    /// Every stored record, in stable id order.
    pub fn list(&self) -> Result<Vec<FindingRecord>, LedgerError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let entries = fs::read_dir(&self.root).map_err(|source| LedgerError::Io {
            path: self.root.clone(),
            source,
        })?;
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| LedgerError::Io {
                path: self.root.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                paths.push(path);
            }
        }
        paths.sort();

        let mut records = Vec::new();
        for path in paths {
            let body = fs::read_to_string(&path).map_err(|source| LedgerError::Io {
                path: path.clone(),
                source,
            })?;
            records.push(serde_json::from_str(&body)?);
        }
        Ok(records)
    }

    /// Replace a record's state, appending the decision that caused the change.
    pub fn transition(
        &self,
        id: &str,
        state: FindingState,
        decision: Decision,
    ) -> Result<FindingRecord, LedgerError> {
        let mut record = self
            .get(id)?
            .ok_or_else(|| LedgerError::Unknown(id.to_string()))?;
        record.state = state;
        record.updated_at = unix_timestamp();
        record.decisions.push(decision);
        write_record(&record_path(&self.root, id)?, &record)?;
        Ok(record)
    }
}

/// A decision stamped with the current wall-clock time.
pub fn decision(kind: DecisionKind, actor: &str, detail: impl Into<String>) -> Decision {
    Decision {
        at: unix_timestamp(),
        kind,
        actor: actor.to_string(),
        detail: detail.into(),
    }
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("cannot access the findings ledger at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("finding record is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("finding {0} is not in the ledger")]
    Unknown(String),
    #[error("finding id {0:?} is not a valid record name")]
    InvalidId(String),
}

/// Write a record through a temporary file so an interrupted write cannot
/// truncate an existing audit record.
fn write_record(path: &Path, record: &FindingRecord) -> Result<(), LedgerError> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|source| LedgerError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let body = serde_json::to_string_pretty(record)?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, body).map_err(|source| LedgerError::Io {
        path: temporary.clone(),
        source,
    })?;
    fs::rename(&temporary, path).map_err(|source| LedgerError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn record_path(root: &Path, id: &str) -> Result<PathBuf, LedgerError> {
    // Ids are generated, but this store is also reachable from CLI input, so
    // the filename is validated rather than trusted.
    if id.is_empty()
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(LedgerError::InvalidId(id.to_string()));
    }
    Ok(root.join(format!("{id}.json")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::applicability::{assess_applicability, ApplicabilityInput};
    use crate::matching::VersionMatch;
    use crate::plan::{CompatibilityRisk, RemediationKind, RemediationPlan, VersionAvailability};
    use crate::severity::{Priority, SeverityAssessment, SeverityBand, SeverityScoreSource};
    use synaptic_api::{Ecosystem, PackageCoordinate};

    fn sample_finding(id: &str) -> Finding {
        Finding {
            version: Finding::VERSION,
            id: id.to_string(),
            advisory_id: "RUSTSEC-2026-0001".into(),
            aliases: vec!["CVE-2026-1111".into()],
            summary: Some("example".into()),
            package: PackageCoordinate::new(Ecosystem::Cargo, "example"),
            resolved_version: "1.0.0".into(),
            dependency_path: Vec::new(),
            is_direct_dependency: true,
            verdict: assess_applicability(&ApplicabilityInput {
                version_match: VersionMatch::Affected,
                ..Default::default()
            }),
            severity: SeverityAssessment {
                band: SeverityBand::High,
                base_score: Some(7.5),
                vector: None,
                source: SeverityScoreSource::CvssV3Vector,
            },
            priority: Priority::P1,
            remediation: RemediationPlan {
                kind: RemediationKind::Upgrade,
                recommended_version: Some("1.1.0".into()),
                availability: VersionAvailability::Unverified,
                compatibility_risk: CompatibilityRisk::Minor,
                required_changes: Vec::new(),
                validation_commands: Vec::new(),
                notes: Vec::new(),
            },
            references: Vec::new(),
        }
    }

    fn store() -> (tempfile::TempDir, FindingStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FindingStore::new(dir.path());
        (dir, store)
    }

    #[test]
    fn records_a_new_finding_as_open() {
        let (_dir, store) = store();

        let record = store
            .upsert(
                &sample_finding("vuln_finding_aaa"),
                "repo",
                "abc123",
                "policy-digest",
                decision(DecisionKind::Detected, "scan", "first detection"),
            )
            .unwrap();

        assert_eq!(record.state, FindingState::Open);
        assert_eq!(record.decisions.len(), 1);
        assert_eq!(record.decisions[0].kind, DecisionKind::Detected);
    }

    #[test]
    fn rescanning_appends_history_instead_of_replacing_it() {
        let (_dir, store) = store();
        let finding = sample_finding("vuln_finding_aaa");

        store
            .upsert(
                &finding,
                "repo",
                "abc123",
                "policy-digest",
                decision(DecisionKind::Detected, "scan", "first"),
            )
            .unwrap();
        let second = store
            .upsert(
                &finding,
                "repo",
                "def456",
                "policy-digest",
                decision(DecisionKind::Redetected, "scan", "second"),
            )
            .unwrap();

        assert_eq!(second.decisions.len(), 2);
        assert_eq!(second.decisions[0].kind, DecisionKind::Detected);
        assert_eq!(second.decisions[1].kind, DecisionKind::Redetected);
        assert_eq!(
            second.created_at,
            second.decisions[0].at.max(second.created_at)
        );
    }

    #[test]
    fn a_rescan_refreshes_the_base_sha_without_losing_the_creation_time() {
        let (_dir, store) = store();
        let finding = sample_finding("vuln_finding_aaa");

        let first = store
            .upsert(
                &finding,
                "repo",
                "abc123",
                "d",
                decision(DecisionKind::Detected, "scan", ""),
            )
            .unwrap();
        let second = store
            .upsert(
                &finding,
                "repo",
                "def456",
                "d",
                decision(DecisionKind::Redetected, "scan", ""),
            )
            .unwrap();

        assert_eq!(second.base_sha, "def456");
        assert_eq!(second.created_at, first.created_at);
    }

    #[test]
    fn reads_back_a_stored_record() {
        let (_dir, store) = store();
        store
            .upsert(
                &sample_finding("vuln_finding_aaa"),
                "repo",
                "abc",
                "d",
                decision(DecisionKind::Detected, "scan", ""),
            )
            .unwrap();

        let record = store.get("vuln_finding_aaa").unwrap().unwrap();

        assert_eq!(record.finding.advisory_id, "RUSTSEC-2026-0001");
    }

    #[test]
    fn an_unknown_finding_reads_back_as_none() {
        let (_dir, store) = store();

        assert_eq!(store.get("vuln_finding_missing").unwrap(), None);
    }

    #[test]
    fn lists_records_in_stable_order() {
        let (_dir, store) = store();
        for id in ["vuln_finding_ccc", "vuln_finding_aaa", "vuln_finding_bbb"] {
            store
                .upsert(
                    &sample_finding(id),
                    "repo",
                    "abc",
                    "d",
                    decision(DecisionKind::Detected, "scan", ""),
                )
                .unwrap();
        }

        let ids = store
            .list()
            .unwrap()
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>();

        assert_eq!(
            ids,
            vec![
                "vuln_finding_aaa".to_string(),
                "vuln_finding_bbb".to_string(),
                "vuln_finding_ccc".to_string()
            ]
        );
    }

    #[test]
    fn an_empty_ledger_lists_nothing_rather_than_failing() {
        let (_dir, store) = store();

        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn a_transition_appends_the_decision_that_caused_it() {
        let (_dir, store) = store();
        store
            .upsert(
                &sample_finding("vuln_finding_aaa"),
                "repo",
                "abc",
                "d",
                decision(DecisionKind::Detected, "scan", ""),
            )
            .unwrap();

        let record = store
            .transition(
                "vuln_finding_aaa",
                FindingState::Accepted,
                decision(
                    DecisionKind::ExceptionGranted,
                    "security-review",
                    "unreachable until 2026-11-01",
                ),
            )
            .unwrap();

        assert_eq!(record.state, FindingState::Accepted);
        assert_eq!(record.decisions.len(), 2);
        assert_eq!(record.decisions[1].detail, "unreachable until 2026-11-01");
    }

    #[test]
    fn transitioning_an_unknown_finding_is_an_error() {
        let (_dir, store) = store();

        let error = store
            .transition(
                "vuln_finding_missing",
                FindingState::Resolved,
                decision(DecisionKind::Resolved, "scan", ""),
            )
            .unwrap_err();

        assert!(matches!(error, LedgerError::Unknown(_)));
    }

    #[test]
    fn a_finding_id_that_could_escape_the_store_directory_is_rejected() {
        let (_dir, store) = store();

        let error = store.get("../../etc/passwd").unwrap_err();

        assert!(matches!(error, LedgerError::InvalidId(_)));
    }

    #[test]
    fn history_survives_a_reopened_store() {
        let dir = tempfile::tempdir().unwrap();
        FindingStore::new(dir.path())
            .upsert(
                &sample_finding("vuln_finding_aaa"),
                "repo",
                "abc",
                "d",
                decision(DecisionKind::Detected, "scan", "first"),
            )
            .unwrap();

        let reopened = FindingStore::new(dir.path())
            .get("vuln_finding_aaa")
            .unwrap()
            .unwrap();

        assert_eq!(reopened.decisions.len(), 1);
    }
}
