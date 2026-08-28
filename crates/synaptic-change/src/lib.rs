//! Deterministic implicit-requirement recovery and change-contract verification.
//!
//! Recovery composes Synaptic's existing task retrieval and reverse-impact
//! indexes. Verification is intentionally structural: callers attest executable
//! proofs after running them in their own trusted environment.
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use synaptic_core::{NodeId, sanitize_label};
use synaptic_graph::KnowledgeGraph;
use synaptic_memory::{
    MemoryError, MemoryKind, MemoryLifecycle, MemoryPrincipal, MemoryQuery, MemorySearchHit,
    MemoryStore, VerificationStatus,
};
use synaptic_predict::{ForecastOptions, NodeRef, forecast_nodes_with_index};
use synaptic_query::{QueryIndex, ReverseImpactIndex, TraversalMode};
use thiserror::Error;

pub const CHANGE_CONTRACT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractState {
    Draft,
    Approved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementCategory {
    Scope,
    Tests,
    ApiCompatibility,
    HistoricalInvariant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strength {
    Must,
    Should,
    Observe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceBand {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceTier {
    Executable,
    Structural,
    Historical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofKind {
    Scope,
    AffectedTests,
    PublicApiPresence,
    ManualAttestation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractSnapshot {
    pub repository: String,
    pub base_revision: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub graph_revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub id: String,
    pub tier: EvidenceTier,
    pub source: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofObligation {
    pub id: String,
    pub kind: ProofKind,
    pub description: String,
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    pub id: String,
    pub category: RequirementCategory,
    pub statement: String,
    pub strength: Strength,
    pub confidence: ConfidenceBand,
    pub evidence_ids: Vec<String>,
    pub proof_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractScope {
    pub anchors: Vec<NodeRef>,
    pub expected_files: Vec<String>,
    pub protected_symbols: Vec<NodeRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeContract {
    pub schema_version: u32,
    pub id: String,
    pub revision: u32,
    pub state: ContractState,
    pub task: String,
    pub snapshot: ContractSnapshot,
    pub requirements: Vec<Requirement>,
    pub evidence: Vec<EvidenceRef>,
    pub proofs: Vec<ProofObligation>,
    pub scope: ContractScope,
    pub unknowns: Vec<String>,
    pub contract_hash: String,
}

impl ChangeContract {
    pub fn verify_hash(&self) -> bool {
        self.contract_hash == self.computed_hash()
    }

    pub fn approve(mut self) -> Result<Self, ContractError> {
        if !self.verify_hash() {
            return Err(ContractError::Integrity);
        }
        if self.state == ContractState::Draft {
            self.state = ContractState::Approved;
            self.revision = self
                .revision
                .checked_add(1)
                .ok_or(ContractError::RevisionOverflow)?;
            self.seal();
        }
        Ok(self)
    }

    pub fn add_historical_constraints(
        &mut self,
        historical: &[HistoricalConstraint],
    ) -> Result<(), ContractError> {
        self.ensure_mutable()?;
        let mut existing = self
            .requirements
            .iter()
            .map(|requirement| requirement.id.clone())
            .collect::<HashSet<_>>();
        for item in historical {
            let requirement_id = stable_id(
                "r-history",
                &[item.source.as_str(), item.statement.as_str()],
            );
            if existing.contains(&requirement_id) {
                continue;
            }
            let evidence_item = EvidenceRef {
                id: stable_id(
                    "e-history",
                    &[item.source.as_str(), item.statement.as_str()],
                ),
                tier: EvidenceTier::Historical,
                source: item.source.clone(),
                summary: item.statement.clone(),
            };
            let proof = ProofObligation {
                id: stable_id(
                    "p-history",
                    &[item.source.as_str(), item.statement.as_str()],
                ),
                kind: ProofKind::ManualAttestation,
                description: format!("Attest historical constraint: {}", item.statement),
                targets: Vec::new(),
            };
            self.requirements.push(Requirement {
                id: requirement_id.clone(),
                category: RequirementCategory::HistoricalInvariant,
                statement: item.statement.clone(),
                strength: item.strength,
                confidence: item.confidence,
                evidence_ids: vec![evidence_item.id.clone()],
                proof_ids: vec![proof.id.clone()],
            });
            self.evidence.push(evidence_item);
            self.proofs.push(proof);
            existing.insert(requirement_id);
        }
        self.seal();
        Ok(())
    }

    pub fn note_unknown(&mut self, unknown: impl Into<String>) -> Result<(), ContractError> {
        self.ensure_mutable()?;
        let unknown = unknown.into();
        if !self.unknowns.contains(&unknown) {
            self.unknowns.push(unknown);
            self.seal();
        }
        Ok(())
    }

    fn ensure_mutable(&self) -> Result<(), ContractError> {
        if self.state != ContractState::Draft {
            return Err(ContractError::ApprovedImmutable);
        }
        if !self.verify_hash() {
            return Err(ContractError::Integrity);
        }
        Ok(())
    }

    fn seal(&mut self) {
        self.contract_hash = self.computed_hash();
    }

    fn computed_hash(&self) -> String {
        let mut unhashed = self.clone();
        unhashed.contract_hash.clear();
        let bytes = serde_json::to_vec(&unhashed).expect("change contract is serializable");
        blake3::hash(&bytes).to_hex().to_string()
    }
}

#[derive(Debug, Clone)]
pub struct RecoveryOptions {
    pub max_anchors: usize,
    pub depth: usize,
    pub max_hits: usize,
}

impl Default for RecoveryOptions {
    fn default() -> Self {
        Self {
            max_anchors: 8,
            depth: 3,
            max_hits: 200,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalConstraint {
    pub statement: String,
    pub source: String,
    pub strength: Strength,
    pub confidence: ConfidenceBand,
}

/// Recover source-grounded, active repository-wide decisions plus decisions
/// attached to the current graph anchors. The memory store retains lifecycle,
/// authorization, supersession, and ranking semantics; this adapter only maps
/// results into the change-contract vocabulary.
pub fn historical_constraints_from_memory(
    store: &MemoryStore,
    principal: &MemoryPrincipal,
    anchors: &[NodeRef],
    limit: usize,
) -> Result<Vec<HistoricalConstraint>, MemoryError> {
    let cap = if limit == 0 { 8 } else { limit };
    let kinds = vec![
        MemoryKind::ArchitectureDecision,
        MemoryKind::Invariant,
        MemoryKind::Convention,
        MemoryKind::Procedure,
    ];
    let subjects = anchors
        .iter()
        .flat_map(|anchor| [anchor.id.as_str(), anchor.file.as_str()])
        .filter(|subject| !subject.is_empty())
        .collect::<BTreeSet<_>>();
    let mut by_id = BTreeMap::<String, MemorySearchHit>::new();
    let mut include = |hit: MemorySearchHit| {
        if hit.record.lifecycle != MemoryLifecycle::Active || hit.record.sources.is_empty() {
            return;
        }
        match by_id.get_mut(&hit.record.id) {
            Some(existing) if hit.score > existing.score => *existing = hit,
            Some(_) => {}
            None => {
                by_id.insert(hit.record.id.clone(), hit);
            }
        }
    };
    let primary = MemoryStore::open(store.root());
    for hit in primary.search_authorized(
        &MemoryQuery {
            kinds: kinds.clone(),
            limit: usize::MAX,
            ..MemoryQuery::default()
        },
        principal,
    )? {
        if hit.record.affected_symbols.is_empty()
            && hit.record.symbol_changes.is_empty()
            && hit.record.path_changes.is_empty()
        {
            include(hit);
        }
    }
    for subject in subjects {
        for hit in store.search_authorized(
            &MemoryQuery {
                kinds: kinds.clone(),
                symbol: Some(subject.to_string()),
                limit: cap,
                ..MemoryQuery::default()
            },
            principal,
        )? {
            include(hit);
        }
    }
    let mut hits = by_id.into_values().collect::<Vec<_>>();
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| right.record.occurred_at.cmp(&left.record.occurred_at))
            .then_with(|| left.record.id.cmp(&right.record.id))
    });
    hits.truncate(cap);
    Ok(hits
        .into_iter()
        .map(|hit| {
            let record = hit.record;
            let title = sanitize_label(&record.title);
            let summary = sanitize_label(&record.summary);
            let artifact = sanitize_label(&record.sources[0].uri);
            let passed = record.verification.status == VerificationStatus::Passed;
            HistoricalConstraint {
                statement: format!("{title}: {summary}"),
                source: format!("memory:{} ({artifact})", record.id),
                strength: if record.kind == MemoryKind::Invariant
                    && passed
                    && record.confidence >= 0.8
                {
                    Strength::Must
                } else {
                    Strength::Should
                },
                confidence: if passed && record.confidence >= 0.8 {
                    ConfidenceBand::High
                } else if record.confidence >= 0.5 {
                    ConfidenceBand::Medium
                } else {
                    ConfidenceBand::Low
                },
            }
        })
        .collect())
}

#[derive(Debug, Error)]
pub enum ContractError {
    #[error("task must not be empty")]
    EmptyTask,
    #[error("no repository-local graph anchors matched the task")]
    NoAnchors,
    #[error("change contract integrity check failed")]
    Integrity,
    #[error("approved change contracts are immutable")]
    ApprovedImmutable,
    #[error("change contract revision overflow")]
    RevisionOverflow,
    #[error("invalid contract id: {0}")]
    InvalidId(String),
    #[error("contract not found: {0}")]
    NotFound(String),
    #[error("contract storage error: {0}")]
    Io(#[from] std::io::Error),
    #[error("contract JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Recover a deterministic contract from task retrieval, graph impact, and
/// caller-supplied durable historical constraints.
pub fn recover_contract(
    kg: &KnowledgeGraph,
    query_index: &QueryIndex,
    affected_index: &ReverseImpactIndex,
    task: &str,
    snapshot: ContractSnapshot,
    historical: &[HistoricalConstraint],
    options: &RecoveryOptions,
) -> Result<ChangeContract, ContractError> {
    let task = task.trim();
    if task.is_empty() {
        return Err(ContractError::EmptyTask);
    }
    let result = query_index.query(
        kg,
        task,
        options.max_anchors.saturating_mul(4).max(8),
        TraversalMode::Bfs,
    );
    let ranked_seeds: Vec<NodeId> = result
        .seeds
        .into_iter()
        .filter(|id| kg.node(id).is_some_and(|node| !node.source_file.is_empty()))
        .take(options.max_anchors.max(1))
        .collect();
    let production_seeds = ranked_seeds
        .iter()
        .filter(|id| kg.node(id).is_some_and(|node| !node.is_test()))
        .cloned()
        .collect::<Vec<_>>();
    let seeds = if production_seeds.is_empty() {
        ranked_seeds
    } else {
        production_seeds
    };
    if seeds.is_empty() {
        return Err(ContractError::NoAnchors);
    }

    let forecast = forecast_nodes_with_index(
        kg,
        affected_index,
        &seeds,
        &ForecastOptions {
            depth: options.depth,
            max_hits: options.max_hits,
            ..ForecastOptions::default()
        },
    );
    let mut anchors = forecast.changed_nodes;
    anchors.sort_by(node_ref_cmp);
    let mut expected_files = anchors
        .iter()
        .filter(|node| !node.file.is_empty())
        .map(|node| normalize_path(&node.file))
        .collect::<Vec<_>>();
    expected_files.sort();
    expected_files.dedup();
    if expected_files.is_empty() {
        return Err(ContractError::NoAnchors);
    }
    let mut protected_symbols = forecast.public_api_breaks;
    protected_symbols.sort_by(node_ref_cmp);

    let mut evidence = anchors
        .iter()
        .map(|anchor| EvidenceRef {
            id: stable_id("e-anchor", &[anchor.id.as_str()]),
            tier: EvidenceTier::Structural,
            source: anchor.file.clone(),
            summary: format!("Task-relevant graph anchor: {}", anchor.label),
        })
        .collect::<Vec<_>>();
    let anchor_evidence = evidence.iter().map(|item| item.id.clone()).collect();
    let scope_proof = ProofObligation {
        id: stable_id(
            "p-scope",
            &expected_files
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        ),
        kind: ProofKind::Scope,
        description: "Changed files stay within the recovered task scope".into(),
        targets: expected_files.clone(),
    };
    let mut proofs = vec![scope_proof.clone()];
    let mut requirements = vec![Requirement {
        id: stable_id("r-scope", &[task]),
        category: RequirementCategory::Scope,
        statement: "Keep the change centered on the recovered task anchors".into(),
        strength: Strength::Should,
        confidence: ConfidenceBand::Medium,
        evidence_ids: anchor_evidence,
        proof_ids: vec![scope_proof.id],
    }];

    if !forecast.at_risk_tests.is_empty() {
        let mut test_targets = forecast
            .at_risk_tests
            .iter()
            .map(|hit| normalize_path(&hit.file))
            .collect::<Vec<_>>();
        test_targets.sort();
        test_targets.dedup();
        let test_evidence = EvidenceRef {
            id: stable_id(
                "e-tests",
                &test_targets.iter().map(String::as_str).collect::<Vec<_>>(),
            ),
            tier: EvidenceTier::Executable,
            source: "synaptic reverse-impact index".into(),
            summary: format!("{} affected test file(s)", test_targets.len()),
        };
        let test_proof = ProofObligation {
            id: stable_id(
                "p-tests",
                &test_targets.iter().map(String::as_str).collect::<Vec<_>>(),
            ),
            kind: ProofKind::AffectedTests,
            description: "Run the affected tests and attest that they pass".into(),
            targets: test_targets,
        };
        requirements.push(Requirement {
            id: stable_id("r-tests", &[task]),
            category: RequirementCategory::Tests,
            statement: "All tests affected by the recovered anchors must pass".into(),
            strength: Strength::Must,
            confidence: ConfidenceBand::High,
            evidence_ids: vec![test_evidence.id.clone()],
            proof_ids: vec![test_proof.id.clone()],
        });
        evidence.push(test_evidence);
        proofs.push(test_proof);
    }

    if !protected_symbols.is_empty() {
        let protected_ids = protected_symbols
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>();
        let api_evidence = EvidenceRef {
            id: stable_id("e-api", &protected_ids),
            tier: EvidenceTier::Structural,
            source: "synaptic public visibility metadata".into(),
            summary: format!("{} public task anchor(s)", protected_symbols.len()),
        };
        let api_proof = ProofObligation {
            id: stable_id("p-api", &protected_ids),
            kind: ProofKind::PublicApiPresence,
            description: "Protected public symbols remain present in the current graph".into(),
            targets: protected_ids.iter().map(|id| (*id).to_string()).collect(),
        };
        requirements.push(Requirement {
            id: stable_id("r-api", &[task]),
            category: RequirementCategory::ApiCompatibility,
            statement:
                "Preserve recovered public API identities or revise and re-approve the contract"
                    .into(),
            strength: Strength::Must,
            confidence: ConfidenceBand::High,
            evidence_ids: vec![api_evidence.id.clone()],
            proof_ids: vec![api_proof.id.clone()],
        });
        evidence.push(api_evidence);
        proofs.push(api_proof);
    }

    let id = stable_id(
        "cc",
        &[
            snapshot.repository.as_str(),
            snapshot.base_revision.as_str(),
            task,
        ],
    );
    let mut contract = ChangeContract {
        schema_version: CHANGE_CONTRACT_SCHEMA_VERSION,
        id,
        revision: 1,
        state: ContractState::Draft,
        task: task.to_string(),
        snapshot,
        requirements,
        evidence,
        proofs,
        scope: ContractScope {
            anchors,
            expected_files,
            protected_symbols,
        },
        unknowns: vec![
            "Behavioral intent not represented by executable, structural, or historical evidence remains unknown"
                .into(),
            "Public API verification currently proves symbol presence, not signature compatibility".into(),
        ],
        contract_hash: String::new(),
    };
    contract.seal();
    contract.add_historical_constraints(historical)?;
    Ok(contract)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationInput {
    pub base_revision: String,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub passed_proofs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofStatus {
    Passed,
    Failed,
    Unproven,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationState {
    Satisfied,
    SatisfiedWithWarnings,
    NeedsClarification,
    Violated,
    Stale,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofResult {
    pub proof_id: String,
    pub status: ProofStatus,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationReport {
    pub contract_id: String,
    pub contract_revision: u32,
    pub state: VerificationState,
    pub proofs: Vec<ProofResult>,
    pub warnings: Vec<String>,
    pub summary: String,
}

pub fn verify_contract(
    contract: &ChangeContract,
    kg: &KnowledgeGraph,
    input: &VerificationInput,
) -> VerificationReport {
    if !contract.verify_hash() {
        return terminal_report(
            contract,
            VerificationState::Invalid,
            "Contract hash is invalid",
        );
    }
    if input.base_revision != contract.snapshot.base_revision {
        return terminal_report(
            contract,
            VerificationState::Stale,
            "Verification base does not match the contract snapshot",
        );
    }

    let passed: HashSet<&str> = input.passed_proofs.iter().map(String::as_str).collect();
    let changed: HashSet<String> = input
        .changed_files
        .iter()
        .map(|path| normalize_path(path))
        .collect();
    let expected: HashSet<String> = contract
        .scope
        .expected_files
        .iter()
        .map(|path| normalize_path(path))
        .collect();
    let mut results = Vec::with_capacity(contract.proofs.len());
    for proof in &contract.proofs {
        let (status, detail) = match proof.kind {
            ProofKind::Scope => {
                let mut unexpected = changed.difference(&expected).cloned().collect::<Vec<_>>();
                unexpected.sort();
                if unexpected.is_empty() {
                    (
                        ProofStatus::Passed,
                        "Changed files are within recovered scope".into(),
                    )
                } else {
                    (
                        ProofStatus::Failed,
                        format!("Files outside recovered scope: {}", unexpected.join(", ")),
                    )
                }
            }
            ProofKind::PublicApiPresence => {
                let mut missing = proof
                    .targets
                    .iter()
                    .filter(|id| !kg.contains_node(&NodeId((*id).clone())))
                    .cloned()
                    .collect::<Vec<_>>();
                missing.sort();
                if missing.is_empty() {
                    (
                        ProofStatus::Passed,
                        "Protected public symbols remain present".into(),
                    )
                } else {
                    (
                        ProofStatus::Failed,
                        format!(
                            "Protected public symbols are missing: {}",
                            missing.join(", ")
                        ),
                    )
                }
            }
            ProofKind::AffectedTests | ProofKind::ManualAttestation => {
                if passed.contains(proof.id.as_str()) {
                    (
                        ProofStatus::Passed,
                        "Caller supplied a passing attestation".into(),
                    )
                } else {
                    (
                        ProofStatus::Unproven,
                        "No passing attestation was supplied".into(),
                    )
                }
            }
        };
        results.push(ProofResult {
            proof_id: proof.id.clone(),
            status,
            detail,
        });
    }

    let statuses: HashMap<&str, ProofStatus> = results
        .iter()
        .map(|result| (result.proof_id.as_str(), result.status))
        .collect();
    let mut state = if contract.state == ContractState::Approved {
        VerificationState::Satisfied
    } else {
        VerificationState::NeedsClarification
    };
    let mut warnings = if contract.state == ContractState::Draft {
        vec!["Contract is still a draft and must be explicitly approved".into()]
    } else {
        Vec::new()
    };
    for requirement in &contract.requirements {
        for proof_id in &requirement.proof_ids {
            let status = statuses
                .get(proof_id.as_str())
                .copied()
                .unwrap_or(ProofStatus::Unproven);
            match (requirement.strength, status) {
                (Strength::Must, ProofStatus::Failed) => state = VerificationState::Violated,
                (Strength::Must, ProofStatus::Unproven) if state != VerificationState::Violated => {
                    state = VerificationState::NeedsClarification
                }
                (
                    Strength::Should | Strength::Observe,
                    ProofStatus::Failed | ProofStatus::Unproven,
                ) => {
                    warnings.push(format!("{}: {}", requirement.id, requirement.statement));
                }
                _ => {}
            }
        }
    }
    if state == VerificationState::Satisfied && !warnings.is_empty() {
        state = VerificationState::SatisfiedWithWarnings;
    }
    let summary = match state {
        VerificationState::Satisfied => "Approved contract satisfied".into(),
        VerificationState::SatisfiedWithWarnings => {
            "Approved contract satisfied with warnings".into()
        }
        VerificationState::NeedsClarification => "Required proof or approval is missing".into(),
        VerificationState::Violated => "One or more MUST requirements are violated".into(),
        VerificationState::Stale => "Contract was recovered from a different base revision".into(),
        VerificationState::Invalid => "Contract integrity validation failed".into(),
    };
    VerificationReport {
        contract_id: contract.id.clone(),
        contract_revision: contract.revision,
        state,
        proofs: results,
        warnings,
        summary,
    }
}

fn terminal_report(
    contract: &ChangeContract,
    state: VerificationState,
    summary: &str,
) -> VerificationReport {
    VerificationReport {
        contract_id: contract.id.clone(),
        contract_revision: contract.revision,
        state,
        proofs: Vec::new(),
        warnings: Vec::new(),
        summary: summary.into(),
    }
}

#[derive(Debug, Clone)]
pub struct ContractStore {
    root: PathBuf,
}

impl ContractStore {
    pub fn under(repository_root: impl AsRef<Path>) -> Self {
        Self {
            root: repository_root.as_ref().join(".synaptic").join("contracts"),
        }
    }

    pub fn save(&self, contract: &ChangeContract) -> Result<PathBuf, ContractError> {
        if !contract.verify_hash() {
            return Err(ContractError::Integrity);
        }
        validate_id(&contract.id)?;
        let dir = self.root.join(&contract.id);
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("v{}.json", contract.revision));
        let bytes = serde_json::to_vec_pretty(contract)?;
        if path.exists() {
            if fs::read(&path)? == bytes {
                return Ok(path);
            }
            return Err(ContractError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "immutable contract version already exists: {}",
                    path.display()
                ),
            )));
        }
        let tmp = dir.join(format!(
            ".v{}.{}.tmp",
            contract.revision,
            std::process::id()
        ));
        fs::write(&tmp, &bytes)?;
        match fs::rename(&tmp, &path) {
            Ok(()) => Ok(path),
            Err(error) => {
                let _ = fs::remove_file(&tmp);
                Err(ContractError::Io(error))
            }
        }
    }

    /// Place a newly recovered draft after the latest immutable revision with
    /// the same deterministic contract id.
    pub fn prepare_revision(
        &self,
        mut contract: ChangeContract,
    ) -> Result<ChangeContract, ContractError> {
        contract.ensure_mutable()?;
        match self.load_latest(&contract.id) {
            Ok(previous) => {
                contract.revision = previous
                    .revision
                    .checked_add(1)
                    .ok_or(ContractError::RevisionOverflow)?;
                contract.seal();
            }
            Err(ContractError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        Ok(contract)
    }

    pub fn load_latest(&self, id: &str) -> Result<ChangeContract, ContractError> {
        validate_id(id)?;
        let dir = self.root.join(id);
        let mut versions = fs::read_dir(&dir)
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => ContractError::NotFound(id.into()),
                _ => ContractError::Io(error),
            })?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_str()?;
                let version = name
                    .strip_prefix('v')?
                    .strip_suffix(".json")?
                    .parse::<u32>()
                    .ok()?;
                Some((version, entry.path()))
            })
            .collect::<Vec<_>>();
        versions.sort_by_key(|(version, _)| *version);
        let path = versions
            .last()
            .map(|(_, path)| path)
            .ok_or_else(|| ContractError::NotFound(id.into()))?;
        Self::load_path(path)
    }

    pub fn load_path(path: impl AsRef<Path>) -> Result<ChangeContract, ContractError> {
        let contract: ChangeContract = serde_json::from_slice(&fs::read(path)?)?;
        if contract.schema_version != CHANGE_CONTRACT_SCHEMA_VERSION || !contract.verify_hash() {
            return Err(ContractError::Integrity);
        }
        Ok(contract)
    }
}

fn validate_id(id: &str) -> Result<(), ContractError> {
    if !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Ok(())
    } else {
        Err(ContractError::InvalidId(id.into()))
    }
}

fn stable_id(prefix: &str, parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(&[0]);
    }
    format!("{prefix}-{}", &hasher.finalize().to_hex()[..12])
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn node_ref_cmp(left: &NodeRef, right: &NodeRef) -> std::cmp::Ordering {
    left.file
        .cmp(&right.file)
        .then_with(|| left.label.cmp(&right.label))
        .then_with(|| left.id.cmp(&right.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use synaptic_core::GraphData;

    fn fixture() -> KnowledgeGraph {
        let data: GraphData = serde_json::from_value(json!({
            "directed": true,
            "nodes": [
                {"id":"login", "label":"login", "file_type":"code", "source_file":"src/login.rs", "kind":"function", "visibility":"public"},
                {"id":"login_test", "label":"login works", "file_type":"code", "source_file":"tests/login.rs", "kind":"function", "_is_test":true}
            ],
            "links": [
                {"source":"login_test", "target":"login", "relation":"calls", "confidence":"EXTRACTED", "source_file":"tests/login.rs", "weight":1.0}
            ],
            "built_at_commit": "base-1"
        }))
        .unwrap();
        KnowledgeGraph::from_graph_data(data)
    }

    fn recover() -> ChangeContract {
        let kg = fixture();
        recover_contract(
            &kg,
            &QueryIndex::build(&kg),
            &ReverseImpactIndex::build(&kg, synaptic_query::DEFAULT_AFFECTED_RELATIONS),
            "change login behavior",
            ContractSnapshot {
                repository: "fixture".into(),
                base_revision: "base-1".into(),
                graph_revision: Some("base-1".into()),
            },
            &[],
            &RecoveryOptions::default(),
        )
        .unwrap()
    }

    #[test]
    fn recovery_is_deterministic_and_verification_is_fail_closed() {
        let draft = recover();
        assert!(draft.verify_hash());
        assert_eq!(draft, recover());
        assert!(
            draft
                .scope
                .protected_symbols
                .iter()
                .any(|node| node.id == "login")
        );
        let test_proof = draft
            .proofs
            .iter()
            .find(|proof| proof.kind == ProofKind::AffectedTests)
            .unwrap()
            .id
            .clone();

        let approved = draft.approve().unwrap();
        let missing = verify_contract(
            &approved,
            &fixture(),
            &VerificationInput {
                base_revision: "base-1".into(),
                changed_files: vec!["src/login.rs".into()],
                passed_proofs: Vec::new(),
            },
        );
        assert_eq!(missing.state, VerificationState::NeedsClarification);

        let satisfied = verify_contract(
            &approved,
            &fixture(),
            &VerificationInput {
                base_revision: "base-1".into(),
                changed_files: vec!["src/login.rs".into()],
                passed_proofs: vec![test_proof],
            },
        );
        assert_eq!(satisfied.state, VerificationState::Satisfied);
    }

    #[test]
    fn storage_is_versioned_and_tampering_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let store = ContractStore::under(root.path());
        let draft = recover();
        let draft_path = store.save(&draft).unwrap();
        let approved = draft.approve().unwrap();
        let approved_path = store.save(&approved).unwrap();
        assert_ne!(draft_path, approved_path);
        assert_eq!(store.save(&approved).unwrap(), approved_path);
        assert_eq!(store.load_latest(&approved.id).unwrap(), approved);

        let next_draft = store.prepare_revision(recover()).unwrap();
        assert_eq!(next_draft.revision, 3);
        store.save(&next_draft).unwrap();
        let next_approved = next_draft.approve().unwrap();
        assert_eq!(next_approved.revision, 4);
        store.save(&next_approved).unwrap();
        assert_eq!(store.load_latest(&approved.id).unwrap(), next_approved);

        let mut tampered = next_approved;
        tampered.task.push_str(" altered");
        assert!(matches!(
            store.save(&tampered),
            Err(ContractError::Integrity)
        ));
    }

    #[test]
    fn recovery_rejects_fileless_external_scope() {
        let data: GraphData = serde_json::from_value(json!({
            "directed": true,
            "nodes": [{
                "id":"external_router",
                "label":"Router middleware dispatch",
                "file_type":"code",
                "source_file":"",
                "kind":"function"
            }],
            "links": []
        }))
        .unwrap();
        let kg = KnowledgeGraph::from_graph_data(data);
        assert!(matches!(
            recover_contract(
                &kg,
                &QueryIndex::build(&kg),
                &ReverseImpactIndex::build(&kg, synaptic_query::DEFAULT_AFFECTED_RELATIONS),
                "change Router middleware dispatch",
                ContractSnapshot {
                    repository: "fixture".into(),
                    base_revision: "base-1".into(),
                    graph_revision: None,
                },
                &[],
                &RecoveryOptions::default(),
            ),
            Err(ContractError::NoAnchors)
        ));
    }

    #[test]
    fn active_grounded_memory_is_reused_without_promoting_stale_records() {
        let root = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(root.path());
        let mut invariant = synaptic_memory::MemoryRecord::new(
            "login-invariant",
            MemoryKind::Invariant,
            "Preserve login replay protection",
            "A token may only be exchanged once",
            "fixture",
            2,
            vec![synaptic_memory::SourceArtifact {
                kind: "adr".into(),
                uri: "docs/adr/login.md".into(),
                revision: Some("base-1".into()),
                digest: None,
            }],
        );
        invariant.affected_symbols = vec![synaptic_memory::SymbolAnchor {
            node_id: "login".into(),
            label: "login".into(),
            source_file: "src/login.rs".into(),
            repo: None,
            commit: Some("base-1".into()),
            confidence: 1.0,
        }];
        invariant.confidence = 0.9;
        invariant.verification.status = VerificationStatus::Passed;
        store.record(&invariant).unwrap();

        let global = synaptic_memory::MemoryRecord::new(
            "repository-convention",
            MemoryKind::Convention,
            "Follow the repository contribution policy",
            "All changes require the documented review workflow",
            "fixture",
            3,
            vec![synaptic_memory::SourceArtifact {
                kind: "convention".into(),
                uri: "CONTRIBUTING.md".into(),
                revision: Some("base-1".into()),
                digest: None,
            }],
        );
        store.record(&global).unwrap();

        let mut unrelated = invariant.clone();
        unrelated.idempotency_key = "billing-invariant".into();
        unrelated.id = "mem_billing_invariant".into();
        unrelated.title = "Preserve billing behavior".into();
        unrelated.affected_symbols[0].node_id = "billing".into();
        unrelated.affected_symbols[0].label = "billing".into();
        unrelated.affected_symbols[0].source_file = "src/billing.rs".into();
        store.record(&unrelated).unwrap();

        let mut stale = invariant.clone();
        stale.idempotency_key = "old-login-adr".into();
        stale.id = "mem_stale_login".into();
        stale.kind = MemoryKind::ArchitectureDecision;
        stale.lifecycle = MemoryLifecycle::Resolved;
        store.record(&stale).unwrap();

        let mut draft = recover();
        let historical = historical_constraints_from_memory(
            &store,
            &MemoryPrincipal::operator(),
            &draft.scope.anchors,
            8,
        )
        .unwrap();
        assert_eq!(historical.len(), 2);
        let invariant = historical
            .iter()
            .find(|item| item.statement.contains("replay protection"))
            .unwrap();
        assert_eq!(invariant.strength, Strength::Must);
        assert_eq!(invariant.confidence, ConfidenceBand::High);
        assert!(historical.iter().any(|item| {
            item.statement.contains("repository contribution policy")
                && item.strength == Strength::Should
        }));
        assert!(
            !historical
                .iter()
                .any(|item| item.statement.contains("billing"))
        );
        draft.add_historical_constraints(&historical).unwrap();
        assert!(draft.verify_hash());
        assert!(draft.requirements.iter().any(|requirement| {
            requirement.category == RequirementCategory::HistoricalInvariant
                && requirement.statement.contains("replay protection")
        }));
        assert!(matches!(
            draft
                .approve()
                .unwrap()
                .add_historical_constraints(&historical),
            Err(ContractError::ApprovedImmutable)
        ));
    }
}
