use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_RUNTIME_EVIDENCE_BYTES: usize = 32 * 1024 * 1024;
const MAX_SPANS: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSurfaceKind {
    Http,
    Rpc,
    Message,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeSurfaceEvidence {
    pub id: String,
    pub kind: RuntimeSurfaceKind,
    pub protocol: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_function: Option<String>,
    pub evidence_digest: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeEvidenceReport {
    pub version: u32,
    pub origin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_start_unix_nano: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_end_unix_nano: Option<u64>,
    pub complete_window: bool,
    pub spans_scanned: usize,
    pub rejected_spans: usize,
    pub observations: Vec<RuntimeSurfaceEvidence>,
}

/// Import the OTLP JSON shape without retaining arbitrary attributes. Only the
/// allow-listed, low-cardinality API coordinates below survive serialization.
pub fn import_runtime_evidence(
    origin: &str,
    bytes: &[u8],
) -> Result<RuntimeEvidenceReport, RuntimeEvidenceError> {
    if bytes.len() > MAX_RUNTIME_EVIDENCE_BYTES {
        return Err(RuntimeEvidenceError::TooLarge(bytes.len()));
    }
    let root: Value = serde_json::from_slice(bytes)?;
    let resource_spans = root
        .get("resourceSpans")
        .and_then(Value::as_array)
        .ok_or(RuntimeEvidenceError::NotOtlp)?;
    let mut environment = BTreeSet::new();
    let mut window_start = None;
    let mut window_end = None;
    let mut spans_scanned = 0;
    let mut rejected_spans = 0;
    let mut observations = Vec::new();

    for resource_span in resource_spans {
        let resource_attributes = resource_span
            .pointer("/resource/attributes")
            .map(attribute_map)
            .unwrap_or_default();
        if let Some(value) = resource_attributes
            .get("deployment.environment.name")
            .or_else(|| resource_attributes.get("deployment.environment"))
        {
            environment.insert(value.clone());
        }
        for scope in ["scopeSpans", "instrumentationLibrarySpans"] {
            for scope_span in resource_span
                .get(scope)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                for span in scope_span
                    .get("spans")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    spans_scanned += 1;
                    if spans_scanned > MAX_SPANS {
                        return Err(RuntimeEvidenceError::TooManySpans(spans_scanned));
                    }
                    let start = integer_field(span, "startTimeUnixNano");
                    let end = integer_field(span, "endTimeUnixNano");
                    if let Some(start) = start {
                        window_start =
                            Some(window_start.map_or(start, |prior: u64| prior.min(start)));
                    }
                    if let Some(end) = end {
                        window_end = Some(window_end.map_or(end, |prior: u64| prior.max(end)));
                    }
                    let attributes = span
                        .get("attributes")
                        .map(attribute_map)
                        .unwrap_or_default();
                    if let Some(observation) = sanitize_span(&attributes) {
                        observations.push(observation);
                    } else {
                        rejected_spans += 1;
                    }
                }
            }
        }
    }
    observations.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut aggregated: Vec<RuntimeSurfaceEvidence> = Vec::new();
    for observation in observations {
        if let Some(existing) = aggregated
            .last_mut()
            .filter(|prior| prior.id == observation.id)
        {
            existing.occurrences += observation.occurrences;
        } else {
            aggregated.push(observation);
        }
    }
    let environment = (environment.len() == 1).then(|| environment.into_iter().next().unwrap());
    Ok(RuntimeEvidenceReport {
        version: 1,
        origin: origin.replace('\\', "/"),
        environment,
        window_start_unix_nano: window_start,
        window_end_unix_nano: window_end,
        complete_window: window_start.is_some() && window_end.is_some(),
        spans_scanned,
        rejected_spans,
        observations: aggregated,
    })
}

fn sanitize_span(attributes: &BTreeMap<String, String>) -> Option<RuntimeSurfaceEvidence> {
    let source_file = attributes
        .get("code.file.path")
        .or_else(|| attributes.get("code.filepath"))
        .and_then(|value| sanitize_source_file(value));
    let source_line = attributes
        .get("code.line.number")
        .or_else(|| attributes.get("code.lineno"))
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|line| *line > 0);
    let source_function = attributes
        .get("code.function.name")
        .or_else(|| attributes.get("code.function"))
        .map(|value| value.trim())
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .map(str::to_string);
    if let Some(method) = attributes
        .get("http.request.method")
        .or_else(|| attributes.get("http.method"))
    {
        let authority = attributes
            .get("server.address")
            .or_else(|| attributes.get("net.peer.name"))
            .map(|value| value.trim().to_ascii_lowercase());
        let path = attributes
            .get("http.route")
            .or_else(|| attributes.get("url.path"))
            .or_else(|| attributes.get("http.target"))
            .map(|value| sanitize_path(value));
        if authority.is_none() && path.is_none() {
            return None;
        }
        return Some(runtime_observation(
            RuntimeSurfaceKind::Http,
            attributes.get("url.scheme").map_or("http", String::as_str),
            method,
            authority,
            path,
            None,
            None,
            source_file,
            source_line,
            source_function,
        ));
    }
    if let (Some(system), Some(service), Some(method)) = (
        attributes.get("rpc.system"),
        attributes.get("rpc.service"),
        attributes.get("rpc.method"),
    ) {
        return Some(runtime_observation(
            RuntimeSurfaceKind::Rpc,
            system,
            "CALL",
            None,
            None,
            Some(service.clone()),
            Some(method.clone()),
            source_file,
            source_line,
            source_function,
        ));
    }
    if let Some(system) = attributes.get("messaging.system") {
        let destination = attributes
            .get("messaging.destination.name")
            .or_else(|| attributes.get("messaging.destination"))
            .cloned();
        let operation = attributes
            .get("messaging.operation.type")
            .or_else(|| attributes.get("messaging.operation"))
            .cloned();
        destination.as_ref()?;
        let method = operation.clone().unwrap_or_else(|| "message".into());
        return Some(runtime_observation(
            RuntimeSurfaceKind::Message,
            system,
            &method,
            destination,
            None,
            None,
            operation,
            source_file,
            source_line,
            source_function,
        ));
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn runtime_observation(
    kind: RuntimeSurfaceKind,
    protocol: &str,
    method: &str,
    authority: Option<String>,
    path: Option<String>,
    service: Option<String>,
    operation: Option<String>,
    source_file: Option<String>,
    source_line: Option<u32>,
    source_function: Option<String>,
) -> RuntimeSurfaceEvidence {
    let identity = serde_json::to_vec(&(
        kind,
        protocol.to_ascii_lowercase(),
        method.to_ascii_uppercase(),
        &authority,
        &path,
        &service,
        &operation,
        &source_file,
        source_line,
        &source_function,
    ))
    .expect("runtime coordinates serialize");
    let evidence_digest = blake3::hash(&identity).to_hex().to_string();
    RuntimeSurfaceEvidence {
        id: format!("runtime_surface_{}", &evidence_digest[..24]),
        kind,
        protocol: protocol.to_ascii_lowercase(),
        method: method.to_ascii_uppercase(),
        authority,
        path,
        service,
        operation,
        source_file,
        source_line,
        source_function,
        evidence_digest,
        occurrences: 1,
    }
}

fn sanitize_source_file(value: &str) -> Option<String> {
    let normalized = value.trim().replace('\\', "/");
    if normalized.is_empty()
        || normalized.len() > 4096
        || normalized.split('/').any(|part| part == "..")
    {
        return None;
    }
    let mut parts = normalized
        .split('/')
        .filter(|part| !part.is_empty() && !part.ends_with(':'))
        .collect::<Vec<_>>();
    if parts.len() > 8 {
        parts = parts.split_off(parts.len() - 8);
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn sanitize_path(value: &str) -> String {
    let path = value.split(['?', '#']).next().unwrap_or("/");
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            if is_high_cardinality(segment) {
                ":id"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>();
    format!("/{}", segments.join("/"))
}

fn is_high_cardinality(value: &str) -> bool {
    value.chars().all(|ch| ch.is_ascii_digit())
        || (value.len() >= 16 && value.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '-'))
}

fn attribute_map(value: &Value) -> BTreeMap<String, String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|attribute| {
            let key = attribute.get("key")?.as_str()?.to_string();
            let value = attribute.get("value")?;
            let value = value
                .get("stringValue")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| value.get("intValue").map(value_string))
                .or_else(|| value.get("boolValue").map(value_string))?;
            Some((key, value))
        })
        .collect()
}

fn value_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn integer_field(value: &Value, field: &str) -> Option<u64> {
    value
        .get(field)
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeEvidenceError {
    #[error("runtime evidence exceeds the {MAX_RUNTIME_EVIDENCE_BYTES}-byte cap: {0} bytes")]
    TooLarge(usize),
    #[error("runtime evidence exceeds the {MAX_SPANS}-span cap: {0}")]
    TooManySpans(usize),
    #[error("invalid runtime evidence JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("runtime evidence is not an OTLP JSON export")]
    NotOtlp,
}
