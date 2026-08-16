use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use synaptic_core::{GraphData, Visibility};

use crate::{
    ApiBreakingChange, ApiChangeEvent, ApiContract, BreakingChangeKind, Ecosystem, EvidenceSpan,
    FetchedArtifact, SdkBindingRule, SdkSymbolAnchor, SourceArtifact, VendorConfig, VersionRange,
    diff_contracts, normalize_openapi, sanitize_release_text,
};

/// Vendor-specific behavior is deliberately narrow. Most implementations are
/// configured compositions of the reusable adapters below.
pub trait VendorAdapter {
    fn id(&self) -> &str;
    fn package_matchers(&self) -> &[crate::PackageCoordinate];
    fn host_matchers(&self) -> &[String];
    fn normalize_contract(&self, artifact: &FetchedArtifact) -> Result<ApiContract, AdapterError>;
    fn diff(
        &self,
        old: &ApiContract,
        new: &ApiContract,
        source: SourceArtifact,
        affected_versions: VersionRange,
    ) -> Result<ApiChangeEvent, AdapterError>;
    fn sdk_bindings(&self, ecosystem: Ecosystem) -> Vec<&SdkBindingRule>;
    fn migration_hints(&self, change: &ApiBreakingChange) -> Vec<String>;
}

#[derive(Debug, Clone, Copy)]
pub struct ConfiguredVendorAdapter<'a> {
    config: &'a VendorConfig,
}

impl<'a> ConfiguredVendorAdapter<'a> {
    pub fn new(config: &'a VendorConfig) -> Self {
        Self { config }
    }
}

impl VendorAdapter for ConfiguredVendorAdapter<'_> {
    fn id(&self) -> &str {
        &self.config.id
    }

    fn package_matchers(&self) -> &[crate::PackageCoordinate] {
        &self.config.packages
    }

    fn host_matchers(&self) -> &[String] {
        &self.config.hosts
    }

    fn normalize_contract(&self, artifact: &FetchedArtifact) -> Result<ApiContract, AdapterError> {
        OpenApiAdapter::new(self.id()).normalize_contract(artifact)
    }

    fn diff(
        &self,
        old: &ApiContract,
        new: &ApiContract,
        source: SourceArtifact,
        affected_versions: VersionRange,
    ) -> Result<ApiChangeEvent, AdapterError> {
        OpenApiAdapter::new(self.id()).diff(old, new, source, affected_versions)
    }

    fn sdk_bindings(&self, ecosystem: Ecosystem) -> Vec<&SdkBindingRule> {
        self.config
            .sdk_bindings
            .iter()
            .filter(|binding| binding.package.ecosystem == ecosystem)
            .collect()
    }

    fn migration_hints(&self, change: &ApiBreakingChange) -> Vec<String> {
        let replacement = change
            .new_operation
            .as_ref()
            .map(|operation| {
                format!(
                    "replace uses with {} {}",
                    operation.method, operation.canonical_path
                )
            })
            .into_iter();
        std::iter::once(change.migration_summary.clone())
            .chain(replacement)
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct OpenApiAdapter {
    vendor: String,
}

impl OpenApiAdapter {
    pub fn new(vendor: impl Into<String>) -> Self {
        Self {
            vendor: vendor.into().trim().to_ascii_lowercase(),
        }
    }
}

impl VendorAdapter for OpenApiAdapter {
    fn id(&self) -> &str {
        &self.vendor
    }

    fn package_matchers(&self) -> &[crate::PackageCoordinate] {
        &[]
    }

    fn host_matchers(&self) -> &[String] {
        &[]
    }

    fn normalize_contract(&self, artifact: &FetchedArtifact) -> Result<ApiContract, AdapterError> {
        Ok(normalize_openapi(&self.vendor, &artifact.bytes)?)
    }

    fn diff(
        &self,
        old: &ApiContract,
        new: &ApiContract,
        source: SourceArtifact,
        affected_versions: VersionRange,
    ) -> Result<ApiChangeEvent, AdapterError> {
        Ok(diff_contracts(old, new, source, affected_versions)?)
    }

    fn sdk_bindings(&self, _ecosystem: Ecosystem) -> Vec<&SdkBindingRule> {
        Vec::new()
    }

    fn migration_hints(&self, change: &ApiBreakingChange) -> Vec<String> {
        vec![change.migration_summary.clone()]
    }
}

/// Checked-in contracts use the same deterministic OpenAPI semantics.
pub type StaticAdapter = OpenApiAdapter;

#[derive(Debug, Clone, Copy, Default)]
pub struct ChangelogAdapter;

impl ChangelogAdapter {
    pub fn review_candidate(&self, artifact: &FetchedArtifact) -> Option<String> {
        let summary = sanitize_release_text(&String::from_utf8_lossy(&artifact.bytes));
        let lowercase = summary.to_ascii_lowercase();
        [
            "breaking",
            "removed",
            "no longer",
            "deprecated",
            "minimum supported",
        ]
        .iter()
        .any(|needle| lowercase.contains(needle))
        .then_some(summary)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdkSurface {
    pub version: u32,
    pub vendor: String,
    pub package: String,
    pub release: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub minimum_supported_version: Option<String>,
    pub digest: String,
    pub exports: BTreeMap<String, String>,
    #[serde(default = "complete_by_default")]
    pub complete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub losses: Vec<SdkSurfaceLoss>,
}

fn complete_by_default() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdkSurfaceLoss {
    pub source_file: String,
    pub symbol: String,
    pub reason: String,
}

impl SdkSurface {
    const VERSION: u32 = 1;
}

/// Build a package public surface from Synaptic's language-neutral graph. This
/// uses the same typed visibility/signature metadata emitted by every extractor,
/// so adding a language parser does not require an SDK-specific parser branch.
pub fn extract_sdk_surface_from_graph(
    vendor: &str,
    package: &str,
    release: &str,
    minimum_supported_version: Option<String>,
    graph: &GraphData,
) -> Result<SdkSurface, AdapterError> {
    if vendor.trim().is_empty() || package.trim().is_empty() || release.trim().is_empty() {
        return Err(AdapterError::InvalidSurface);
    }
    let explicitly_exported = graph
        .links
        .iter()
        .filter(|edge| matches!(edge.relation.as_str(), "exports" | "re_exports"))
        .map(|edge| &edge.target)
        .collect::<std::collections::BTreeSet<_>>();
    let public = graph
        .nodes
        .iter()
        .filter(|node| {
            node.visibility() == Some(Visibility::Public) || explicitly_exported.contains(&node.id)
        })
        .filter(|node| !node.is_test())
        .collect::<Vec<_>>();
    let mut counts = BTreeMap::<String, usize>::new();
    for node in &public {
        *counts.entry(node.label.clone()).or_default() += 1;
    }
    let mut exports = BTreeMap::new();
    let mut losses = Vec::new();
    for (symbol, count) in counts.iter().filter(|(_, count)| **count > 1) {
        losses.push(SdkSurfaceLoss {
            source_file: String::new(),
            symbol: symbol.clone(),
            reason: format!("{count} public declarations share this unqualified symbol"),
        });
    }
    for node in public {
        let symbol = if counts.get(&node.label).copied().unwrap_or(0) > 1 {
            format!("{}::{}", node.source_file.replace('\\', "/"), node.label)
        } else {
            node.label.clone()
        };
        let signature = match node.signature() {
            Some(signature) => signature.raw.trim().to_string(),
            None => {
                losses.push(SdkSurfaceLoss {
                    source_file: node.source_file.to_string(),
                    symbol: symbol.clone(),
                    reason: "public symbol has no extracted signature".into(),
                });
                "unknown".into()
            }
        };
        exports.insert(symbol, signature);
    }
    losses.sort_by(|left, right| {
        left.symbol
            .cmp(&right.symbol)
            .then_with(|| left.source_file.cmp(&right.source_file))
    });
    let canonical = serde_json::to_vec(&(
        vendor.trim().to_ascii_lowercase(),
        package,
        release,
        &minimum_supported_version,
        &exports,
        &losses,
    ))?;
    Ok(SdkSurface {
        version: SdkSurface::VERSION,
        vendor: vendor.trim().to_ascii_lowercase(),
        package: package.into(),
        release: release.into(),
        minimum_supported_version,
        digest: blake3::hash(&canonical).to_hex().to_string(),
        exports,
        complete: losses.is_empty(),
        losses,
    })
}

#[derive(Debug, Clone)]
pub struct PackageReleaseAdapter {
    vendor: String,
    package: String,
}

impl PackageReleaseAdapter {
    pub fn new(vendor: impl Into<String>, package: impl Into<String>) -> Self {
        Self {
            vendor: vendor.into().trim().to_ascii_lowercase(),
            package: package.into(),
        }
    }

    pub fn normalize_surface(
        &self,
        artifact: &FetchedArtifact,
    ) -> Result<SdkSurface, AdapterError> {
        if artifact.bytes.len() > 10 * 1024 * 1024 {
            return Err(AdapterError::TooLarge(artifact.bytes.len()));
        }
        #[derive(Deserialize)]
        struct RawSurface {
            version: String,
            #[serde(default)]
            minimum_supported_version: Option<String>,
            exports: BTreeMap<String, String>,
        }
        let raw: RawSurface = serde_json::from_slice(&artifact.bytes)?;
        if raw.version.trim().is_empty()
            || raw
                .exports
                .iter()
                .any(|(symbol, signature)| symbol.trim().is_empty() || signature.len() > 4_096)
        {
            return Err(AdapterError::InvalidSurface);
        }
        if raw
            .minimum_supported_version
            .as_deref()
            .is_some_and(|version| semver::Version::parse(version).is_err())
        {
            return Err(AdapterError::InvalidSurface);
        }
        let canonical = serde_json::to_vec(&(
            &self.vendor,
            &self.package,
            &raw.version,
            &raw.minimum_supported_version,
            &raw.exports,
        ))?;
        Ok(SdkSurface {
            version: SdkSurface::VERSION,
            vendor: self.vendor.clone(),
            package: self.package.clone(),
            release: raw.version,
            minimum_supported_version: raw.minimum_supported_version,
            digest: blake3::hash(&canonical).to_hex().to_string(),
            exports: raw.exports,
            complete: true,
            losses: Vec::new(),
        })
    }

    pub fn diff_surfaces(
        &self,
        old: &SdkSurface,
        new: &SdkSurface,
        source: SourceArtifact,
        affected_versions: VersionRange,
    ) -> Result<Vec<ApiBreakingChange>, AdapterError> {
        if old.vendor != self.vendor
            || new.vendor != self.vendor
            || old.package != self.package
            || new.package != self.package
        {
            return Err(AdapterError::SurfaceIdentity);
        }
        let mut changes = Vec::new();
        for (symbol, old_signature) in &old.exports {
            match new.exports.get(symbol) {
                None => changes.push(sdk_change(
                    BreakingChangeKind::SdkExportRemoved,
                    &self.package,
                    symbol,
                    Some(old_signature),
                    None,
                    &source,
                    &affected_versions,
                )),
                Some(new_signature) if new_signature != old_signature => changes.push(sdk_change(
                    BreakingChangeKind::SdkSignatureChanged,
                    &self.package,
                    symbol,
                    Some(old_signature),
                    Some(new_signature),
                    &source,
                    &affected_versions,
                )),
                Some(_) => {}
            }
        }
        if let (Some(old_minimum), Some(new_minimum)) = (
            old.minimum_supported_version.as_deref(),
            new.minimum_supported_version.as_deref(),
        ) {
            let old_version =
                semver::Version::parse(old_minimum).map_err(|_| AdapterError::InvalidSurface)?;
            let new_version =
                semver::Version::parse(new_minimum).map_err(|_| AdapterError::InvalidSurface)?;
            if new_version > old_version {
                changes.push(minimum_version_change(
                    &self.package,
                    old_minimum,
                    new_minimum,
                    &source,
                    &affected_versions,
                ));
            }
        }
        changes.sort_by(|a, b| a.change_id.cmp(&b.change_id));
        Ok(changes)
    }

    pub fn diff_event(
        &self,
        old: &SdkSurface,
        new: &SdkSurface,
        source: SourceArtifact,
        affected_versions: VersionRange,
    ) -> Result<ApiChangeEvent, AdapterError> {
        let changes = self.diff_surfaces(old, new, source.clone(), affected_versions)?;
        let identity = serde_json::to_vec(&(
            &self.vendor,
            &source.revision,
            &old.digest,
            &new.digest,
            &changes,
        ))?;
        let digest = blake3::hash(&identity).to_hex().to_string();
        Ok(ApiChangeEvent {
            version: ApiChangeEvent::VERSION,
            id: format!("api_event_{}_{}", self.vendor, &digest[..24]),
            vendor: self.vendor.clone(),
            release: Some(new.release.clone()),
            occurred_at: source.fetched_at,
            source,
            changes,
        })
    }
}

fn minimum_version_change(
    package: &str,
    old_version: &str,
    new_version: &str,
    source: &SourceArtifact,
    affected_versions: &VersionRange,
) -> ApiBreakingChange {
    let summary = format!(
        "minimum supported version for {package} increased from {old_version} to {new_version}"
    );
    let pointer = "/minimum_supported_version".to_string();
    let evidence_digest =
        blake3::hash(format!("{}\0{}\0{}", source.uri, pointer, summary).as_bytes())
            .to_hex()
            .to_string();
    let identity = blake3::hash(
        format!("minimum\0{package}\0{old_version}\0{new_version}\0{evidence_digest}").as_bytes(),
    )
    .to_hex()
    .to_string();
    ApiBreakingChange {
        change_id: format!("change_{}", &identity[..24]),
        kind: BreakingChangeKind::MinimumSupportedVersionRaised,
        affected_versions: affected_versions.clone(),
        old_operation: None,
        new_operation: None,
        old_sdk_symbols: vec![SdkSymbolAnchor {
            package: package.into(),
            member: "*".into(),
            signature: Some(old_version.into()),
        }],
        new_sdk_symbols: vec![SdkSymbolAnchor {
            package: package.into(),
            member: "*".into(),
            signature: Some(new_version.into()),
        }],
        migration_summary: summary.clone(),
        evidence: vec![EvidenceSpan {
            source_uri: source.uri.clone(),
            pointer,
            summary,
            digest: evidence_digest,
        }],
        confidence: 1.0,
    }
}

fn sdk_change(
    kind: BreakingChangeKind,
    package: &str,
    symbol: &str,
    old_signature: Option<&String>,
    new_signature: Option<&String>,
    source: &SourceArtifact,
    affected_versions: &VersionRange,
) -> ApiBreakingChange {
    let summary = match kind {
        BreakingChangeKind::SdkExportRemoved => {
            format!("SDK export {package}:{symbol} was removed")
        }
        _ => format!("SDK signature changed for {package}:{symbol}"),
    };
    let pointer = format!("/exports/{symbol}");
    let evidence_digest =
        blake3::hash(format!("{}\0{}\0{}", source.uri, pointer, summary).as_bytes())
            .to_hex()
            .to_string();
    let change_digest =
        blake3::hash(format!("{kind:?}\0{package}\0{symbol}\0{evidence_digest}").as_bytes())
            .to_hex()
            .to_string();
    ApiBreakingChange {
        change_id: format!("change_{}", &change_digest[..24]),
        kind,
        affected_versions: affected_versions.clone(),
        old_operation: None,
        new_operation: None,
        old_sdk_symbols: vec![SdkSymbolAnchor {
            package: package.into(),
            member: symbol.into(),
            signature: old_signature.cloned(),
        }],
        new_sdk_symbols: new_signature
            .map(|signature| SdkSymbolAnchor {
                package: package.into(),
                member: symbol.into(),
                signature: Some(signature.clone()),
            })
            .into_iter()
            .collect(),
        migration_summary: summary.clone(),
        evidence: vec![EvidenceSpan {
            source_uri: source.uri.clone(),
            pointer,
            summary,
            digest: evidence_digest,
        }],
        confidence: 1.0,
    }
}

/// Stripe contributes conventions and mappings through normal configuration;
/// orchestration still uses the generic adapter path.
#[derive(Debug, Clone, Copy)]
pub struct StripeAdapter<'a>(ConfiguredVendorAdapter<'a>);

impl<'a> StripeAdapter<'a> {
    pub fn from_config(config: &'a VendorConfig) -> Result<Self, AdapterError> {
        if config.id != "stripe" {
            return Err(AdapterError::NotStripe(config.id.clone()));
        }
        Ok(Self(ConfiguredVendorAdapter::new(config)))
    }

    pub fn openapi(&self) -> OpenApiAdapter {
        OpenApiAdapter::new(self.id())
    }

    pub fn changelog(&self) -> ChangelogAdapter {
        ChangelogAdapter
    }
}

impl VendorAdapter for StripeAdapter<'_> {
    fn id(&self) -> &str {
        self.0.id()
    }
    fn package_matchers(&self) -> &[crate::PackageCoordinate] {
        self.0.package_matchers()
    }
    fn host_matchers(&self) -> &[String] {
        self.0.host_matchers()
    }
    fn normalize_contract(&self, artifact: &FetchedArtifact) -> Result<ApiContract, AdapterError> {
        self.0.normalize_contract(artifact)
    }
    fn diff(
        &self,
        old: &ApiContract,
        new: &ApiContract,
        source: SourceArtifact,
        affected_versions: VersionRange,
    ) -> Result<ApiChangeEvent, AdapterError> {
        self.0.diff(old, new, source, affected_versions)
    }
    fn sdk_bindings(&self, ecosystem: Ecosystem) -> Vec<&SdkBindingRule> {
        self.0.sdk_bindings(ecosystem)
    }
    fn migration_hints(&self, change: &ApiBreakingChange) -> Vec<String> {
        self.0.migration_hints(change)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("contract adapter failed: {0}")]
    Contract(#[from] crate::ContractError),
    #[error("invalid package-release JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("package-release surface exceeds the cap: {0} bytes")]
    TooLarge(usize),
    #[error("package-release surface has invalid exports or release version")]
    InvalidSurface,
    #[error("SDK surfaces do not belong to this vendor/package adapter")]
    SurfaceIdentity,
    #[error("Stripe adapter cannot be built from vendor {0:?}")]
    NotStripe(String),
}
