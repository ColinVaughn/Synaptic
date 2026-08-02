use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Dependency, PackageCoordinate};

fn enabled_by_default() -> bool {
    true
}

/// Repository policy for discovery, repair, verification, and draft publication.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiMaintenanceConfig {
    pub schema: u32,
    #[serde(default)]
    pub mode: MaintenanceMode,
    #[serde(default = "default_base_branch")]
    pub base_branch: String,
    #[serde(default = "default_max_files")]
    pub max_files: usize,
    #[serde(default = "default_max_changed_lines")]
    pub max_changed_lines: usize,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: usize,
    #[serde(default = "default_max_risk_score")]
    pub max_risk_score: u8,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    #[serde(default)]
    pub allow_workflow_changes: bool,
    #[serde(default)]
    pub allow_generated_changes: bool,
    #[serde(default = "enabled_by_default")]
    pub require_resolved_version: bool,
    #[serde(default = "enabled_by_default")]
    pub require_graph_invariants: bool,
    #[serde(default = "enabled_by_default")]
    pub require_tests: bool,
    #[serde(default)]
    pub commands: CommandPolicy,
    #[serde(default)]
    pub publish: PublishPolicy,
    #[serde(default)]
    pub coverage: CoveragePolicy,
    #[serde(default)]
    pub vendors: Vec<VendorConfig>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaintenanceMode {
    Report,
    #[default]
    DraftPr,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandPolicy {
    #[serde(default)]
    pub check: Option<String>,
    #[serde(default)]
    pub test: Option<String>,
    #[serde(default)]
    pub policy: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishPolicy {
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub reviewers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoveragePolicy {
    #[serde(default = "default_runtime_min_observations")]
    pub runtime_min_observations: usize,
    #[serde(default)]
    pub waivers: Vec<CoverageWaiver>,
}

impl Default for CoveragePolicy {
    fn default() -> Self {
        Self {
            runtime_min_observations: default_runtime_min_observations(),
            waivers: Vec::new(),
        }
    }
}

const fn default_runtime_min_observations() -> usize {
    2
}

/// A review decision tied to exact evidence. If the observed call changes, its
/// digest changes and this waiver no longer applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageWaiver {
    pub observation_id: String,
    pub evidence_digest: String,
    pub reason: String,
}

fn default_base_branch() -> String {
    "main".into()
}

const fn default_max_files() -> usize {
    12
}

const fn default_max_changed_lines() -> usize {
    800
}

const fn default_max_attempts() -> usize {
    3
}

const fn default_max_risk_score() -> u8 {
    80
}

impl ApiMaintenanceConfig {
    pub const SCHEMA: u32 = 1;

    pub fn parse(source: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(source)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema != Self::SCHEMA {
            return Err(ConfigError::UnsupportedSchema(self.schema));
        }
        if self.max_files == 0
            || self.max_changed_lines == 0
            || self.max_attempts == 0
            || self.max_attempts > 3
            || self.max_risk_score > 100
            || self.coverage.runtime_min_observations == 0
            || self.coverage.runtime_min_observations > 10_000
        {
            return Err(ConfigError::InvalidLimits);
        }
        let mut waived_observations = BTreeSet::new();
        for waiver in &self.coverage.waivers {
            let valid_id = waiver.observation_id.starts_with("external_surface_")
                && waiver
                    .observation_id
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_');
            let valid_digest = waiver.evidence_digest.len() == 64
                && waiver
                    .evidence_digest
                    .chars()
                    .all(|character| character.is_ascii_hexdigit());
            if !valid_id
                || !valid_digest
                || waiver.reason.trim().is_empty()
                || waiver.reason.len() > 512
                || !waived_observations.insert(waiver.observation_id.clone())
            {
                return Err(ConfigError::InvalidCoverageWaiver(
                    waiver.observation_id.clone(),
                ));
            }
        }
        let mut ids = BTreeSet::new();
        for vendor in &self.vendors {
            let id = normalized_vendor_id(&vendor.id)?;
            if !ids.insert(id.clone()) {
                return Err(ConfigError::DuplicateVendor(id));
            }
            if vendor.packages.is_empty()
                && vendor.hosts.is_empty()
                && vendor.sdk_bindings.is_empty()
                && vendor.sources.is_empty()
            {
                return Err(ConfigError::EmptyMatchers(id));
            }
            for host in &vendor.hosts {
                validate_host(host, &id)?;
            }
            for binding in &vendor.sdk_bindings {
                if binding.member.trim().is_empty()
                    || binding
                        .imports
                        .iter()
                        .any(|import| import.trim().is_empty())
                    || binding.protocol.trim().is_empty()
                    || binding.method.trim().is_empty()
                    || binding.path.trim().is_empty()
                {
                    return Err(ConfigError::InvalidSdkBinding {
                        vendor: id.clone(),
                        member: binding.member.clone(),
                    });
                }
            }
            if !(0.0..=1.0).contains(&vendor.auto_repair_confidence) {
                return Err(ConfigError::InvalidConfidence {
                    vendor: id.clone(),
                    confidence: vendor.auto_repair_confidence,
                });
            }
            for source in &vendor.sources {
                source.validate(&id)?;
            }
        }
        Ok(())
    }
}

pub fn maintenance_policy_digest(
    config: &ApiMaintenanceConfig,
) -> Result<String, serde_json::Error> {
    Ok(blake3::hash(&serde_json::to_vec(config)?)
        .to_hex()
        .to_string())
}

pub const DEFAULT_CONFIG_PATH: &str = ".synaptic/api-maintenance.toml";

/// Load the conventional repository-local API maintenance configuration.
/// Repositories that have not opted in return `None`; an unreadable or invalid
/// config fails explicitly.
pub fn load_optional_registry(root: &Path) -> Result<Option<VendorRegistry>, ConfigLoadError> {
    let path = root.join(DEFAULT_CONFIG_PATH);
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(ConfigLoadError::Read { path, source }),
    };
    let config = ApiMaintenanceConfig::parse(&source).map_err(|source| ConfigLoadError::Parse {
        path: path.clone(),
        source: Box::new(source),
    })?;
    VendorRegistry::new(config)
        .map(Some)
        .map_err(|source| ConfigLoadError::Parse {
            path,
            source: Box::new(source),
        })
}

/// One vendor's package and host matchers. No vendor is special in this model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VendorConfig {
    pub id: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    #[serde(default)]
    pub packages: Vec<PackageCoordinate>,
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub sdk_bindings: Vec<SdkBindingRule>,
    #[serde(default)]
    pub sources: Vec<VendorSource>,
    #[serde(default = "default_auto_repair_confidence")]
    pub auto_repair_confidence: f32,
}

fn default_auto_repair_confidence() -> f32 {
    0.92
}

fn default_contract_cap() -> u64 {
    10 * 1024 * 1024
}

fn default_text_cap() -> u64 {
    1024 * 1024
}

fn default_poll_interval() -> u64 {
    60
}

/// Official or checked-in source consumed by the generic scanner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VendorSource {
    OpenApi {
        uri: String,
        #[serde(default = "default_affected_versions")]
        affected_versions: String,
        #[serde(default = "default_contract_cap")]
        max_bytes: u64,
        #[serde(default = "default_poll_interval")]
        min_poll_interval_seconds: u64,
    },
    Changelog {
        uri: String,
        #[serde(default = "default_text_cap")]
        max_bytes: u64,
        #[serde(default = "default_poll_interval")]
        min_poll_interval_seconds: u64,
    },
    PackageRelease {
        uri: String,
        package: PackageCoordinate,
        #[serde(default = "default_affected_versions")]
        affected_versions: String,
        #[serde(default = "default_text_cap")]
        max_bytes: u64,
        #[serde(default = "default_poll_interval")]
        min_poll_interval_seconds: u64,
    },
    StaticContract {
        path: PathBuf,
        #[serde(default = "default_affected_versions")]
        affected_versions: String,
        #[serde(default = "default_contract_cap")]
        max_bytes: u64,
    },
    Webhook {
        path: PathBuf,
        #[serde(default = "default_affected_versions")]
        affected_versions: String,
        #[serde(default = "default_contract_cap")]
        max_bytes: u64,
    },
}

fn default_affected_versions() -> String {
    "*".into()
}

impl VendorSource {
    pub fn location(&self) -> String {
        match self {
            Self::OpenApi { uri, .. }
            | Self::Changelog { uri, .. }
            | Self::PackageRelease { uri, .. } => uri.clone(),
            Self::StaticContract { path, .. } | Self::Webhook { path, .. } => {
                path.to_string_lossy().into_owned()
            }
        }
    }

    pub fn max_bytes(&self) -> u64 {
        match self {
            Self::OpenApi { max_bytes, .. }
            | Self::Changelog { max_bytes, .. }
            | Self::PackageRelease { max_bytes, .. }
            | Self::StaticContract { max_bytes, .. }
            | Self::Webhook { max_bytes, .. } => *max_bytes,
        }
    }

    pub fn affected_versions(&self) -> Option<&str> {
        match self {
            Self::OpenApi {
                affected_versions, ..
            }
            | Self::PackageRelease {
                affected_versions, ..
            }
            | Self::StaticContract {
                affected_versions, ..
            }
            | Self::Webhook {
                affected_versions, ..
            } => Some(affected_versions),
            Self::Changelog { .. } => None,
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::StaticContract { .. } | Self::Webhook { .. })
    }

    pub fn min_poll_interval_seconds(&self) -> u64 {
        match self {
            Self::OpenApi {
                min_poll_interval_seconds,
                ..
            }
            | Self::Changelog {
                min_poll_interval_seconds,
                ..
            }
            | Self::PackageRelease {
                min_poll_interval_seconds,
                ..
            } => *min_poll_interval_seconds,
            Self::StaticContract { .. } | Self::Webhook { .. } => 0,
        }
    }

    fn validate(&self, vendor: &str) -> Result<(), ConfigError> {
        if self.max_bytes() == 0 || self.location().trim().is_empty() {
            return Err(ConfigError::InvalidSource {
                vendor: vendor.into(),
                location: self.location(),
            });
        }
        if let Some(requirement) = self.affected_versions() {
            semver::VersionReq::parse(requirement).map_err(|_| {
                ConfigError::InvalidVersionRange {
                    vendor: vendor.into(),
                    requirement: requirement.into(),
                }
            })?;
        }
        Ok(())
    }
}

/// An exact package/member-chain mapping supplied by a generic or vendor adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdkBindingRule {
    pub package: PackageCoordinate,
    /// Source-level import/module namespaces accepted for this package. Registry
    /// artifact names and code namespaces frequently differ (Maven, NuGet, Hex,
    /// SwiftPM), so adapters declare that relationship explicitly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<String>,
    pub member: String,
    #[serde(default = "default_sdk_protocol")]
    pub protocol: String,
    pub method: String,
    pub path: String,
}

fn default_sdk_protocol() -> String {
    "https".into()
}

/// A fail-closed dependency-to-vendor decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum VendorMatch {
    Unmatched,
    Matched { vendor_id: String },
    Ambiguous { vendor_ids: Vec<String> },
}

/// Pre-indexed vendor configuration used by repository inventory.
#[derive(Debug, Clone)]
pub struct VendorRegistry {
    config: ApiMaintenanceConfig,
    by_package: BTreeMap<PackageCoordinate, Vec<String>>,
    by_host: BTreeMap<String, Vec<String>>,
}

impl VendorRegistry {
    pub fn new(mut config: ApiMaintenanceConfig) -> Result<Self, ConfigError> {
        config.validate()?;
        let mut by_package: BTreeMap<PackageCoordinate, Vec<String>> = BTreeMap::new();
        let mut by_host: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for vendor in &mut config.vendors {
            vendor.id = normalized_vendor_id(&vendor.id)?;
            vendor.hosts = vendor
                .hosts
                .iter()
                .map(|host| normalize_host(host))
                .collect();
            vendor.packages.sort();
            vendor.packages.dedup();
            for binding in &mut vendor.sdk_bindings {
                binding.imports = binding
                    .imports
                    .iter()
                    .map(|import| import.trim().to_string())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
            }
            vendor.sdk_bindings.sort_by(|a, b| {
                a.package
                    .cmp(&b.package)
                    .then_with(|| a.member.cmp(&b.member))
            });
            vendor.sdk_bindings.dedup();
            if !vendor.enabled {
                continue;
            }
            for package in vendor
                .packages
                .iter()
                .chain(vendor.sdk_bindings.iter().map(|binding| &binding.package))
            {
                by_package
                    .entry(package.clone())
                    .or_default()
                    .push(vendor.id.clone());
            }
            for host in &vendor.hosts {
                by_host
                    .entry(host.clone())
                    .or_default()
                    .push(vendor.id.clone());
            }
        }
        for ids in by_package.values_mut().chain(by_host.values_mut()) {
            ids.sort();
            ids.dedup();
        }
        Ok(Self {
            config,
            by_package,
            by_host,
        })
    }

    pub fn config(&self) -> &ApiMaintenanceConfig {
        &self.config
    }

    pub fn match_dependency(&self, dependency: &Dependency) -> VendorMatch {
        self.match_package(&dependency.package)
    }

    pub fn match_package(&self, package: &PackageCoordinate) -> VendorMatch {
        match self.by_package.get(package) {
            None => VendorMatch::Unmatched,
            Some(ids) if ids.len() == 1 => VendorMatch::Matched {
                vendor_id: ids[0].clone(),
            },
            Some(ids) => VendorMatch::Ambiguous {
                vendor_ids: ids.clone(),
            },
        }
    }

    /// Match an absolute HTTP authority to a configured vendor. Overlapping host
    /// claims are reported as ambiguous rather than selecting a vendor by order.
    pub fn match_host(&self, host: &str) -> VendorMatch {
        match self.by_host.get(&normalize_host(host)) {
            None => VendorMatch::Unmatched,
            Some(ids) if ids.len() == 1 => VendorMatch::Matched {
                vendor_id: ids[0].clone(),
            },
            Some(ids) => VendorMatch::Ambiguous {
                vendor_ids: ids.clone(),
            },
        }
    }

    pub fn vendor(&self, id: &str) -> Option<&VendorConfig> {
        self.config.vendors.iter().find(|vendor| vendor.id == id)
    }

    pub fn sdk_binding(
        &self,
        vendor_id: &str,
        package: &PackageCoordinate,
        member: &str,
    ) -> Option<&SdkBindingRule> {
        self.vendor(vendor_id)?.sdk_bindings.iter().find(|binding| {
            &binding.package == package && binding.member.eq_ignore_ascii_case(member)
        })
    }

    /// Return every enabled rule matching a source import namespace and member.
    /// The binding layer handles cardinality so overlapping rules fail closed.
    pub(crate) fn sdk_bindings_for_import(
        &self,
        ecosystem: crate::Ecosystem,
        import: &str,
        member: &str,
    ) -> Vec<(&str, &SdkBindingRule)> {
        let mut matches = self
            .config
            .vendors
            .iter()
            .filter(|vendor| vendor.enabled)
            .flat_map(|vendor| {
                vendor.sdk_bindings.iter().filter_map(move |binding| {
                    (sdk_ecosystems_compatible(ecosystem, binding.package.ecosystem)
                        && binding.member.eq_ignore_ascii_case(member)
                        && binding
                            .imports
                            .iter()
                            .any(|configured| import_namespace_matches(configured, import)))
                    .then_some((vendor.id.as_str(), binding))
                })
            })
            .collect::<Vec<_>>();
        matches.sort_by(|(left_vendor, left), (right_vendor, right)| {
            left_vendor
                .cmp(right_vendor)
                .then_with(|| left.package.cmp(&right.package))
                .then_with(|| left.member.cmp(&right.member))
        });
        matches.dedup_by(|(left_vendor, left), (right_vendor, right)| {
            left_vendor == right_vendor
                && left.package == right.package
                && left.member.eq_ignore_ascii_case(&right.member)
        });
        matches
    }
}

fn import_namespace_matches(configured: &str, observed: &str) -> bool {
    let configured = configured.trim();
    let observed = observed.trim();
    if configured.eq_ignore_ascii_case(observed) {
        return true;
    }
    observed.get(configured.len()..).is_some_and(|suffix| {
        observed[..configured.len()].eq_ignore_ascii_case(configured)
            && (suffix.starts_with('.')
                || suffix.starts_with("::")
                || suffix.starts_with('\\')
                || suffix.starts_with('/'))
    })
}

fn sdk_ecosystems_compatible(observed: crate::Ecosystem, configured: crate::Ecosystem) -> bool {
    observed == configured
        || matches!(
            (observed, configured),
            (crate::Ecosystem::Conan, crate::Ecosystem::Vcpkg)
                | (crate::Ecosystem::Vcpkg, crate::Ecosystem::Conan)
                | (crate::Ecosystem::Swift, crate::Ecosystem::Cocoapods)
                | (crate::Ecosystem::Cocoapods, crate::Ecosystem::Swift)
        )
}

fn normalized_vendor_id(value: &str) -> Result<String, ConfigError> {
    let id = value.trim().to_ascii_lowercase();
    if id.is_empty()
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        return Err(ConfigError::InvalidVendorId(value.to_string()));
    }
    Ok(id)
}

fn validate_host(host: &str, vendor: &str) -> Result<(), ConfigError> {
    let normalized = normalize_host(host);
    if normalized.is_empty()
        || normalized.contains('/')
        || normalized.contains('@')
        || normalized.contains("..")
    {
        return Err(ConfigError::InvalidHost {
            vendor: vendor.to_string(),
            host: host.to_string(),
        });
    }
    Ok(())
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid API maintenance TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("unsupported API maintenance schema {0}; expected {schema}", schema = ApiMaintenanceConfig::SCHEMA)]
    UnsupportedSchema(u32),
    #[error("invalid vendor id {0:?}; use letters, digits, '-' or '_'")]
    InvalidVendorId(String),
    #[error("vendor {0:?} appears more than once")]
    DuplicateVendor(String),
    #[error("vendor {0:?} needs at least one package, host, SDK binding, or source")]
    EmptyMatchers(String),
    #[error("vendor {vendor:?} has invalid host matcher {host:?}")]
    InvalidHost { vendor: String, host: String },
    #[error("vendor {vendor:?} has an invalid SDK binding for member {member:?}")]
    InvalidSdkBinding { vendor: String, member: String },
    #[error("invalid policy limits (attempts must be 1..=3 and risk must be 0..=100)")]
    InvalidLimits,
    #[error("invalid or duplicate coverage waiver for {0:?}")]
    InvalidCoverageWaiver(String),
    #[error("vendor {vendor:?} has invalid auto-repair confidence {confidence}")]
    InvalidConfidence { vendor: String, confidence: f32 },
    #[error("vendor {vendor:?} has invalid source {location:?}")]
    InvalidSource { vendor: String, location: String },
    #[error("vendor {vendor:?} has invalid affected version range {requirement:?}")]
    InvalidVersionRange { vendor: String, requirement: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigLoadError {
    #[error("reading API maintenance config {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing API maintenance config {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<ConfigError>,
    },
}
