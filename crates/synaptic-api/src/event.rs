use std::fmt;
use std::str::FromStr;

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

use crate::ApiOperationAnchor;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionRange {
    pub requirement: String,
}

impl VersionRange {
    pub fn parse(value: &str) -> Result<Self, semver::Error> {
        let requirement = value.trim();
        VersionReq::parse(requirement)?;
        Ok(Self {
            requirement: requirement.to_string(),
        })
    }

    pub fn any() -> Self {
        Self {
            requirement: "*".into(),
        }
    }

    pub fn contains(&self, version: &str) -> Option<bool> {
        let version = normalize_version(version)?;
        let version = Version::parse(version).ok()?;
        VersionReq::parse(&self.requirement)
            .ok()
            .map(|requirement| requirement.matches(&version))
    }
}

impl fmt::Display for VersionRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.requirement)
    }
}

impl FromStr for VersionRange {
    type Err = semver::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

fn normalize_version(value: &str) -> Option<&str> {
    let value = value.trim().trim_start_matches(['v', '=']);
    let end = value
        .char_indices()
        .find(|(_, character)| !character.is_ascii_digit() && !matches!(character, '.' | '-' | '+'))
        .map(|(index, _)| index)
        .unwrap_or(value.len());
    (end > 0).then_some(&value[..end])
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceArtifact {
    pub uri: String,
    pub revision: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub etag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_modified: Option<String>,
    pub content_digest: String,
    pub fetched_at: i64,
    pub adapter_version: u32,
    pub evidence_kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceSpan {
    pub source_uri: String,
    pub pointer: String,
    pub summary: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdkSymbolAnchor {
    pub package: String,
    pub member: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakingChangeKind {
    OperationRemoved,
    OperationRenamed,
    PathOrMethodChanged,
    RequiredRequestFieldAdded,
    RequestFieldRemoved,
    RequestFieldRenamed,
    RequestFieldTypeChanged,
    RequestEnumNarrowed,
    ResponseFieldRemoved,
    ResponseFieldRenamed,
    ResponseFieldTypeChanged,
    ResponseEnumChanged,
    AuthenticationOrVersionBehaviorChanged,
    WebhookChanged,
    SdkExportRemoved,
    SdkSignatureChanged,
    MinimumSupportedVersionRaised,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiBreakingChange {
    pub change_id: String,
    pub kind: BreakingChangeKind,
    pub affected_versions: VersionRange,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub old_operation: Option<ApiOperationAnchor>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub new_operation: Option<ApiOperationAnchor>,
    #[serde(default)]
    pub old_sdk_symbols: Vec<SdkSymbolAnchor>,
    #[serde(default)]
    pub new_sdk_symbols: Vec<SdkSymbolAnchor>,
    pub migration_summary: String,
    #[serde(default)]
    pub evidence: Vec<EvidenceSpan>,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiChangeEvent {
    pub version: u32,
    pub id: String,
    pub vendor: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub release: Option<String>,
    pub occurred_at: i64,
    pub source: SourceArtifact,
    #[serde(default)]
    pub changes: Vec<ApiBreakingChange>,
}

impl ApiChangeEvent {
    pub const VERSION: u32 = 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_range_handles_registry_prefixes_and_unknowns() {
        let range = VersionRange::parse(">=1.2.0, <2.0.0").unwrap();
        assert_eq!(range.contains("v1.8.2"), Some(true));
        assert_eq!(range.contains("2.0.0"), Some(false));
        assert_eq!(range.contains("workspace:*"), None);
    }
}
