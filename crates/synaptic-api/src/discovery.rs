use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    normalize_contract, ApiMaintenanceConfig, CommandPolicy, CoveragePolicy, MaintenanceMode,
    ParseCompleteness, PublishPolicy, SurfaceFormat, SurfaceLoss, VendorConfig, VendorSource,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractDiscoveryReport {
    pub version: u32,
    pub candidates_scanned: usize,
    pub contracts: Vec<DiscoveredContract>,
    pub rejected: Vec<RejectedContractCandidate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredContract {
    pub path: String,
    pub format: SurfaceFormat,
    pub format_version: String,
    pub digest: String,
    pub operations: usize,
    pub completeness: ParseCompleteness,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub losses: Vec<SurfaceLoss>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedContractCandidate {
    pub path: String,
    pub error: String,
}

pub fn discover_contracts(root: &Path) -> Result<ContractDiscoveryReport, DiscoveryError> {
    if !root.is_dir() {
        return Err(DiscoveryError::InvalidRoot(root.to_path_buf()));
    }
    let mut candidates = Vec::new();
    collect_candidates(root, root, &mut candidates)?;
    candidates.sort();
    let mut contracts = Vec::new();
    let mut rejected = Vec::new();
    for path in &candidates {
        let relative = relative(root, path);
        match normalize_contract("unassigned", &fs::read(path)?) {
            Ok(contract) => contracts.push(DiscoveredContract {
                path: relative,
                format: contract.format,
                format_version: contract.format_version,
                digest: contract.digest,
                operations: contract.operations.len(),
                completeness: contract.completeness,
                losses: contract.losses,
            }),
            Err(error) => rejected.push(RejectedContractCandidate {
                path: relative,
                error: error.to_string(),
            }),
        }
    }
    contracts.sort_by(|left, right| left.path.cmp(&right.path));
    rejected.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(ContractDiscoveryReport {
        version: 1,
        candidates_scanned: candidates.len(),
        contracts,
        rejected,
    })
}

/// Produce a disabled, report-only overlay for human review. Discovery never
/// silently promotes an inferred owner into a monitored vendor; approving a
/// candidate requires choosing its vendor id and setting `enabled = true`.
pub fn candidate_profile_toml(
    report: &ContractDiscoveryReport,
) -> Result<String, toml::ser::Error> {
    let vendors = report
        .contracts
        .iter()
        .map(|contract| {
            let identity = format!("{}\0{}", contract.path, contract.digest);
            let digest = blake3::hash(identity.as_bytes()).to_hex();
            VendorConfig {
                id: format!("candidate_{}", &digest[..12]),
                enabled: false,
                packages: Vec::new(),
                hosts: Vec::new(),
                sdk_bindings: Vec::new(),
                sources: vec![VendorSource::StaticContract {
                    path: PathBuf::from(&contract.path),
                    affected_versions: "*".into(),
                    max_bytes: 10 * 1024 * 1024,
                }],
                auto_repair_confidence: 1.0,
            }
        })
        .collect();
    let config = ApiMaintenanceConfig {
        schema: ApiMaintenanceConfig::SCHEMA,
        mode: MaintenanceMode::Report,
        base_branch: "main".into(),
        max_files: 12,
        max_changed_lines: 800,
        max_attempts: 3,
        max_risk_score: 80,
        allowed_paths: Vec::new(),
        allow_workflow_changes: false,
        allow_generated_changes: false,
        require_resolved_version: true,
        require_graph_invariants: true,
        require_tests: true,
        commands: CommandPolicy::default(),
        publish: PublishPolicy::default(),
        coverage: CoveragePolicy::default(),
        vendors,
    };
    let serialized = toml::to_string_pretty(&config)?;
    Ok(format!(
        "# Synaptic review-only candidate profile.\n# Set authoritative vendor ids and enable only after provenance review.\n{serialized}"
    ))
}

fn collect_candidates(
    root: &Path,
    directory: &Path,
    out: &mut Vec<PathBuf>,
) -> Result<(), DiscoveryError> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if kind.is_dir() {
            if !synaptic_detect::noise::is_noise_dir(&name, directory) {
                collect_candidates(root, &path, out)?;
            }
        } else if kind.is_file() && is_contract_candidate(&name) && path.starts_with(root) {
            out.push(path);
        }
    }
    Ok(())
}

fn is_contract_candidate(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        Path::new(&lower)
            .extension()
            .and_then(|value| value.to_str()),
        Some("graphql" | "graphqls" | "gql" | "proto" | "wsdl" | "xsd" | "smithy")
    ) || ((lower.ends_with(".json") || lower.ends_with(".yaml") || lower.ends_with(".yml"))
        && ["openapi", "swagger", "asyncapi", "openrpc", "smithy"]
            .iter()
            .any(|marker| lower.contains(marker)))
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("contract discovery root is not a directory: {0}")]
    InvalidRoot(PathBuf),
    #[error("contract discovery I/O: {0}")]
    Io(#[from] std::io::Error),
}
