use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ApiBreakingChange, ApiChangeEvent, ApiOperationAnchor, BreakingChangeKind, EvidenceSpan,
    SourceArtifact, VersionRange,
};

const MAX_CONTRACT_BYTES: usize = 10 * 1024 * 1024;
const HTTP_METHODS: &[&str] = &[
    "get", "put", "post", "delete", "patch", "head", "options", "trace",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceFormat {
    #[default]
    OpenApi,
    AsyncApi,
    GraphQl,
    Protobuf,
    Wsdl,
    Smithy,
    OpenRpc,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseCompleteness {
    #[default]
    Complete,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceLoss {
    pub pointer: String,
    pub reason: String,
}

/// Protocol-neutral contract reader boundary. Readers consume untrusted bytes and
/// return the common operation/field model used by compatibility policies.
pub trait SurfaceReader {
    fn read(&self, vendor: &str, bytes: &[u8]) -> Result<ApiContract, ContractError>;
}

/// Built-in reader that validates and auto-detects every supported contract form.
#[derive(Debug, Default, Clone, Copy)]
pub struct AutoSurfaceReader;

impl SurfaceReader for AutoSurfaceReader {
    fn read(&self, vendor: &str, bytes: &[u8]) -> Result<ApiContract, ContractError> {
        normalize_contract_inner(vendor, bytes)
    }
}

/// Protocol compatibility boundary. Alternative policies can retain wire/source
/// distinctions while sharing acquisition and storage.
pub trait CompatibilityPolicy {
    fn diff(
        &self,
        old: &ApiContract,
        new: &ApiContract,
        source: SourceArtifact,
        affected_versions: VersionRange,
    ) -> Result<ApiChangeEvent, ContractError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultCompatibilityPolicy;

impl CompatibilityPolicy for DefaultCompatibilityPolicy {
    fn diff(
        &self,
        old: &ApiContract,
        new: &ApiContract,
        source: SourceArtifact,
        affected_versions: VersionRange,
    ) -> Result<ApiChangeEvent, ContractError> {
        diff_contracts(old, new, source, affected_versions)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiContract {
    pub version: u32,
    pub vendor: String,
    pub digest: String,
    pub format_version: String,
    #[serde(default)]
    pub format: SurfaceFormat,
    #[serde(default)]
    pub completeness: ParseCompleteness,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub losses: Vec<SurfaceLoss>,
    pub operations: BTreeMap<String, ContractOperation>,
}

impl ApiContract {
    pub const VERSION: u32 = 1;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractOperation {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub declared_operation_id: Option<String>,
    pub anchor: ApiOperationAnchor,
    pub request_fields: BTreeMap<String, FieldShape>,
    pub response_fields: BTreeMap<String, FieldShape>,
    pub security_digest: String,
    pub webhook: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldShape {
    pub field_type: String,
    pub required: bool,
    #[serde(default)]
    pub enum_values: BTreeSet<String>,
}

pub fn normalize_openapi(vendor: &str, bytes: &[u8]) -> Result<ApiContract, ContractError> {
    if bytes.len() > MAX_CONTRACT_BYTES {
        return Err(ContractError::TooLarge(bytes.len()));
    }
    let source = std::str::from_utf8(bytes).map_err(|_| ContractError::NonUtf8)?;
    let root: Value = if source.trim_start().starts_with(['{', '[']) {
        serde_json::from_str(source)?
    } else {
        serde_norway::from_str(source).map_err(|error| ContractError::Yaml(error.to_string()))?
    };
    let format_version = root
        .get("openapi")
        .or_else(|| root.get("swagger"))
        .and_then(Value::as_str)
        .ok_or(ContractError::NotOpenApi)?
        .to_string();
    let vendor = vendor.trim().to_ascii_lowercase();
    let mut operations = BTreeMap::new();
    collect_operations(&root, &vendor, "paths", false, &mut operations);
    collect_operations(&root, &vendor, "webhooks", true, &mut operations);
    let canonical = serde_json::to_vec(&root)?;
    let digest = blake3::hash(&canonical).to_hex().to_string();
    let losses = reference_losses(&root);
    Ok(ApiContract {
        version: ApiContract::VERSION,
        vendor,
        digest,
        format_version,
        format: SurfaceFormat::OpenApi,
        completeness: if losses.is_empty() {
            ParseCompleteness::Complete
        } else {
            ParseCompleteness::Partial
        },
        losses,
        operations,
    })
}

/// Auto-detect and normalize a supported contract without trusting its extension or
/// media type.
pub fn normalize_contract(vendor: &str, bytes: &[u8]) -> Result<ApiContract, ContractError> {
    AutoSurfaceReader.read(vendor, bytes)
}

fn normalize_contract_inner(vendor: &str, bytes: &[u8]) -> Result<ApiContract, ContractError> {
    if bytes.len() > MAX_CONTRACT_BYTES {
        return Err(ContractError::TooLarge(bytes.len()));
    }
    let source = std::str::from_utf8(bytes).map_err(|_| ContractError::NonUtf8)?;
    let trimmed = source.trim_start();
    let structured: Option<Value> = if trimmed.starts_with(['{', '[']) {
        serde_json::from_str(source).ok()
    } else {
        serde_norway::from_str(source).ok()
    };
    if let Some(root) = structured.as_ref() {
        if root.get("openapi").is_some() || root.get("swagger").is_some() {
            return normalize_openapi(vendor, bytes);
        }
        if root.get("asyncapi").is_some() {
            return normalize_asyncapi(vendor, root);
        }
        if root.get("openrpc").is_some() {
            return normalize_openrpc(vendor, root);
        }
        if root.pointer("/data/__schema").is_some() || root.get("__schema").is_some() {
            return normalize_graphql_introspection(vendor, root);
        }
        if root.get("smithy").is_some() && root.get("shapes").is_some() {
            return normalize_smithy_ast(vendor, root);
        }
    }
    if trimmed.starts_with('<') && source.contains("definitions") {
        return normalize_wsdl(vendor, source);
    }
    if looks_like_protobuf(source) {
        return normalize_protobuf(vendor, source);
    }
    if looks_like_smithy(source) {
        return normalize_smithy(vendor, source);
    }
    if looks_like_graphql(source) {
        return normalize_graphql_sdl(vendor, source);
    }
    Err(ContractError::UnknownFormat)
}

fn base_contract(
    vendor: &str,
    format: SurfaceFormat,
    format_version: String,
    canonical: &[u8],
    operations: BTreeMap<String, ContractOperation>,
    mut losses: Vec<SurfaceLoss>,
) -> ApiContract {
    let compatibility_boundary = match format {
        SurfaceFormat::OpenApi => None,
        SurfaceFormat::AsyncApi => Some(
            "AsyncAPI protocol bindings and producer/consumer compatibility require a format-specific policy",
        ),
        SurfaceFormat::GraphQl => Some(
            "GraphQL directive, deprecation, interface, and client compatibility require a format-specific policy",
        ),
        SurfaceFormat::Protobuf => Some(
            "Protobuf wire, wire-JSON, and generated-source compatibility require descriptors and a format-specific policy",
        ),
        SurfaceFormat::Wsdl => Some(
            "WSDL message, binding, and imported XML Schema compatibility require a format-specific policy",
        ),
        SurfaceFormat::Smithy => Some(
            "Smithy trait, protocol, and transform compatibility require a format-specific policy",
        ),
        SurfaceFormat::OpenRpc => Some(
            "OpenRPC errors, servers, and JSON-RPC compatibility require a format-specific policy",
        ),
    };
    if let Some(reason) = compatibility_boundary {
        losses.push(SurfaceLoss {
            pointer: "/compatibility".into(),
            reason: reason.into(),
        });
    }
    losses.sort_by(|left, right| left.pointer.cmp(&right.pointer));
    losses.dedup();
    ApiContract {
        version: ApiContract::VERSION,
        vendor: vendor.trim().to_ascii_lowercase(),
        digest: blake3::hash(canonical).to_hex().to_string(),
        format_version,
        format,
        completeness: if losses.is_empty() {
            ParseCompleteness::Complete
        } else {
            ParseCompleteness::Partial
        },
        losses,
        operations,
    }
}

fn reference_losses(root: &Value) -> Vec<SurfaceLoss> {
    fn push_segment(pointer: &mut String, segment: &str) {
        pointer.push('/');
        for character in segment.chars() {
            match character {
                '~' => pointer.push_str("~0"),
                '/' => pointer.push_str("~1"),
                character => pointer.push(character),
            }
        }
    }

    fn loss_pointer(pointer: &str) -> String {
        let mut result = String::with_capacity(pointer.len() + 5);
        result.push_str(pointer);
        result.push_str("/$ref");
        result
    }

    fn visit(
        root: &Value,
        value: &Value,
        pointer: &mut String,
        depth: usize,
        out: &mut Vec<SurfaceLoss>,
    ) {
        if depth > 128 {
            out.push(SurfaceLoss {
                pointer: pointer.clone(),
                reason: "document nesting exceeds the 128-level normalization cap".into(),
            });
            return;
        }
        match value {
            Value::Object(object) => {
                if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                    if let Some(target) = reference.strip_prefix('#') {
                        if root.pointer(target).is_none() {
                            out.push(SurfaceLoss {
                                pointer: loss_pointer(pointer),
                                reason: format!("unresolved local reference {reference}"),
                            });
                        }
                    } else {
                        out.push(SurfaceLoss {
                            pointer: loss_pointer(pointer),
                            reason: format!(
                                "remote reference is not fetched implicitly: {reference}"
                            ),
                        });
                    }
                }
                for (key, child) in object {
                    let original_len = pointer.len();
                    push_segment(pointer, key);
                    visit(root, child, pointer, depth + 1, out);
                    pointer.truncate(original_len);
                }
            }
            Value::Array(array) => {
                for (index, child) in array.iter().enumerate() {
                    let original_len = pointer.len();
                    write!(pointer, "/{index}").expect("writing to a String cannot fail");
                    visit(root, child, pointer, depth + 1, out);
                    pointer.truncate(original_len);
                }
            }
            _ => {}
        }
    }
    let mut losses = Vec::new();
    visit(root, root, &mut String::new(), 0, &mut losses);
    losses.sort_by(|left, right| left.pointer.cmp(&right.pointer));
    losses.dedup();
    losses
}

fn empty_operation(
    vendor: &str,
    protocol: &str,
    method: &str,
    path: &str,
    key: &str,
) -> ContractOperation {
    ContractOperation {
        key: key.to_string(),
        declared_operation_id: Some(key.to_string()),
        anchor: ApiOperationAnchor::new(vendor, protocol, method, path),
        request_fields: BTreeMap::new(),
        response_fields: BTreeMap::new(),
        security_digest: blake3::hash(b"null").to_hex().to_string(),
        webhook: false,
    }
}

fn normalize_asyncapi(vendor: &str, root: &Value) -> Result<ApiContract, ContractError> {
    let mut operations = BTreeMap::new();
    if let Some(channels) = root.get("channels").and_then(Value::as_object) {
        for (channel, item) in channels {
            let Some(item) = resolve(root, item, 0) else {
                continue;
            };
            for action in ["publish", "subscribe"] {
                let Some(operation) = item.get(action).and_then(|value| resolve(root, value, 0))
                else {
                    continue;
                };
                let key = operation
                    .get("operationId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{action} {channel}"));
                let mut normalized = empty_operation(vendor, "asyncapi", action, channel, &key);
                let message = operation
                    .get("message")
                    .and_then(|value| resolve(root, value, 0));
                if let Some(payload) = message
                    .and_then(|message| message.get("payload"))
                    .and_then(|value| resolve(root, value, 0))
                {
                    let fields = if action == "publish" {
                        &mut normalized.request_fields
                    } else {
                        &mut normalized.response_fields
                    };
                    flatten_schema(root, payload, "", false, 0, fields);
                }
                operations.insert(key, normalized);
            }
        }
    }
    // AsyncAPI 3.x moved operations to a top-level map and made channels
    // reusable address/message objects.
    if let Some(declared) = root.get("operations").and_then(Value::as_object) {
        for (key, raw_operation) in declared {
            let Some(operation) = resolve(root, raw_operation, 0) else {
                continue;
            };
            let action = operation
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("send");
            let channel = operation
                .get("channel")
                .and_then(|value| resolve(root, value, 0));
            let address = channel
                .and_then(|channel| channel.get("address"))
                .and_then(Value::as_str)
                .unwrap_or(key);
            let mut normalized = empty_operation(vendor, "asyncapi", action, address, key.as_str());
            let mut messages = operation
                .get("messages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|message| resolve(root, message, 0))
                .collect::<Vec<_>>();
            if messages.is_empty() {
                messages.extend(
                    channel
                        .and_then(|channel| channel.get("messages"))
                        .and_then(Value::as_object)
                        .into_iter()
                        .flat_map(|messages| messages.values())
                        .filter_map(|message| resolve(root, message, 0)),
                );
            }
            for message in messages {
                let Some(payload) = message
                    .get("payload")
                    .and_then(|value| resolve(root, value, 0))
                else {
                    continue;
                };
                let fields = if action == "send" {
                    &mut normalized.request_fields
                } else {
                    &mut normalized.response_fields
                };
                flatten_schema(root, payload, "", false, 0, fields);
            }
            operations.insert(key.clone(), normalized);
        }
    }
    let canonical = serde_json::to_vec(root)?;
    Ok(base_contract(
        vendor,
        SurfaceFormat::AsyncApi,
        root.get("asyncapi")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        &canonical,
        operations,
        reference_losses(root),
    ))
}

fn normalize_openrpc(vendor: &str, root: &Value) -> Result<ApiContract, ContractError> {
    let mut operations = BTreeMap::new();
    for method in root
        .get("methods")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = method.get("name").and_then(Value::as_str) else {
            continue;
        };
        let mut operation = empty_operation(vendor, "jsonrpc", "call", name, name);
        for parameter in method
            .get("params")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(parameter_name) = parameter.get("name").and_then(Value::as_str) else {
                continue;
            };
            let schema = parameter
                .get("schema")
                .and_then(|value| resolve(root, value, 0));
            operation.request_fields.insert(
                parameter_name.to_string(),
                FieldShape {
                    field_type: schema.map(schema_type).unwrap_or_else(|| "unknown".into()),
                    required: parameter
                        .get("required")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    enum_values: schema.map(schema_enum).unwrap_or_default(),
                },
            );
        }
        if let Some(result) = method.get("result") {
            let result_name = result
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("result");
            let schema = result
                .get("schema")
                .and_then(|value| resolve(root, value, 0));
            operation.response_fields.insert(
                result_name.to_string(),
                FieldShape {
                    field_type: schema.map(schema_type).unwrap_or_else(|| "unknown".into()),
                    required: true,
                    enum_values: schema.map(schema_enum).unwrap_or_default(),
                },
            );
        }
        operations.insert(name.to_string(), operation);
    }
    let canonical = serde_json::to_vec(root)?;
    Ok(base_contract(
        vendor,
        SurfaceFormat::OpenRpc,
        root.get("openrpc")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        &canonical,
        operations,
        reference_losses(root),
    ))
}

fn looks_like_graphql(source: &str) -> bool {
    regex::Regex::new(r"(?m)^\s*(schema\s*\{|type\s+(Query|Mutation|Subscription)\b)")
        .expect("valid GraphQL detector")
        .is_match(source)
}

fn normalize_graphql_sdl(vendor: &str, source: &str) -> Result<ApiContract, ContractError> {
    let schema_re = regex::Regex::new(r"(?s)\b(?:extend\s+)?schema\b[^\{]*\{(.*?)\}")
        .expect("valid GraphQL schema regex");
    let root_re =
        regex::Regex::new(r"(?m)\b(query|mutation|subscription)\s*:\s*([_A-Za-z][_0-9A-Za-z]*)")
            .expect("valid GraphQL root regex");
    let type_re =
        regex::Regex::new(r"(?s)\b(?:extend\s+)?type\s+([_A-Za-z][_0-9A-Za-z]*)\b[^\{]*\{(.*?)\}")
            .expect("valid GraphQL type regex");
    let field_re = regex::Regex::new(
        r"([_A-Za-z][_0-9A-Za-z]*)\s*(?:\(([^)]*)\))?\s*:\s*([!\[\]_0-9A-Za-z]+)",
    )
    .expect("valid GraphQL field regex");
    let arg_re = regex::Regex::new(r"([_A-Za-z][_0-9A-Za-z]*)\s*:\s*([!\[\]_0-9A-Za-z]+)")
        .expect("valid GraphQL argument regex");
    let mut root_types = BTreeMap::new();
    for schema in schema_re.captures_iter(source) {
        for root in root_re.captures_iter(&schema[1]) {
            let operation_kind = match &root[1] {
                "query" => "Query",
                "mutation" => "Mutation",
                "subscription" => "Subscription",
                _ => unreachable!("root regex limits operation kinds"),
            };
            root_types.insert(root[2].to_string(), operation_kind);
        }
    }
    if root_types.is_empty() {
        root_types.extend([
            ("Query".to_string(), "Query"),
            ("Mutation".to_string(), "Mutation"),
            ("Subscription".to_string(), "Subscription"),
        ]);
    }
    let mut operations = BTreeMap::new();
    for captures in type_re.captures_iter(source) {
        let type_name = &captures[1];
        let Some(operation_kind) = root_types.get(type_name) else {
            continue;
        };
        for field in field_re.captures_iter(&captures[2]) {
            let key = format!("{type_name}.{}", &field[1]);
            let mut operation = empty_operation(vendor, "graphql", operation_kind, &field[1], &key);
            if let Some(arguments) = field.get(2) {
                for argument in arg_re.captures_iter(arguments.as_str()) {
                    operation.request_fields.insert(
                        argument[1].to_string(),
                        FieldShape {
                            field_type: argument[2].to_string(),
                            required: argument[2].ends_with('!'),
                            enum_values: BTreeSet::new(),
                        },
                    );
                }
            }
            operation.response_fields.insert(
                "result".into(),
                FieldShape {
                    field_type: field[3].to_string(),
                    required: field[3].ends_with('!'),
                    enum_values: BTreeSet::new(),
                },
            );
            operations.insert(key, operation);
        }
    }
    if operations.is_empty() {
        return Err(ContractError::EmptySurface(SurfaceFormat::GraphQl));
    }
    Ok(base_contract(
        vendor,
        SurfaceFormat::GraphQl,
        "sdl".into(),
        source.as_bytes(),
        operations,
        Vec::new(),
    ))
}

fn graphql_type(value: &Value) -> String {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("UNKNOWN");
    match kind {
        "NON_NULL" => format!(
            "{}!",
            value
                .get("ofType")
                .map(graphql_type)
                .unwrap_or_else(|| "unknown".into())
        ),
        "LIST" => format!(
            "[{}]",
            value
                .get("ofType")
                .map(graphql_type)
                .unwrap_or_else(|| "unknown".into())
        ),
        _ => value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(kind)
            .to_string(),
    }
}

fn normalize_graphql_introspection(
    vendor: &str,
    root: &Value,
) -> Result<ApiContract, ContractError> {
    let schema = root
        .pointer("/data/__schema")
        .or_else(|| root.get("__schema"))
        .ok_or(ContractError::UnknownFormat)?;
    let root_names = ["queryType", "mutationType", "subscriptionType"]
        .into_iter()
        .filter_map(|kind| {
            schema
                .get(kind)
                .and_then(|value| value.get("name"))
                .and_then(Value::as_str)
                .map(|name| (kind, name))
        })
        .collect::<BTreeMap<_, _>>();
    let mut operations = BTreeMap::new();
    for object in schema
        .get("types")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(type_name) = object.get("name").and_then(Value::as_str) else {
            continue;
        };
        let root_kind = root_names
            .iter()
            .find_map(|(kind, name)| (*name == type_name).then_some(*kind));
        let Some(root_kind) = root_kind else { continue };
        for field in object
            .get("fields")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = field.get("name").and_then(Value::as_str) else {
                continue;
            };
            let key = format!("{type_name}.{name}");
            let mut operation = empty_operation(vendor, "graphql", root_kind, name, &key);
            for argument in field
                .get("args")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(argument_name) = argument.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let field_type = argument
                    .get("type")
                    .map(graphql_type)
                    .unwrap_or_else(|| "unknown".into());
                operation.request_fields.insert(
                    argument_name.into(),
                    FieldShape {
                        required: field_type.ends_with('!'),
                        field_type,
                        enum_values: BTreeSet::new(),
                    },
                );
            }
            let field_type = field
                .get("type")
                .map(graphql_type)
                .unwrap_or_else(|| "unknown".into());
            operation.response_fields.insert(
                "result".into(),
                FieldShape {
                    required: field_type.ends_with('!'),
                    field_type,
                    enum_values: BTreeSet::new(),
                },
            );
            operations.insert(key, operation);
        }
    }
    let canonical = serde_json::to_vec(root)?;
    Ok(base_contract(
        vendor,
        SurfaceFormat::GraphQl,
        "introspection".into(),
        &canonical,
        operations,
        Vec::new(),
    ))
}

fn looks_like_protobuf(source: &str) -> bool {
    source.contains("syntax") && source.contains("service") && source.contains("rpc")
}

fn normalize_protobuf(vendor: &str, source: &str) -> Result<ApiContract, ContractError> {
    let package_re = regex::Regex::new(r"(?m)\bpackage\s+([._0-9A-Za-z]+)\s*;").unwrap();
    let service_re = regex::Regex::new(r"(?s)\bservice\s+([A-Za-z_]\w*)\s*\{(.*?)\}").unwrap();
    let rpc_re = regex::Regex::new(r"\brpc\s+([A-Za-z_]\w*)\s*\(\s*(?:stream\s+)?([._0-9A-Za-z]+)\s*\)\s*returns\s*\(\s*(?:stream\s+)?([._0-9A-Za-z]+)\s*\)").unwrap();
    let package = package_re
        .captures(source)
        .map(|captures| captures[1].to_string())
        .unwrap_or_default();
    let mut operations = BTreeMap::new();
    for service in service_re.captures_iter(source) {
        for rpc in rpc_re.captures_iter(&service[2]) {
            let key = [package.as_str(), &service[1], &rpc[1]]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(".");
            let mut operation = empty_operation(vendor, "grpc", "call", &key, &key);
            operation.request_fields.insert(
                "message".into(),
                FieldShape {
                    field_type: rpc[2].into(),
                    required: true,
                    enum_values: BTreeSet::new(),
                },
            );
            operation.response_fields.insert(
                "message".into(),
                FieldShape {
                    field_type: rpc[3].into(),
                    required: true,
                    enum_values: BTreeSet::new(),
                },
            );
            operations.insert(key, operation);
        }
    }
    Ok(base_contract(
        vendor,
        SurfaceFormat::Protobuf,
        "proto3".into(),
        source.as_bytes(),
        operations,
        Vec::new(),
    ))
}

fn normalize_wsdl(vendor: &str, source: &str) -> Result<ApiContract, ContractError> {
    let document = roxmltree::Document::parse(source)
        .map_err(|error| ContractError::Xml(error.to_string()))?;
    let mut operations = BTreeMap::new();
    for interface in document
        .descendants()
        .filter(|node| matches!(node.tag_name().name(), "portType" | "interface"))
    {
        let interface_name = interface.attribute("name").unwrap_or("interface");
        for operation_node in interface
            .children()
            .filter(|node| node.tag_name().name() == "operation")
        {
            let Some(name) = operation_node.attribute("name") else {
                continue;
            };
            let key = format!("{interface_name}.{name}");
            let mut operation = empty_operation(vendor, "soap", "call", name, &key);
            if let Some(input) = operation_node
                .children()
                .find(|node| node.tag_name().name() == "input")
                && let Some(message) = input
                    .attribute("message")
                    .or_else(|| input.attribute("element"))
            {
                operation.request_fields.insert(
                    "message".into(),
                    FieldShape {
                        field_type: message.into(),
                        required: true,
                        enum_values: BTreeSet::new(),
                    },
                );
            }
            if let Some(output) = operation_node
                .children()
                .find(|node| node.tag_name().name() == "output")
                && let Some(message) = output
                    .attribute("message")
                    .or_else(|| output.attribute("element"))
            {
                operation.response_fields.insert(
                    "message".into(),
                    FieldShape {
                        field_type: message.into(),
                        required: true,
                        enum_values: BTreeSet::new(),
                    },
                );
            }
            operations.insert(key, operation);
        }
    }
    Ok(base_contract(
        vendor,
        SurfaceFormat::Wsdl,
        "wsdl".into(),
        source.as_bytes(),
        operations,
        Vec::new(),
    ))
}

fn looks_like_smithy(source: &str) -> bool {
    source.contains("$version")
        && source.contains("namespace")
        && (source.contains("service") || source.contains("operation"))
}

fn normalize_smithy(vendor: &str, source: &str) -> Result<ApiContract, ContractError> {
    let namespace_re = regex::Regex::new(r"(?m)^\s*namespace\s+([._0-9A-Za-z]+)").unwrap();
    let operation_re = regex::Regex::new(r"\boperation\s+([A-Za-z_]\w*)").unwrap();
    let namespace = namespace_re
        .captures(source)
        .map(|captures| captures[1].to_string())
        .unwrap_or_default();
    let mut operations = BTreeMap::new();
    for operation in operation_re.captures_iter(source) {
        let key = if namespace.is_empty() {
            operation[1].to_string()
        } else {
            format!("{namespace}.{}", &operation[1])
        };
        operations.insert(
            key.clone(),
            empty_operation(vendor, "smithy", "call", &key, &key),
        );
    }
    Ok(base_contract(
        vendor,
        SurfaceFormat::Smithy,
        "2".into(),
        source.as_bytes(),
        operations,
        Vec::new(),
    ))
}

fn normalize_smithy_ast(vendor: &str, root: &Value) -> Result<ApiContract, ContractError> {
    let mut operations = BTreeMap::new();
    for (shape_id, shape) in root
        .get("shapes")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if shape.get("type").and_then(Value::as_str) != Some("operation") {
            continue;
        }
        operations.insert(
            shape_id.clone(),
            empty_operation(vendor, "smithy", "call", shape_id, shape_id),
        );
    }
    let canonical = serde_json::to_vec(root)?;
    Ok(base_contract(
        vendor,
        SurfaceFormat::Smithy,
        root.get("smithy")
            .and_then(Value::as_str)
            .unwrap_or("2")
            .into(),
        &canonical,
        operations,
        Vec::new(),
    ))
}

fn collect_operations(
    root: &Value,
    vendor: &str,
    section: &str,
    webhook: bool,
    operations: &mut BTreeMap<String, ContractOperation>,
) {
    let Some(paths) = root.get(section).and_then(Value::as_object) else {
        return;
    };
    for (path, path_item) in paths {
        let Some(path_item) = resolve(root, path_item, 0).and_then(Value::as_object) else {
            continue;
        };
        for method in HTTP_METHODS {
            let Some(operation) = path_item
                .get(*method)
                .and_then(|value| resolve(root, value, 0))
            else {
                continue;
            };
            let declared_operation_id = operation
                .get("operationId")
                .and_then(Value::as_str)
                .map(str::to_string);
            let key = declared_operation_id
                .clone()
                .unwrap_or_else(|| format!("{} {}", method.to_ascii_uppercase(), path));
            let protocol = if webhook { "webhook" } else { "https" };
            let anchor = ApiOperationAnchor::new(vendor, protocol, method, path);
            let request_schema = request_schema(root, operation);
            let response_schema = first_response_schema(root, operation);
            let mut request_fields = BTreeMap::new();
            let mut response_fields = BTreeMap::new();
            if let Some(schema) = request_schema {
                flatten_schema(root, schema, "", false, 0, &mut request_fields);
            }
            if let Some(schema) = response_schema {
                flatten_schema(root, schema, "", false, 0, &mut response_fields);
            }
            add_header_parameters(root, path_item.get("parameters"), &mut request_fields);
            add_header_parameters(root, operation.get("parameters"), &mut request_fields);
            let security = operation
                .get("security")
                .or_else(|| root.get("security"))
                .cloned()
                .unwrap_or(Value::Null);
            let security_digest = blake3::hash(
                serde_json::to_string(&security)
                    .unwrap_or_default()
                    .as_bytes(),
            )
            .to_hex()
            .to_string();
            operations.insert(
                key.clone(),
                ContractOperation {
                    key,
                    declared_operation_id,
                    anchor,
                    request_fields,
                    response_fields,
                    security_digest,
                    webhook,
                },
            );
        }
    }
}

fn request_schema<'a>(root: &'a Value, operation: &'a Value) -> Option<&'a Value> {
    let request_body = operation
        .get("requestBody")
        .and_then(|value| resolve(root, value, 0));
    request_body
        .and_then(|body| body.pointer("/content/application~1json/schema"))
        .or_else(|| {
            request_body.and_then(|body| body.pointer("/content/application~1problem+json/schema"))
        })
        .or_else(|| {
            operation
                .get("parameters")?
                .as_array()?
                .iter()
                .filter_map(|parameter| resolve(root, parameter, 0))
                .find(|parameter| parameter.get("in").and_then(Value::as_str) == Some("body"))
                .and_then(|parameter| parameter.get("schema"))
        })
}

fn first_response_schema<'a>(root: &'a Value, operation: &'a Value) -> Option<&'a Value> {
    let responses = operation.get("responses")?.as_object()?;
    responses
        .iter()
        .filter(|(status, _)| status.starts_with('2') || status.as_str() == "default")
        .find_map(|(_, response)| {
            let response = resolve(root, response, 0)?;
            response
                .pointer("/content/application~1json/schema")
                .or_else(|| response.pointer("/content/application~1problem+json/schema"))
                .or_else(|| response.get("schema"))
        })
}

fn add_header_parameters(
    root: &Value,
    parameters: Option<&Value>,
    fields: &mut BTreeMap<String, FieldShape>,
) {
    let Some(parameters) = parameters.and_then(Value::as_array) else {
        return;
    };
    for parameter in parameters {
        let Some(parameter) = resolve(root, parameter, 0) else {
            continue;
        };
        if parameter.get("in").and_then(Value::as_str) != Some("header") {
            continue;
        }
        let Some(name) = parameter.get("name").and_then(Value::as_str) else {
            continue;
        };
        let schema = parameter
            .get("schema")
            .and_then(|value| resolve(root, value, 0));
        fields.insert(
            format!("header:{name}"),
            FieldShape {
                field_type: schema.map(schema_type).unwrap_or_else(|| "unknown".into()),
                required: parameter
                    .get("required")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                enum_values: schema.map(schema_enum).unwrap_or_default(),
            },
        );
    }
}

fn flatten_schema(
    root: &Value,
    schema: &Value,
    prefix: &str,
    required: bool,
    depth: usize,
    fields: &mut BTreeMap<String, FieldShape>,
) {
    if depth > 24 {
        return;
    }
    let Some(schema) = resolve(root, schema, depth) else {
        return;
    };
    if !prefix.is_empty() {
        fields.insert(
            prefix.to_string(),
            FieldShape {
                field_type: schema_type(schema),
                required,
                enum_values: schema_enum(schema),
            },
        );
    }
    let required_fields = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, child) in properties {
            let child_path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            flatten_schema(
                root,
                child,
                &child_path,
                required_fields.contains(name.as_str()),
                depth + 1,
                fields,
            );
        }
    }
    if let Some(items) = schema.get("items") {
        let child_path = format!("{prefix}[]");
        flatten_schema(root, items, &child_path, required, depth + 1, fields);
    }
}

fn resolve<'a>(root: &'a Value, value: &'a Value, depth: usize) -> Option<&'a Value> {
    if depth > 24 {
        return None;
    }
    let Some(reference) = value.get("$ref").and_then(Value::as_str) else {
        return Some(value);
    };
    let pointer = reference.strip_prefix('#')?;
    let target = root.pointer(pointer)?;
    resolve(root, target, depth + 1)
}

fn schema_type(schema: &Value) -> String {
    match schema.get("type") {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("|"),
        _ if schema.get("properties").is_some() => "object".into(),
        _ => "unknown".into(),
    }
}

fn schema_enum(schema: &Value) -> BTreeSet<String> {
    schema
        .get("enum")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string())
        })
        .collect()
}

pub fn diff_contracts(
    old: &ApiContract,
    new: &ApiContract,
    mut source: SourceArtifact,
    affected_versions: VersionRange,
) -> Result<ApiChangeEvent, ContractError> {
    if old.vendor != new.vendor {
        return Err(ContractError::VendorMismatch {
            old: old.vendor.clone(),
            new: new.vendor.clone(),
        });
    }
    if source.content_digest.is_empty() {
        source.content_digest = new.digest.clone();
    }
    let mut changes = Vec::new();
    let mut matched_new = BTreeSet::new();
    let mut new_by_anchor =
        BTreeMap::<(&str, &str, &str), Option<(&str, &ContractOperation)>>::new();
    for (key, operation) in &new.operations {
        new_by_anchor
            .entry((
                operation.anchor.protocol.as_str(),
                operation.anchor.method.as_str(),
                operation.anchor.canonical_path.as_str(),
            ))
            .and_modify(|candidate| *candidate = None)
            .or_insert(Some((key.as_str(), operation)));
    }
    for (key, old_operation) in &old.operations {
        let new_match = new
            .operations
            .get(key)
            .map(|operation| (key.as_str(), operation))
            .or_else(|| {
                new_by_anchor
                    .get(&(
                        old_operation.anchor.protocol.as_str(),
                        old_operation.anchor.method.as_str(),
                        old_operation.anchor.canonical_path.as_str(),
                    ))
                    .copied()
                    .flatten()
                    .filter(|(new_key, _)| !matched_new.contains(*new_key))
            });
        let Some((new_key, new_operation)) = new_match else {
            changes.push(change(
                if old_operation.webhook {
                    BreakingChangeKind::WebhookChanged
                } else {
                    BreakingChangeKind::OperationRemoved
                },
                old_operation,
                None,
                &affected_versions,
                &source,
                format!("operation {key} was removed"),
                format!("/operations/{key}"),
            ));
            continue;
        };
        matched_new.insert(new_key.to_string());
        if new_key != key {
            changes.push(change(
                BreakingChangeKind::OperationRenamed,
                old_operation,
                Some(new_operation),
                &affected_versions,
                &source,
                format!("operation {key} was renamed to {new_key}"),
                format!("/operations/{key}"),
            ));
        }
        if old_operation.anchor.method != new_operation.anchor.method
            || old_operation.anchor.canonical_path != new_operation.anchor.canonical_path
        {
            changes.push(change(
                BreakingChangeKind::PathOrMethodChanged,
                old_operation,
                Some(new_operation),
                &affected_versions,
                &source,
                format!(
                    "operation {key} moved from {} {} to {} {}",
                    old_operation.anchor.method,
                    old_operation.anchor.canonical_path,
                    new_operation.anchor.method,
                    new_operation.anchor.canonical_path
                ),
                format!("/operations/{key}"),
            ));
        }
        diff_fields(
            old_operation,
            new_operation,
            &affected_versions,
            &source,
            &mut changes,
        );
        if old_operation.security_digest != new_operation.security_digest {
            changes.push(change(
                BreakingChangeKind::AuthenticationOrVersionBehaviorChanged,
                old_operation,
                Some(new_operation),
                &affected_versions,
                &source,
                format!("authentication or version-header behavior changed for {key}"),
                format!("/operations/{key}/security"),
            ));
        }
    }
    changes.sort_by(|a, b| a.change_id.cmp(&b.change_id));
    let identity = serde_json::to_vec(&(
        &old.vendor,
        &source.revision,
        &old.digest,
        &new.digest,
        &changes,
    ))?;
    let id = format!(
        "api_event_{}_{}",
        old.vendor,
        &blake3::hash(&identity).to_hex()[..24]
    );
    Ok(ApiChangeEvent {
        version: ApiChangeEvent::VERSION,
        id,
        vendor: old.vendor.clone(),
        release: (!source.revision.is_empty()).then(|| source.revision.clone()),
        occurred_at: source.fetched_at,
        source,
        changes,
    })
}

fn diff_fields(
    old: &ContractOperation,
    new: &ContractOperation,
    versions: &VersionRange,
    source: &SourceArtifact,
    changes: &mut Vec<ApiBreakingChange>,
) {
    let request_renames = field_renames(&old.request_fields, &new.request_fields);
    let response_renames = field_renames(&old.response_fields, &new.response_fields);
    let renamed_request_targets = request_renames.values().collect::<BTreeSet<_>>();
    for (field, new_shape) in &new.request_fields {
        let old_shape = old.request_fields.get(field);
        if renamed_request_targets.contains(field) {
            continue;
        }
        if new_shape.required && old_shape.is_none_or(|shape| !shape.required) {
            changes.push(field_change(
                operation_kind(old, BreakingChangeKind::RequiredRequestFieldAdded),
                old,
                new,
                versions,
                source,
                field,
            ));
        }
    }
    for (field, old_shape) in &old.request_fields {
        let Some(new_shape) = new.request_fields.get(field) else {
            if let Some(new_field) = request_renames.get(field) {
                changes.push(field_rename(
                    operation_kind(old, BreakingChangeKind::RequestFieldRenamed),
                    old,
                    new,
                    versions,
                    source,
                    field,
                    new_field,
                ));
            } else {
                changes.push(field_change(
                    operation_kind(old, BreakingChangeKind::RequestFieldRemoved),
                    old,
                    new,
                    versions,
                    source,
                    field,
                ));
            }
            continue;
        };
        if old_shape.field_type != new_shape.field_type {
            changes.push(field_change(
                operation_kind(old, BreakingChangeKind::RequestFieldTypeChanged),
                old,
                new,
                versions,
                source,
                field,
            ));
        }
        if !old_shape.enum_values.is_empty()
            && !new_shape.enum_values.is_empty()
            && !old_shape.enum_values.is_subset(&new_shape.enum_values)
        {
            changes.push(field_change(
                operation_kind(old, BreakingChangeKind::RequestEnumNarrowed),
                old,
                new,
                versions,
                source,
                field,
            ));
        }
    }
    for (field, old_shape) in &old.response_fields {
        let Some(new_shape) = new.response_fields.get(field) else {
            if let Some(new_field) = response_renames.get(field) {
                changes.push(field_rename(
                    operation_kind(old, BreakingChangeKind::ResponseFieldRenamed),
                    old,
                    new,
                    versions,
                    source,
                    field,
                    new_field,
                ));
            } else {
                changes.push(field_change(
                    operation_kind(old, BreakingChangeKind::ResponseFieldRemoved),
                    old,
                    new,
                    versions,
                    source,
                    field,
                ));
            }
            continue;
        };
        if old_shape.field_type != new_shape.field_type {
            changes.push(field_change(
                operation_kind(old, BreakingChangeKind::ResponseFieldTypeChanged),
                old,
                new,
                versions,
                source,
                field,
            ));
        }
        if old_shape.enum_values != new_shape.enum_values
            && (!old_shape.enum_values.is_empty() || !new_shape.enum_values.is_empty())
        {
            changes.push(field_change(
                operation_kind(old, BreakingChangeKind::ResponseEnumChanged),
                old,
                new,
                versions,
                source,
                field,
            ));
        }
    }
}

fn field_renames(
    old: &BTreeMap<String, FieldShape>,
    new: &BTreeMap<String, FieldShape>,
) -> BTreeMap<String, String> {
    let mut renames = BTreeMap::new();
    let mut candidates = BTreeMap::<(&str, bool, &BTreeSet<String>), Option<&String>>::new();
    for (name, shape) in new.iter().filter(|(name, _)| !old.contains_key(*name)) {
        candidates
            .entry((
                shape.field_type.as_str(),
                shape.required,
                &shape.enum_values,
            ))
            .and_modify(|candidate| *candidate = None)
            .or_insert(Some(name));
    }
    for (old_name, old_shape) in old.iter().filter(|(name, _)| !new.contains_key(*name)) {
        if let Some(candidate) = candidates.get_mut(&(
            old_shape.field_type.as_str(),
            old_shape.required,
            &old_shape.enum_values,
        )) && let Some(new_name) = candidate.take()
        {
            renames.insert(old_name.clone(), new_name.clone());
        }
    }
    renames
}

fn operation_kind(
    operation: &ContractOperation,
    default: BreakingChangeKind,
) -> BreakingChangeKind {
    if operation.webhook {
        BreakingChangeKind::WebhookChanged
    } else {
        default
    }
}

fn field_rename(
    kind: BreakingChangeKind,
    old: &ContractOperation,
    new: &ContractOperation,
    versions: &VersionRange,
    source: &SourceArtifact,
    old_field: &str,
    new_field: &str,
) -> ApiBreakingChange {
    change(
        kind,
        old,
        Some(new),
        versions,
        source,
        format!("field {old_field} was renamed to {new_field}"),
        format!("/operations/{}/fields/{old_field}", old.key),
    )
}

fn field_change(
    kind: BreakingChangeKind,
    old: &ContractOperation,
    new: &ContractOperation,
    versions: &VersionRange,
    source: &SourceArtifact,
    field: &str,
) -> ApiBreakingChange {
    change(
        kind,
        old,
        Some(new),
        versions,
        source,
        format!("{kind:?} at field {field}"),
        format!("/operations/{}/fields/{field}", old.key),
    )
}

fn change(
    kind: BreakingChangeKind,
    old: &ContractOperation,
    new: Option<&ContractOperation>,
    versions: &VersionRange,
    source: &SourceArtifact,
    summary: String,
    pointer: String,
) -> ApiBreakingChange {
    let evidence_identity = format!("{}\0{}\0{}", source.uri, pointer, summary);
    let evidence = EvidenceSpan {
        source_uri: source.uri.clone(),
        pointer,
        summary: summary.chars().take(500).collect(),
        digest: blake3::hash(evidence_identity.as_bytes())
            .to_hex()
            .to_string(),
    };
    let identity = format!(
        "{:?}\0{}\0{}\0{}",
        kind,
        old.anchor.id,
        new.map(|operation| operation.anchor.id.as_str())
            .unwrap_or(""),
        evidence.digest
    );
    ApiBreakingChange {
        change_id: format!(
            "change_{}",
            &blake3::hash(identity.as_bytes()).to_hex()[..24]
        ),
        kind,
        affected_versions: versions.clone(),
        old_operation: Some(old.anchor.clone()),
        new_operation: new.map(|operation| operation.anchor.clone()),
        old_sdk_symbols: Vec::new(),
        new_sdk_symbols: Vec::new(),
        migration_summary: summary,
        evidence: vec![evidence],
        confidence: 1.0,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ContractError {
    #[error("contract exceeds the {MAX_CONTRACT_BYTES}-byte cap: {0} bytes")]
    TooLarge(usize),
    #[error("contract is not UTF-8")]
    NonUtf8,
    #[error("invalid JSON contract: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid YAML contract: {0}")]
    Yaml(String),
    #[error("artifact is not an OpenAPI/Swagger contract")]
    NotOpenApi,
    #[error("artifact is not a recognized API contract")]
    UnknownFormat,
    #[error("{0:?} contract contains no readable operations")]
    EmptySurface(SurfaceFormat),
    #[error("invalid XML contract: {0}")]
    Xml(String),
    #[error("cannot diff contracts for different vendors: {old:?} vs {new:?}")]
    VendorMismatch { old: String, new: String },
}
