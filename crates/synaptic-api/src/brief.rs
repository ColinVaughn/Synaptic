use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use synaptic_core::NodeId;
use synaptic_graph::KnowledgeGraph;
use synaptic_query::{DEFAULT_AFFECTED_RELATIONS, affected_nodes_multi};

use crate::{
    ApiChangeEvent, ApiUsageBinding, ApplicabilityState, EvidenceSpan, RelevanceAssessment,
    redaction::redact_sensitive_text,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefBudget {
    pub max_files: usize,
    pub max_source_bytes: usize,
    pub max_impact_nodes: usize,
    pub max_evidence_chars: usize,
}

impl Default for BriefBudget {
    fn default() -> Self {
        Self {
            max_files: 12,
            max_source_bytes: 48_000,
            max_impact_nodes: 100,
            max_evidence_chars: 8_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSlice {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEvidence {
    pub kind: String,
    pub summary: String,
    pub source: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationRequirement {
    pub gate: String,
    pub required: bool,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiImpactHit {
    pub id: String,
    pub label: String,
    pub file: String,
    pub depth: usize,
    pub via_relation: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub community: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub repository: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiImpactForecast {
    pub version: u32,
    pub seed_node_ids: Vec<String>,
    pub blast_radius: Vec<ApiImpactHit>,
    pub blast_radius_total: usize,
    pub at_risk_tests: Vec<ApiImpactHit>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepairBrief {
    pub version: u32,
    pub id: String,
    pub repository_identity: String,
    pub base_sha: String,
    pub event: ApiChangeEvent,
    pub applicability: RelevanceAssessment,
    pub usage_bindings: Vec<ApiUsageBinding>,
    pub impact: ApiImpactForecast,
    pub official_evidence: Vec<EvidenceSpan>,
    pub source_slices: Vec<SourceSlice>,
    pub memory: Vec<MemoryEvidence>,
    pub dynamic_hazards: Vec<String>,
    pub allowed_files: Vec<String>,
    pub required_tests: Vec<String>,
    pub verification: Vec<VerificationRequirement>,
}

pub struct RepairBriefRequest<'a> {
    pub repository_root: &'a Path,
    pub repository_identity: &'a str,
    pub base_sha: &'a str,
    pub event: &'a ApiChangeEvent,
    pub assessment: &'a RelevanceAssessment,
    pub graph: &'a KnowledgeGraph,
    pub memory: &'a [MemoryEvidence],
    pub budget: &'a BriefBudget,
}

pub fn build_repair_brief(request: RepairBriefRequest<'_>) -> Result<RepairBrief, BriefError> {
    let RepairBriefRequest {
        repository_root,
        repository_identity,
        base_sha,
        event,
        assessment,
        graph,
        memory,
        budget,
    } = request;
    if assessment.state != ApplicabilityState::Applicable {
        return Err(BriefError::NotApplicable(assessment.state));
    }
    if budget.max_files == 0 || budget.max_source_bytes == 0 || budget.max_impact_nodes == 0 {
        return Err(BriefError::InvalidBudget);
    }
    let seeds = assessment
        .seed_node_ids
        .iter()
        .map(|id| NodeId(id.clone()))
        .collect::<Vec<_>>();
    let impact = impact_from_nodes(graph, &seeds, budget.max_impact_nodes);

    let mut allowed_files = Vec::new();
    for file in assessment
        .bindings
        .iter()
        .map(|binding| binding.source_file.as_str())
        .chain(impact.at_risk_tests.iter().map(|hit| hit.file.as_str()))
        .chain(impact.blast_radius.iter().map(|hit| hit.file.as_str()))
    {
        push_unique_file(&mut allowed_files, file, budget.max_files);
    }
    let mut required_tests = impact
        .at_risk_tests
        .iter()
        .map(|hit| hit.file.clone())
        .filter(|file| !file.is_empty())
        .collect::<Vec<_>>();
    required_tests.sort();
    required_tests.dedup();

    let source_slices = read_slices(repository_root, &allowed_files, budget.max_source_bytes)?;
    let official_evidence = bounded_evidence(event, budget.max_evidence_chars);
    let memory = bounded_memory(memory, budget.max_evidence_chars);
    let dynamic_hazards = impact
        .blast_radius
        .iter()
        .filter_map(|hit| graph.node(&NodeId(hit.id.clone())))
        .flat_map(|node| {
            node.dynamic_sites()
                .into_iter()
                .filter_map(|site| serde_json::to_string(&site).ok())
        })
        .take(50)
        .collect();
    let identity = serde_json::to_vec(&(
        repository_identity,
        base_sha,
        &event.id,
        &allowed_files,
        budget,
    ))?;
    let digest = blake3::hash(&identity).to_hex().to_string();
    Ok(RepairBrief {
        version: 1,
        id: format!("api_run_{}", &digest[..24]),
        repository_identity: repository_identity.into(),
        base_sha: base_sha.into(),
        event: event.clone(),
        applicability: assessment.clone(),
        usage_bindings: assessment.bindings.clone(),
        impact,
        official_evidence,
        source_slices,
        memory,
        dynamic_hazards,
        allowed_files,
        required_tests,
        verification: verification_requirements(),
    })
}

pub fn impact_from_nodes(
    graph: &KnowledgeGraph,
    seeds: &[NodeId],
    max_hits: usize,
) -> ApiImpactForecast {
    let relations = DEFAULT_AFFECTED_RELATIONS.to_vec();
    let hits = affected_nodes_multi(graph, seeds, &relations, 4);
    let mut resolved = hits
        .into_iter()
        .filter_map(|hit| {
            graph.node(&hit.node_id).map(|node| {
                (
                    ApiImpactHit {
                        id: node.id.0.clone(),
                        label: node.label.clone(),
                        file: node.source_file.to_string(),
                        depth: hit.depth,
                        via_relation: hit.via_relation,
                        community: node.community,
                        repository: node.repo.clone(),
                    },
                    node.is_test(),
                )
            })
        })
        .collect::<Vec<_>>();
    resolved.sort_by(|a, b| {
        a.0.depth
            .cmp(&b.0.depth)
            .then_with(|| a.0.file.cmp(&b.0.file))
            .then_with(|| a.0.id.cmp(&b.0.id))
    });
    let at_risk_tests = resolved
        .iter()
        .filter(|(_, is_test)| *is_test)
        .map(|(hit, _)| hit.clone())
        .collect();
    let blast_radius_total = resolved.len();
    let mut blast_radius = resolved.into_iter().map(|(hit, _)| hit).collect::<Vec<_>>();
    blast_radius.truncate(max_hits);
    let mut seed_node_ids = seeds.iter().map(|seed| seed.0.clone()).collect::<Vec<_>>();
    seed_node_ids.sort();
    seed_node_ids.dedup();
    ApiImpactForecast {
        version: 1,
        seed_node_ids,
        blast_radius,
        blast_radius_total,
        at_risk_tests,
    }
}

fn push_unique_file(files: &mut Vec<String>, file: &str, maximum: usize) {
    let file = file.replace('\\', "/");
    if !file.is_empty() && files.len() < maximum && !files.contains(&file) {
        files.push(file);
    }
}

fn read_slices(
    root: &Path,
    files: &[String],
    max_bytes: usize,
) -> Result<Vec<SourceSlice>, BriefError> {
    let root = root.canonicalize()?;
    let mut remaining = max_bytes;
    let mut slices = Vec::new();
    for file in files {
        if remaining == 0 {
            break;
        }
        let relative = Path::new(file);
        if relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(BriefError::UnsafePath(file.clone()));
        }
        let path = root.join(relative);
        let canonical = match path.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(BriefError::Io(error)),
        };
        if !canonical.starts_with(&root) || !canonical.is_file() {
            return Err(BriefError::UnsafePath(file.clone()));
        }
        let bytes = fs::read(canonical)?;
        let take = remaining.min(bytes.len());
        let raw_content = String::from_utf8_lossy(&bytes[..take]).into_owned();
        let digest = blake3::hash(raw_content.as_bytes()).to_hex().to_string();
        let content = redact_sensitive_text(&raw_content);
        remaining -= take;
        slices.push(SourceSlice {
            file: file.clone(),
            start_line: 1,
            end_line: content.lines().count().max(1),
            digest,
            content,
        });
    }
    Ok(slices)
}

fn bounded_evidence(event: &ApiChangeEvent, maximum: usize) -> Vec<EvidenceSpan> {
    let mut remaining = maximum;
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for evidence in event.changes.iter().flat_map(|change| &change.evidence) {
        if remaining == 0 || !seen.insert(evidence.digest.clone()) {
            continue;
        }
        let mut evidence = evidence.clone();
        evidence.summary = evidence.summary.chars().take(remaining).collect();
        evidence.summary = redact_sensitive_text(&evidence.summary);
        remaining = remaining.saturating_sub(evidence.summary.chars().count());
        result.push(evidence);
    }
    result
}

fn bounded_memory(memory: &[MemoryEvidence], maximum: usize) -> Vec<MemoryEvidence> {
    let mut remaining = maximum;
    memory
        .iter()
        .take(20)
        .map_while(|item| {
            if remaining == 0 {
                return None;
            }
            let mut item = item.clone();
            item.summary = item.summary.chars().take(remaining).collect();
            item.summary = redact_sensitive_text(&item.summary);
            remaining = remaining.saturating_sub(item.summary.chars().count());
            Some(item)
        })
        .collect()
}

fn verification_requirements() -> Vec<VerificationRequirement> {
    [
        (
            "patch_integrity",
            "patch applies to the pinned base and stays in policy",
        ),
        (
            "api_usage_invariants",
            "deprecated bindings are removed and replacements are present",
        ),
        (
            "selected_tests_and_build",
            "graph-selected tests and the detected build pass",
        ),
        (
            "repository_policy",
            "configured lint, schema, integration, and security checks pass",
        ),
        (
            "final_forecast",
            "no new cycle, removed public API, or excessive risk is introduced",
        ),
    ]
    .into_iter()
    .map(|(gate, description)| VerificationRequirement {
        gate: gate.into(),
        required: true,
        description: description.into(),
    })
    .collect()
}

#[derive(Debug, thiserror::Error)]
pub enum BriefError {
    #[error("repair brief requires an applicable event, got {0:?}")]
    NotApplicable(ApplicabilityState),
    #[error("repair brief budget values must be positive")]
    InvalidBudget,
    #[error("repair context path is unsafe: {0}")]
    UnsafePath(String),
    #[error("repair context I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("repair brief serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}
