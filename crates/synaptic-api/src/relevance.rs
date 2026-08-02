use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use synaptic_core::GraphData;

use crate::{ApiChangeEvent, ApiInventory, VendorRegistry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicabilityState {
    NotApplicable,
    ReviewRequired,
    Applicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicabilityReason {
    VendorDisabled,
    NoObservedUsage,
    VersionOutsideRange,
    VersionUnknown,
    AmbiguousVendor,
    BindingConfidenceTooLow,
    ChangeConfidenceTooLow,
    UsageOutsideAllowedScope,
    Applicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingBasis {
    AbsoluteUrlHost,
    SdkSymbol,
    GeneratedClient,
    ConfiguredHost,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiUsageBinding {
    pub vendor: String,
    pub operation_node_id: String,
    pub caller_node_id: String,
    pub source_file: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sdk_package: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sdk_member: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sdk_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub api_version: Option<String>,
    pub basis: BindingBasis,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelevanceAssessment {
    pub version: u32,
    pub event_id: String,
    pub vendor: String,
    pub state: ApplicabilityState,
    pub reasons: Vec<ApplicabilityReason>,
    pub matched_change_ids: Vec<String>,
    pub bindings: Vec<ApiUsageBinding>,
    pub seed_node_ids: Vec<String>,
    pub observed_versions: Vec<String>,
}

pub fn evaluate_relevance(
    event: &ApiChangeEvent,
    registry: &VendorRegistry,
    inventory: &ApiInventory,
    graph: &GraphData,
    allowed_paths: &[String],
) -> RelevanceAssessment {
    let Some(vendor) = registry
        .vendor(&event.vendor)
        .filter(|vendor| vendor.enabled)
    else {
        return assessment(
            event,
            ApplicabilityState::NotApplicable,
            vec![ApplicabilityReason::VendorDisabled],
        );
    };
    let all_bindings = usage_bindings(graph, &event.vendor);
    let mut bindings = Vec::new();
    let mut matched_changes = BTreeSet::new();
    let mut operation_changes = BTreeMap::<String, Vec<String>>::new();
    let mut symbol_changes = BTreeMap::<(String, String), Vec<String>>::new();
    let mut wildcard_symbol_changes = BTreeMap::<String, Vec<String>>::new();
    for change in &event.changes {
        for operation in [change.old_operation.as_ref(), change.new_operation.as_ref()]
            .into_iter()
            .flatten()
        {
            operation_changes
                .entry(operation.id.clone())
                .or_default()
                .push(change.change_id.clone());
        }
        for symbol in change.old_sdk_symbols.iter().chain(&change.new_sdk_symbols) {
            if symbol.member == "*" {
                wildcard_symbol_changes
                    .entry(symbol.package.clone())
                    .or_default()
                    .push(change.change_id.clone());
            } else {
                symbol_changes
                    .entry((symbol.package.clone(), symbol.member.to_ascii_lowercase()))
                    .or_default()
                    .push(change.change_id.clone());
            }
        }
    }
    for binding in all_bindings {
        let mut binding_matched = false;
        if let Some(change_ids) = operation_changes.get(&binding.operation_node_id) {
            binding_matched = true;
            matched_changes.extend(change_ids.iter().cloned());
        }
        if let Some(package) = &binding.sdk_package {
            if let Some(change_ids) = wildcard_symbol_changes.get(package) {
                binding_matched = true;
                matched_changes.extend(change_ids.iter().cloned());
            }
            if let Some(member) = &binding.sdk_member {
                if let Some(change_ids) =
                    symbol_changes.get(&(package.clone(), member.to_ascii_lowercase()))
                {
                    binding_matched = true;
                    matched_changes.extend(change_ids.iter().cloned());
                }
            }
        }
        if binding_matched {
            bindings.push(binding);
        }
    }
    if bindings.is_empty() {
        return assessment(
            event,
            ApplicabilityState::NotApplicable,
            vec![ApplicabilityReason::NoObservedUsage],
        );
    }

    let mut review_reasons = Vec::new();
    if inventory
        .ambiguous
        .iter()
        .any(|entry| entry.vendor_ids.iter().any(|id| id == &event.vendor))
    {
        review_reasons.push(ApplicabilityReason::AmbiguousVendor);
    }
    if bindings
        .iter()
        .any(|binding| binding.confidence < vendor.auto_repair_confidence)
    {
        review_reasons.push(ApplicabilityReason::BindingConfidenceTooLow);
    }
    if event
        .changes
        .iter()
        .filter(|change| matched_changes.contains(&change.change_id))
        .any(|change| change.confidence < vendor.auto_repair_confidence)
    {
        review_reasons.push(ApplicabilityReason::ChangeConfidenceTooLow);
    }
    if !allowed_paths.is_empty()
        && bindings
            .iter()
            .any(|binding| !path_allowed(&binding.source_file, allowed_paths))
    {
        review_reasons.push(ApplicabilityReason::UsageOutsideAllowedScope);
    }

    let mut versions = bindings
        .iter()
        .flat_map(|binding| [binding.sdk_version.clone(), binding.api_version.clone()])
        .flatten()
        .collect::<BTreeSet<_>>();
    versions.extend(
        inventory
            .matched
            .iter()
            .filter(|entry| entry.vendor_id == event.vendor)
            .filter_map(|entry| entry.dependency.resolved_version.clone()),
    );
    let applicable_changes = event
        .changes
        .iter()
        .filter(|change| matched_changes.contains(&change.change_id))
        .collect::<Vec<_>>();
    let version_is_universal = applicable_changes
        .iter()
        .all(|change| change.affected_versions.requirement.trim() == "*");
    let mut any_in_range = version_is_universal;
    let mut all_known_outside = !versions.is_empty() && !version_is_universal;
    for change in &applicable_changes {
        for version in &versions {
            match change.affected_versions.contains(version) {
                Some(true) => {
                    any_in_range = true;
                    all_known_outside = false;
                }
                Some(false) => {}
                None => all_known_outside = false,
            }
        }
    }
    if all_known_outside && !any_in_range {
        let mut result = assessment(
            event,
            ApplicabilityState::NotApplicable,
            vec![ApplicabilityReason::VersionOutsideRange],
        );
        result.observed_versions = versions.into_iter().collect();
        return result;
    }
    if !any_in_range && registry.config().require_resolved_version {
        review_reasons.push(ApplicabilityReason::VersionUnknown);
    }

    // `usage_bindings` already establishes this order, and filtering preserves it.
    let state = if review_reasons.is_empty() {
        ApplicabilityState::Applicable
    } else {
        ApplicabilityState::ReviewRequired
    };
    if review_reasons.is_empty() {
        review_reasons.push(ApplicabilityReason::Applicable);
    }
    let mut seed_node_ids = bindings
        .iter()
        .map(|binding| binding.operation_node_id.clone())
        .collect::<Vec<_>>();
    seed_node_ids.sort();
    seed_node_ids.dedup();
    RelevanceAssessment {
        version: 1,
        event_id: event.id.clone(),
        vendor: event.vendor.clone(),
        state,
        reasons: review_reasons,
        matched_change_ids: matched_changes.into_iter().collect(),
        bindings,
        seed_node_ids,
        observed_versions: versions.into_iter().collect(),
    }
}

fn assessment(
    event: &ApiChangeEvent,
    state: ApplicabilityState,
    reasons: Vec<ApplicabilityReason>,
) -> RelevanceAssessment {
    RelevanceAssessment {
        version: 1,
        event_id: event.id.clone(),
        vendor: event.vendor.clone(),
        state,
        reasons,
        matched_change_ids: Vec::new(),
        bindings: Vec::new(),
        seed_node_ids: Vec::new(),
        observed_versions: Vec::new(),
    }
}

fn path_allowed(path: &str, allowed_paths: &[String]) -> bool {
    let normalized = path.replace('\\', "/");
    allowed_paths.iter().any(|prefix| {
        let prefix = prefix.trim_start_matches("./").replace('\\', "/");
        normalized == prefix.trim_end_matches('/')
            || normalized.starts_with(&format!("{}/", prefix.trim_end_matches('/')))
    })
}

pub fn usage_bindings(graph: &GraphData, vendor: &str) -> Vec<ApiUsageBinding> {
    let operation_ids = graph
        .nodes
        .iter()
        .filter(|node| {
            node.extra.get("_node_type").and_then(Value::as_str) == Some("api_operation")
                && node.extra.get("vendor").and_then(Value::as_str) == Some(vendor)
        })
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    let mut bindings = graph
        .links
        .iter()
        .filter(|edge| edge.relation == "uses_api" && operation_ids.contains(&edge.target))
        .flat_map(|edge| {
            edge.sites().into_iter().map(move |site| ApiUsageBinding {
                vendor: vendor.into(),
                operation_node_id: edge.target.0.clone(),
                caller_node_id: edge.source.0.clone(),
                source_file: site.source_file.replace('\\', "/"),
                source_location: site.source_location,
                sdk_package: string_extra(&edge.extra, "sdk_package"),
                sdk_member: string_extra(&edge.extra, "sdk_member_chain"),
                sdk_version: string_extra(&edge.extra, "installed_sdk_version"),
                api_version: string_extra(&edge.extra, "observed_api_version"),
                basis: match string_extra(&edge.extra, "binding_basis").as_deref() {
                    Some("absolute_url_host") => BindingBasis::AbsoluteUrlHost,
                    Some("sdk_symbol") => BindingBasis::SdkSymbol,
                    Some("generated_client") => BindingBasis::GeneratedClient,
                    Some("configured_host") => BindingBasis::ConfiguredHost,
                    _ => BindingBasis::Unknown,
                },
                confidence: edge
                    .confidence_score
                    .unwrap_or_else(|| edge.confidence.default_score()),
            })
        })
        .collect::<Vec<_>>();
    bindings.sort_by(|a, b| {
        a.source_file
            .cmp(&b.source_file)
            .then_with(|| a.caller_node_id.cmp(&b.caller_node_id))
    });
    bindings
}

fn string_extra(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    map.get(key).and_then(Value::as_str).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_bindings_expand_every_deduplicated_source_site() {
        let graph: GraphData = serde_json::from_value(serde_json::json!({
            "directed": true,
            "nodes": [{
                "id": "api:customers", "label": "customers", "file_type": "concept",
                "source_file": "", "_node_type": "api_operation", "vendor": "stripe"
            }],
            "links": [{
                "source": "fn:create", "target": "api:customers", "relation": "uses_api",
                "confidence": "INFERRED", "source_file": "src/Client.cs",
                "source_location": "L1", "api_vendor": "stripe",
                "binding_basis": "sdk_symbol", "sdk_package": "nuget:stripe.net",
                "sdk_member_chain": "StripeClient.Create",
                "sites": [{"source_file":"src/Client.razor","source_location":"L2"}]
            }]
        }))
        .unwrap();

        let bindings = usage_bindings(&graph, "stripe");

        assert_eq!(
            bindings
                .iter()
                .map(|binding| binding.source_file.as_str())
                .collect::<Vec<_>>(),
            vec!["src/Client.cs", "src/Client.razor"]
        );
    }
}
