use serde::{Deserialize, Serialize};

use crate::{RuntimeEvidenceReport, RuntimeSurfaceEvidence, RuntimeSurfaceKind};

const MAX_BEHAVIORAL_EVIDENCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_BEHAVIORAL_OBSERVATIONS: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BehavioralOutcome {
    Success,
    ClientError,
    ServerError,
    Timeout,
    ContractViolation,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehavioralObservation {
    pub id: String,
    pub kind: RuntimeSurfaceKind,
    pub protocol: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    pub outcome: BehavioralOutcome,
    pub occurrences: usize,
    pub evidence_digest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BehavioralRegressionCandidate {
    pub observation_id: String,
    pub summary: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BehavioralEvidenceReport {
    pub version: u32,
    pub origin: String,
    pub environment: String,
    pub window_start_unix_nano: u64,
    pub window_end_unix_nano: u64,
    pub complete_window: bool,
    pub observations: Vec<BehavioralObservation>,
    pub review_candidates: Vec<BehavioralRegressionCandidate>,
}

impl BehavioralEvidenceReport {
    pub const VERSION: u32 = 1;

    pub fn as_runtime_evidence(&self) -> RuntimeEvidenceReport {
        RuntimeEvidenceReport {
            version: 1,
            origin: self.origin.clone(),
            environment: Some(self.environment.clone()),
            window_start_unix_nano: Some(self.window_start_unix_nano),
            window_end_unix_nano: Some(self.window_end_unix_nano),
            complete_window: self.complete_window,
            spans_scanned: self.observations.len(),
            rejected_spans: 0,
            observations: self
                .observations
                .iter()
                .map(|observation| RuntimeSurfaceEvidence {
                    id: format!("runtime_surface_{}", &observation.evidence_digest[..24]),
                    kind: observation.kind,
                    protocol: observation.protocol.clone(),
                    method: observation.method.clone(),
                    authority: observation.authority.clone(),
                    path: observation.path.clone(),
                    service: observation.service.clone(),
                    operation: observation.operation.clone(),
                    source_file: None,
                    source_line: None,
                    source_function: None,
                    evidence_digest: observation.evidence_digest.clone(),
                    occurrences: observation.occurrences,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReport {
    version: u32,
    environment: String,
    window_start_unix_nano: u64,
    window_end_unix_nano: u64,
    observations: Vec<RawObservation>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawObservation {
    kind: RuntimeSurfaceKind,
    protocol: String,
    method: String,
    #[serde(default)]
    authority: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    operation: Option<String>,
    outcome: BehavioralOutcome,
    occurrences: usize,
}

/// Import a redacted synthetic/canary or error-telemetry summary. This boundary
/// deliberately rejects arbitrary attributes, headers, payloads, and query data.
/// Failures become review candidates and never authorize repair by themselves.
pub fn import_behavioral_evidence(
    origin: &str,
    bytes: &[u8],
) -> Result<BehavioralEvidenceReport, BehavioralEvidenceError> {
    if bytes.len() > MAX_BEHAVIORAL_EVIDENCE_BYTES {
        return Err(BehavioralEvidenceError::TooLarge(bytes.len()));
    }
    let raw: RawReport = serde_json::from_slice(bytes)?;
    if raw.version != BehavioralEvidenceReport::VERSION {
        return Err(BehavioralEvidenceError::UnsupportedVersion(raw.version));
    }
    let environment = bounded_coordinate(&raw.environment, "environment")?;
    if raw.window_start_unix_nano > raw.window_end_unix_nano {
        return Err(BehavioralEvidenceError::InvalidWindow);
    }
    if raw.observations.len() > MAX_BEHAVIORAL_OBSERVATIONS {
        return Err(BehavioralEvidenceError::TooManyObservations(
            raw.observations.len(),
        ));
    }

    let mut observations = Vec::with_capacity(raw.observations.len());
    for observed in raw.observations {
        let protocol = bounded_coordinate(&observed.protocol, "protocol")?.to_ascii_lowercase();
        let method = bounded_coordinate(&observed.method, "method")?.to_ascii_uppercase();
        if observed.occurrences == 0 || observed.occurrences > 1_000_000 {
            return Err(BehavioralEvidenceError::InvalidCoordinate(
                "occurrences".into(),
            ));
        }
        let authority = observed
            .authority
            .as_deref()
            .map(sanitize_authority)
            .transpose()?;
        let path = observed.path.as_deref().map(sanitize_path);
        let service = observed
            .service
            .as_deref()
            .map(|value| bounded_coordinate(value, "service"))
            .transpose()?;
        let operation = observed
            .operation
            .as_deref()
            .map(|value| bounded_coordinate(value, "operation"))
            .transpose()?;
        let valid_shape = match observed.kind {
            RuntimeSurfaceKind::Http => authority.is_some() || path.is_some(),
            RuntimeSurfaceKind::Rpc => service.is_some() && operation.is_some(),
            RuntimeSurfaceKind::Message => authority.is_some() || operation.is_some(),
        };
        if !valid_shape {
            return Err(BehavioralEvidenceError::InvalidCoordinate(
                "surface shape".into(),
            ));
        }
        let identity = serde_json::to_vec(&(
            observed.kind,
            &protocol,
            &method,
            &authority,
            &path,
            &service,
            &operation,
            observed.outcome,
        ))?;
        let evidence_digest = blake3::hash(&identity).to_hex().to_string();
        observations.push(BehavioralObservation {
            id: format!("behavioral_observation_{}", &evidence_digest[..24]),
            kind: observed.kind,
            protocol,
            method,
            authority,
            path,
            service,
            operation,
            outcome: observed.outcome,
            occurrences: observed.occurrences,
            evidence_digest,
        });
    }
    observations.sort_by(|left, right| left.id.cmp(&right.id));
    let review_candidates = observations
        .iter()
        .filter(|observation| {
            observation.outcome != BehavioralOutcome::Success && observation.occurrences >= 2
        })
        .map(|observation| BehavioralRegressionCandidate {
            observation_id: observation.id.clone(),
            summary: format!(
                "{:?} observed {} times for {} {}",
                observation.outcome,
                observation.occurrences,
                observation.method,
                observation
                    .path
                    .as_deref()
                    .or(observation.operation.as_deref())
                    .unwrap_or("<surface>")
            ),
            confidence: 1.0,
        })
        .collect();
    Ok(BehavioralEvidenceReport {
        version: BehavioralEvidenceReport::VERSION,
        origin: origin.replace('\\', "/"),
        environment,
        window_start_unix_nano: raw.window_start_unix_nano,
        window_end_unix_nano: raw.window_end_unix_nano,
        complete_window: true,
        observations,
        review_candidates,
    })
}

fn bounded_coordinate(value: &str, field: &str) -> Result<String, BehavioralEvidenceError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 512
        || value.chars().any(char::is_control)
        || value.contains(['?', '#'])
    {
        return Err(BehavioralEvidenceError::InvalidCoordinate(field.into()));
    }
    Ok(value.into())
}

fn sanitize_authority(value: &str) -> Result<String, BehavioralEvidenceError> {
    let value = bounded_coordinate(value, "authority")?.to_ascii_lowercase();
    if value.contains('@') || value.contains('/') {
        return Err(BehavioralEvidenceError::InvalidCoordinate(
            "authority".into(),
        ));
    }
    Ok(value)
}

fn sanitize_path(value: &str) -> String {
    let path = value.split(['?', '#']).next().unwrap_or("/");
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            if segment.chars().all(|character| character.is_ascii_digit())
                || (segment.len() >= 16
                    && segment
                        .chars()
                        .all(|character| character.is_ascii_hexdigit() || character == '-'))
            {
                ":id"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>();
    format!("/{}", segments.join("/"))
}

#[derive(Debug, thiserror::Error)]
pub enum BehavioralEvidenceError {
    #[error("behavioral evidence exceeds the {MAX_BEHAVIORAL_EVIDENCE_BYTES}-byte cap: {0}")]
    TooLarge(usize),
    #[error("behavioral evidence exceeds the {MAX_BEHAVIORAL_OBSERVATIONS}-observation cap: {0}")]
    TooManyObservations(usize),
    #[error("unsupported behavioral evidence version {0}")]
    UnsupportedVersion(u32),
    #[error("behavioral evidence window ends before it starts")]
    InvalidWindow,
    #[error("invalid behavioral evidence coordinate: {0}")]
    InvalidCoordinate(String),
    #[error("invalid behavioral evidence JSON: {0}")]
    Json(#[from] serde_json::Error),
}
