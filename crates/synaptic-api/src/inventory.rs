use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::{
    Dependency, DependencyScope, Ecosystem, PackageCoordinate, PackageUrl, VendorMatch,
    VendorRegistry,
};

/// A dependency uniquely assigned to a configured vendor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorDependency {
    pub vendor_id: String,
    pub dependency: Dependency,
}

/// A dependency claimed by multiple vendors. It cannot trigger automation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmbiguousVendorDependency {
    pub vendor_ids: Vec<String>,
    pub dependency: Dependency,
}

/// Complete dependency inventory plus fail-closed vendor matching decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiInventory {
    pub version: u32,
    pub dependencies: Vec<Dependency>,
    pub matched: Vec<VendorDependency>,
    pub unmatched: Vec<Dependency>,
    pub ambiguous: Vec<AmbiguousVendorDependency>,
}

impl Default for ApiInventory {
    fn default() -> Self {
        Self {
            version: Self::VERSION,
            dependencies: Vec::new(),
            matched: Vec::new(),
            unmatched: Vec::new(),
            ambiguous: Vec::new(),
        }
    }
}

impl ApiInventory {
    pub const VERSION: u32 = 1;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SbomCompleteness {
    Complete,
    Incomplete,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SbomDocumentEvidence {
    pub source_file: String,
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec_version: Option<String>,
    pub completeness: SbomCompleteness,
    pub component_count: usize,
    pub service_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalServiceEvidence {
    pub source_file: String,
    pub name: String,
    pub endpoints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authenticated: Option<bool>,
    pub evidence_digest: String,
}

impl ExternalServiceEvidence {
    pub fn new(
        source_file: impl Into<String>,
        name: impl Into<String>,
        endpoints: Vec<String>,
        authenticated: Option<bool>,
    ) -> Self {
        let source_file = source_file.into();
        let name = name.into();
        let mut endpoints = endpoints
            .into_iter()
            .filter_map(|endpoint| sanitize_service_endpoint(&endpoint))
            .collect::<Vec<_>>();
        endpoints.sort();
        endpoints.dedup();
        let identity = serde_json::to_vec(&(&source_file, &name, &endpoints, authenticated))
            .expect("service evidence identity serializes");
        Self {
            source_file,
            name,
            endpoints,
            authenticated,
            evidence_digest: blake3::hash(&identity).to_hex().to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SbomEvidenceReport {
    pub version: u32,
    pub documents: Vec<SbomDocumentEvidence>,
    pub services: Vec<ExternalServiceEvidence>,
}

impl SbomEvidenceReport {
    pub const VERSION: u32 = 1;
}

impl Default for SbomEvidenceReport {
    fn default() -> Self {
        Self {
            version: Self::VERSION,
            documents: Vec::new(),
            services: Vec::new(),
        }
    }
}

/// Scan manifests, then match every dependency through the vendor registry.
pub fn inventory(root: &Path, registry: &VendorRegistry) -> Result<ApiInventory, InventoryError> {
    let dependencies = scan_dependencies(root)?;
    let mut matched = Vec::new();
    let mut unmatched = Vec::new();
    let mut ambiguous = Vec::new();
    for dependency in &dependencies {
        match registry.match_dependency(dependency) {
            VendorMatch::Unmatched => unmatched.push(dependency.clone()),
            VendorMatch::Matched { vendor_id } => matched.push(VendorDependency {
                vendor_id,
                dependency: dependency.clone(),
            }),
            VendorMatch::Ambiguous { vendor_ids } => ambiguous.push(AmbiguousVendorDependency {
                vendor_ids,
                dependency: dependency.clone(),
            }),
        }
    }
    matched.sort_by(|a, b| {
        a.vendor_id
            .cmp(&b.vendor_id)
            .then_with(|| dependency_key(&a.dependency).cmp(&dependency_key(&b.dependency)))
    });
    unmatched.sort_by_key(dependency_key);
    ambiguous.sort_by_key(|entry| dependency_key(&entry.dependency));
    Ok(ApiInventory {
        version: ApiInventory::VERSION,
        dependencies,
        matched,
        unmatched,
        ambiguous,
    })
}

/// Find supported ecosystem manifests recursively, excluding Synaptic's normal noise dirs.
pub fn scan_dependencies(root: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let manifests = dependency_manifests(root)?;
    scan_dependencies_from_manifests(root, &manifests)
}

/// Scan dependency manifests and SBOM evidence with one deterministic repository walk.
pub fn scan_dependencies_and_sbom_evidence(
    root: &Path,
) -> Result<(Vec<Dependency>, SbomEvidenceReport), InventoryError> {
    let manifests = dependency_manifests(root)?;
    Ok((
        scan_dependencies_from_manifests(root, &manifests)?,
        scan_sbom_evidence_from_manifests(root, &manifests)?,
    ))
}

fn dependency_manifests(root: &Path) -> Result<Vec<PathBuf>, InventoryError> {
    if !root.is_dir() {
        return Err(InventoryError::InvalidRoot(root.to_path_buf()));
    }
    let mut manifests = Vec::new();
    collect_manifests(root, root, &mut manifests)?;
    manifests.sort();
    Ok(manifests)
}

/// `Cargo.lock` contents, parsed at most once per scan and keyed on the
/// lockfile's own path.
///
/// `cargo_lock_versions` is the only lockfile reader that searches *upwards*
/// for its file, so every member of a workspace resolves to the same one.
/// Parsing per manifest therefore re-read one file once per member: on Synaptic
/// itself, 30 `Cargo.toml` files each re-parsed the same 118 KB `Cargo.lock`,
/// which was 67 ms of the 84 ms this function took. Every other reader takes
/// `dir.join(...)`, so its file is naturally distinct per manifest and parsed
/// once already.
///
/// Keyed on the resolved path rather than the manifest's directory, because
/// those 30 manifests sit in 30 different directories and share one lockfile.
/// Scoped to a single scan so a later call cannot be served a stale parse.
type CargoLockCache = std::cell::RefCell<BTreeMap<PathBuf, std::rc::Rc<BTreeMap<String, String>>>>;

fn scan_dependencies_from_manifests(
    root: &Path,
    manifests: &[PathBuf],
) -> Result<Vec<Dependency>, InventoryError> {
    let cargo_locks = CargoLockCache::default();
    let mut dependencies = Vec::new();
    for manifest in manifests {
        let name = manifest
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        match name {
            _ if is_sbom_manifest(name) => dependencies.extend(scan_sbom(root, manifest)?),
            "package.json" => dependencies.extend(scan_package_json(root, manifest)?),
            "pyproject.toml" => dependencies.extend(scan_pyproject(root, manifest)?),
            "go.mod" => dependencies.extend(scan_go_mod(root, manifest)?),
            "Cargo.toml" => dependencies.extend(scan_cargo_toml(root, manifest, &cargo_locks)?),
            "pom.xml" => dependencies.extend(scan_maven_pom(root, manifest)?),
            "build.gradle" | "build.gradle.kts" => {
                dependencies.extend(scan_gradle(root, manifest)?)
            }
            "packages.config" => dependencies.extend(scan_packages_config(root, manifest)?),
            "composer.json" => dependencies.extend(scan_composer(root, manifest)?),
            "Gemfile" => dependencies.extend(scan_gemfile(root, manifest)?),
            "Package.swift" => dependencies.extend(scan_swift_package(root, manifest)?),
            "pubspec.yaml" => dependencies.extend(scan_pubspec(root, manifest)?),
            "mix.exs" => dependencies.extend(scan_mix(root, manifest)?),
            "Project.toml" => dependencies.extend(scan_julia_project(root, manifest)?),
            "build.zig.zon" => dependencies.extend(scan_zig_package(root, manifest)?),
            "Podfile" => dependencies.extend(scan_podfile(root, manifest)?),
            "conanfile.txt" | "conanfile.py" => dependencies.extend(scan_conan(root, manifest)?),
            "vcpkg.json" => dependencies.extend(scan_vcpkg(root, manifest)?),
            "fpm.toml" => dependencies.extend(scan_fpm(root, manifest)?),
            "qlpack.yml" | "qlpack.yaml" => dependencies.extend(scan_qlpack(root, manifest)?),
            "sfdx-project.json" => dependencies.extend(scan_salesforce(root, manifest)?),
            _ if manifest.extension().is_some_and(|ext| ext == "csproj")
                || name == "Directory.Packages.props" =>
            {
                dependencies.extend(scan_msbuild_packages(root, manifest)?)
            }
            _ if is_requirements_file(name) => {
                dependencies.extend(scan_requirements(root, manifest)?)
            }
            _ if manifest.extension().is_some_and(|ext| ext == "rockspec") => {
                dependencies.extend(scan_rockspec(root, manifest)?)
            }
            _ if manifest.extension().is_some_and(|ext| ext == "psd1") => {
                dependencies.extend(scan_powershell_manifest(root, manifest)?)
            }
            _ => {}
        }
    }
    let mut seen = BTreeSet::new();
    dependencies.retain(|dependency| {
        seen.insert((
            dependency.package.clone(),
            dependency.source_file.clone(),
            dependency.scope,
        ))
    });
    dependencies.sort_by_key(dependency_key);
    Ok(dependencies)
}

/// Read SBOM coverage declarations and external-service records independently
/// from dependency extraction. Unknown completeness is preserved and must not be
/// interpreted as an exhaustive inventory.
pub fn scan_sbom_evidence(root: &Path) -> Result<SbomEvidenceReport, InventoryError> {
    let manifests = dependency_manifests(root)?;
    scan_sbom_evidence_from_manifests(root, &manifests)
}

fn scan_sbom_evidence_from_manifests(
    root: &Path,
    manifests: &[PathBuf],
) -> Result<SbomEvidenceReport, InventoryError> {
    let mut report = SbomEvidenceReport {
        version: SbomEvidenceReport::VERSION,
        ..SbomEvidenceReport::default()
    };
    for manifest in manifests.iter().filter(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_sbom_manifest)
    }) {
        let (document, mut services) = scan_sbom_document_evidence(root, manifest)?;
        report.documents.push(document);
        report.services.append(&mut services);
    }
    report
        .documents
        .sort_by(|left, right| left.source_file.cmp(&right.source_file));
    report.services.sort_by(|left, right| {
        left.source_file
            .cmp(&right.source_file)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.evidence_digest.cmp(&right.evidence_digest))
    });
    report
        .services
        .dedup_by(|left, right| left.evidence_digest == right.evidence_digest);
    Ok(report)
}

fn dependency_key(dependency: &Dependency) -> (PackageCoordinate, String, DependencyScope) {
    (
        dependency.package.clone(),
        dependency.source_file.clone(),
        dependency.scope,
    )
}

fn collect_manifests(
    root: &Path,
    dir: &Path,
    manifests: &mut Vec<PathBuf>,
) -> Result<(), InventoryError> {
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() {
            if synaptic_detect::noise::is_noise_dir(&name, dir) {
                continue;
            }
            collect_manifests(root, &path, manifests)?;
        } else if file_type.is_file() && is_dependency_manifest(&name, &path) {
            // Only paths beneath root are collected; keeping the check explicit makes
            // future walker changes fail closed rather than leaking absolute paths.
            if path.strip_prefix(root).is_ok() {
                manifests.push(path);
            }
        }
    }
    Ok(())
}

fn is_dependency_manifest(name: &str, path: &Path) -> bool {
    matches!(
        name,
        "package.json"
            | "pyproject.toml"
            | "go.mod"
            | "Cargo.toml"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "packages.config"
            | "Directory.Packages.props"
            | "composer.json"
            | "Gemfile"
            | "Package.swift"
            | "pubspec.yaml"
            | "mix.exs"
            | "Project.toml"
            | "build.zig.zon"
            | "Podfile"
            | "conanfile.txt"
            | "conanfile.py"
            | "vcpkg.json"
            | "fpm.toml"
            | "qlpack.yml"
            | "qlpack.yaml"
            | "sfdx-project.json"
    ) || is_requirements_file(name)
        || is_sbom_manifest(name)
        || path.extension().is_some_and(|extension| {
            matches!(extension.to_str(), Some("csproj" | "rockspec" | "psd1"))
        })
}

/// Whether a manifest file name is an SBOM document (CycloneDX or SPDX).
///
/// Takes a bare file name, not a path. Callers holding a path should pass the
/// final component.
pub fn is_sbom_manifest(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "bom.json"
        || name == "bom.xml"
        || name == "spdx.json"
        || name.ends_with(".cdx.json")
        || name.ends_with(".cdx.xml")
        || name.ends_with(".spdx.json")
}

fn scan_sbom(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let bytes = fs::read(path)?;
    if bytes
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        == Some(b'<')
    {
        let source = std::str::from_utf8(&bytes).map_err(|_| InventoryError::InvalidXml {
            path: path.to_path_buf(),
            message: "document is not UTF-8".into(),
        })?;
        return scan_cyclonedx_xml(root, path, source);
    }
    let data: JsonValue =
        serde_json::from_slice(&bytes).map_err(|source| InventoryError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    if data
        .get("bomFormat")
        .and_then(JsonValue::as_str)
        .is_some_and(|format| format.eq_ignore_ascii_case("CycloneDX"))
    {
        return scan_cyclonedx(root, path, &data);
    }
    if data
        .get("spdxVersion")
        .and_then(JsonValue::as_str)
        .is_some()
    {
        return scan_spdx(root, path, &data);
    }
    Err(InventoryError::UnknownSbomFormat(path.to_path_buf()))
}

fn scan_sbom_document_evidence(
    root: &Path,
    path: &Path,
) -> Result<(SbomDocumentEvidence, Vec<ExternalServiceEvidence>), InventoryError> {
    let bytes = fs::read(path)?;
    let source_file = relative(root, path);
    if bytes
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        == Some(b'<')
    {
        let source = std::str::from_utf8(&bytes).map_err(|_| InventoryError::InvalidXml {
            path: path.to_path_buf(),
            message: "document is not UTF-8".into(),
        })?;
        let lowercase = source.to_ascii_lowercase();
        if lowercase.contains("<!doctype") || lowercase.contains("<!entity") {
            return Err(InventoryError::InvalidXml {
                path: path.to_path_buf(),
                message: "DTD or entity declarations are forbidden".into(),
            });
        }
        let document =
            roxmltree::Document::parse(source).map_err(|error| InventoryError::InvalidXml {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
        let root_element = document.root_element();
        if root_element.tag_name().name() != "bom" {
            return Err(InventoryError::UnknownSbomFormat(path.to_path_buf()));
        }
        let component_count = document
            .descendants()
            .filter(|node| node.is_element() && node.tag_name().name() == "component")
            .count();
        let completeness = sbom_completeness(
            document
                .descendants()
                .filter(|node| node.is_element() && node.tag_name().name() == "aggregate")
                .filter_map(|node| node.text()),
        );
        let mut services = Vec::new();
        for service in document
            .descendants()
            .filter(|node| node.is_element() && node.tag_name().name() == "service")
        {
            let name = xml_child_text(service, "name").unwrap_or_else(|| "unknown".into());
            let endpoints = service
                .descendants()
                .filter(|node| node.is_element() && node.tag_name().name() == "endpoint")
                .filter_map(|node| node.text())
                .map(str::to_string)
                .collect();
            let authenticated = xml_child_text(service, "authenticated")
                .and_then(|value| value.parse::<bool>().ok());
            services.push(ExternalServiceEvidence::new(
                source_file.clone(),
                name,
                endpoints,
                authenticated,
            ));
        }
        let spec_version = root_element
            .tag_name()
            .namespace()
            .and_then(|namespace| namespace.rsplit('/').next())
            .map(str::to_string);
        return Ok((
            SbomDocumentEvidence {
                source_file,
                format: "cyclonedx".into(),
                spec_version,
                completeness,
                component_count,
                service_count: services.len(),
            },
            services,
        ));
    }

    let data: JsonValue =
        serde_json::from_slice(&bytes).map_err(|source| InventoryError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    if data
        .get("bomFormat")
        .and_then(JsonValue::as_str)
        .is_some_and(|format| format.eq_ignore_ascii_case("CycloneDX"))
    {
        let aggregates = data
            .get("compositions")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
            .filter_map(|composition| composition.get("aggregate"))
            .filter_map(JsonValue::as_str);
        let mut services = Vec::new();
        collect_json_services(&data, &source_file, 0, &mut services);
        return Ok((
            SbomDocumentEvidence {
                source_file,
                format: "cyclonedx".into(),
                spec_version: data
                    .get("specVersion")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string),
                completeness: sbom_completeness(aggregates),
                component_count: data
                    .get("components")
                    .and_then(JsonValue::as_array)
                    .map_or(0, Vec::len),
                service_count: services.len(),
            },
            services,
        ));
    }
    if data
        .get("spdxVersion")
        .and_then(JsonValue::as_str)
        .is_some()
    {
        return Ok((
            SbomDocumentEvidence {
                source_file,
                format: "spdx".into(),
                spec_version: data
                    .get("spdxVersion")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string),
                completeness: SbomCompleteness::Unknown,
                component_count: data
                    .get("packages")
                    .and_then(JsonValue::as_array)
                    .map_or(0, Vec::len),
                service_count: 0,
            },
            Vec::new(),
        ));
    }
    Err(InventoryError::UnknownSbomFormat(path.to_path_buf()))
}

fn sbom_completeness<'a>(aggregates: impl Iterator<Item = &'a str>) -> SbomCompleteness {
    let mut saw_complete = false;
    for aggregate in aggregates {
        let aggregate = aggregate.to_ascii_lowercase();
        if aggregate.starts_with("incomplete") {
            return SbomCompleteness::Incomplete;
        }
        if aggregate == "complete" {
            saw_complete = true;
        }
    }
    if saw_complete {
        SbomCompleteness::Complete
    } else {
        SbomCompleteness::Unknown
    }
}

fn collect_json_services(
    value: &JsonValue,
    source_file: &str,
    depth: usize,
    out: &mut Vec<ExternalServiceEvidence>,
) {
    if depth > 64 {
        return;
    }
    match value {
        JsonValue::Object(object) => {
            for (key, child) in object {
                if key == "services" {
                    if let Some(services) = child.as_array() {
                        for service in services {
                            if let Some(object) = service.as_object() {
                                let name = object
                                    .get("name")
                                    .and_then(JsonValue::as_str)
                                    .unwrap_or("unknown");
                                let endpoints = object
                                    .get("endpoints")
                                    .and_then(JsonValue::as_array)
                                    .into_iter()
                                    .flatten()
                                    .filter_map(JsonValue::as_str)
                                    .map(str::to_string)
                                    .collect();
                                let authenticated =
                                    object.get("authenticated").and_then(JsonValue::as_bool);
                                out.push(ExternalServiceEvidence::new(
                                    source_file,
                                    name,
                                    endpoints,
                                    authenticated,
                                ));
                            }
                            collect_json_services(service, source_file, depth + 1, out);
                        }
                    }
                } else {
                    collect_json_services(child, source_file, depth + 1, out);
                }
            }
        }
        JsonValue::Array(array) => {
            for child in array {
                collect_json_services(child, source_file, depth + 1, out);
            }
        }
        _ => {}
    }
}

fn sanitize_service_endpoint(endpoint: &str) -> Option<String> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return None;
    }
    let without_fragment = endpoint.split('#').next().unwrap_or(endpoint);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let authority = without_query
        .split_once("://")
        .map(|(_, remainder)| remainder.split('/').next().unwrap_or(remainder));
    if authority.is_some_and(|authority| authority.contains('@')) {
        return None;
    }
    Some(without_query.to_string())
}

fn scan_cyclonedx_xml(
    root: &Path,
    path: &Path,
    source: &str,
) -> Result<Vec<Dependency>, InventoryError> {
    let lowercase = source.to_ascii_lowercase();
    if lowercase.contains("<!doctype") || lowercase.contains("<!entity") {
        return Err(InventoryError::InvalidXml {
            path: path.to_path_buf(),
            message: "DTD or entity declarations are forbidden".into(),
        });
    }
    let document =
        roxmltree::Document::parse(source).map_err(|error| InventoryError::InvalidXml {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
    if document.root_element().tag_name().name() != "bom" {
        return Err(InventoryError::UnknownSbomFormat(path.to_path_buf()));
    }

    let mut out = Vec::new();
    for component in document.descendants().filter(|node| {
        node.is_element() && node.tag_name().name().eq_ignore_ascii_case("component")
    }) {
        let name = xml_child_text(component, "name").unwrap_or_else(|| "unknown".into());
        let purl = xml_child_text(component, "purl");
        let parsed = purl
            .as_deref()
            .map(PackageUrl::parse)
            .transpose()
            .map_err(|message| InventoryError::InvalidPackageUrl {
                path: path.to_path_buf(),
                value: purl.clone().unwrap_or_default(),
                message,
            })?;
        let package = parsed.as_ref().map_or_else(
            || {
                let group = xml_child_text(component, "group").filter(|group| !group.is_empty());
                let identity = group.map_or_else(
                    || format!("cyclonedx/{name}"),
                    |group| format!("cyclonedx/{group}/{name}"),
                );
                PackageCoordinate::new(Ecosystem::Generic, identity)
            },
            PackageUrl::to_coordinate,
        );
        let scope = match xml_child_text(component, "scope")
            .unwrap_or_else(|| "required".into())
            .to_ascii_lowercase()
            .as_str()
        {
            "optional" => DependencyScope::Optional,
            "excluded" => DependencyScope::Development,
            _ => DependencyScope::Runtime,
        };
        let mut dependency = Dependency::new(package, relative(root, path), scope);
        dependency.resolved_version = parsed
            .as_ref()
            .and_then(|purl| purl.version.clone())
            .or_else(|| xml_child_text(component, "version"));
        dependency.purl = parsed.map(|purl| purl.to_string());
        out.push(dependency);
    }
    Ok(out)
}

fn xml_child_text(parent: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    parent
        .children()
        .find(|node| node.is_element() && node.tag_name().name().eq_ignore_ascii_case(name))
        .and_then(|node| node.text())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn scan_cyclonedx(
    root: &Path,
    path: &Path,
    data: &JsonValue,
) -> Result<Vec<Dependency>, InventoryError> {
    let mut out = Vec::new();
    for component in data
        .get("components")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
    {
        let name = component
            .get("name")
            .and_then(JsonValue::as_str)
            .unwrap_or("unknown");
        let purl = component.get("purl").and_then(JsonValue::as_str);
        let parsed = purl.map(PackageUrl::parse).transpose().map_err(|message| {
            InventoryError::InvalidPackageUrl {
                path: path.to_path_buf(),
                value: purl.unwrap_or_default().to_string(),
                message,
            }
        })?;
        let package = parsed.as_ref().map_or_else(
            || {
                let group = component
                    .get("group")
                    .and_then(JsonValue::as_str)
                    .filter(|group| !group.is_empty());
                let identity = group.map_or_else(
                    || format!("cyclonedx/{name}"),
                    |group| format!("cyclonedx/{group}/{name}"),
                );
                PackageCoordinate::new(Ecosystem::Generic, identity)
            },
            PackageUrl::to_coordinate,
        );
        let scope = match component
            .get("scope")
            .and_then(JsonValue::as_str)
            .unwrap_or("required")
            .to_ascii_lowercase()
            .as_str()
        {
            "optional" => DependencyScope::Optional,
            "excluded" => DependencyScope::Development,
            _ => DependencyScope::Runtime,
        };
        let mut dependency = Dependency::new(package, relative(root, path), scope);
        dependency.resolved_version = parsed
            .as_ref()
            .and_then(|purl| purl.version.clone())
            .or_else(|| {
                component
                    .get("version")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string)
            });
        dependency.purl = parsed.map(|purl| purl.to_string());
        out.push(dependency);
    }
    Ok(out)
}

fn scan_spdx(
    root: &Path,
    path: &Path,
    data: &JsonValue,
) -> Result<Vec<Dependency>, InventoryError> {
    let mut out = Vec::new();
    for package in data
        .get("packages")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
    {
        let name = package
            .get("name")
            .and_then(JsonValue::as_str)
            .unwrap_or("unknown");
        let purl = package
            .get("externalRefs")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
            .find(|reference| {
                reference
                    .get("referenceType")
                    .and_then(JsonValue::as_str)
                    .is_some_and(|kind| {
                        kind.eq_ignore_ascii_case("purl")
                            || kind.to_ascii_lowercase().ends_with("/purl")
                    })
            })
            .and_then(|reference| reference.get("referenceLocator"))
            .and_then(JsonValue::as_str);
        let parsed = purl.map(PackageUrl::parse).transpose().map_err(|message| {
            InventoryError::InvalidPackageUrl {
                path: path.to_path_buf(),
                value: purl.unwrap_or_default().to_string(),
                message,
            }
        })?;
        let coordinate = parsed.as_ref().map_or_else(
            || PackageCoordinate::new(Ecosystem::Generic, format!("spdx/{name}")),
            PackageUrl::to_coordinate,
        );
        let mut dependency =
            Dependency::new(coordinate, relative(root, path), DependencyScope::Runtime);
        dependency.resolved_version = parsed
            .as_ref()
            .and_then(|purl| purl.version.clone())
            .or_else(|| {
                package
                    .get("versionInfo")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string)
            });
        dependency.purl = parsed.map(|purl| purl.to_string());
        out.push(dependency);
    }
    Ok(out)
}

fn is_requirements_file(name: &str) -> bool {
    name == "requirements.txt"
        || (name.starts_with("requirements-") && name.ends_with(".txt"))
        || (name.starts_with("requirements.") && name.ends_with(".txt"))
}

fn scan_package_json(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let data: JsonValue =
        serde_json::from_slice(&fs::read(path)?).map_err(|source| InventoryError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    let dir = path.parent().unwrap_or(root);
    let resolved = node_lock_versions(dir)?;
    let mut out = Vec::new();
    for (block, scope) in [
        ("dependencies", DependencyScope::Runtime),
        ("devDependencies", DependencyScope::Development),
        ("optionalDependencies", DependencyScope::Optional),
        ("peerDependencies", DependencyScope::Optional),
    ] {
        let Some(entries) = data.get(block).and_then(JsonValue::as_object) else {
            continue;
        };
        let mut names: Vec<_> = entries.keys().collect();
        names.sort();
        for name in names {
            let mut dependency = Dependency::new(
                PackageCoordinate::new(Ecosystem::Npm, name),
                relative(root, path),
                scope,
            );
            dependency.declared_requirement = entries[name].as_str().map(str::to_string);
            dependency.resolved_version = resolved.get(&dependency.package.name).cloned();
            out.push(dependency);
        }
    }
    Ok(out)
}

fn node_lock_versions(dir: &Path) -> Result<BTreeMap<String, String>, InventoryError> {
    let mut versions = BTreeMap::new();
    versions.extend(package_lock_versions(dir)?);
    versions.extend(pnpm_lock_versions(dir)?);
    versions.extend(yarn_lock_versions(dir)?);
    Ok(versions)
}

fn package_lock_versions(dir: &Path) -> Result<BTreeMap<String, String>, InventoryError> {
    let path = dir.join("package-lock.json");
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let data: JsonValue =
        serde_json::from_slice(&fs::read(&path)?).map_err(|source| InventoryError::Json {
            path: path.clone(),
            source,
        })?;
    let mut versions = BTreeMap::new();
    if let Some(packages) = data.get("packages").and_then(JsonValue::as_object) {
        for (key, value) in packages {
            let Some(name) = key.strip_prefix("node_modules/") else {
                continue;
            };
            // Ignore nested transitive copies; a direct root dependency has one
            // node_modules prefix. Scoped packages legitimately contain one slash.
            let nested = if name.starts_with('@') {
                name.matches('/').count() > 1
            } else {
                name.contains('/')
            };
            if nested {
                continue;
            }
            if let Some(version) = value.get("version").and_then(JsonValue::as_str) {
                versions.insert(name.to_ascii_lowercase(), version.to_string());
            }
        }
    }
    if let Some(dependencies) = data.get("dependencies").and_then(JsonValue::as_object) {
        for (name, value) in dependencies {
            if let Some(version) = value.get("version").and_then(JsonValue::as_str) {
                versions
                    .entry(name.to_ascii_lowercase())
                    .or_insert_with(|| version.to_string());
            }
        }
    }
    Ok(versions)
}

fn pnpm_lock_versions(dir: &Path) -> Result<BTreeMap<String, String>, InventoryError> {
    let path = dir.join("pnpm-lock.yaml");
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let mut versions = BTreeMap::new();
    for line in fs::read_to_string(path)?.lines() {
        let trimmed = line.trim();
        if !trimmed.ends_with(':') {
            continue;
        }
        let key = trimmed
            .trim_end_matches(':')
            .trim_matches(['\'', '"'])
            .trim_start_matches('/');
        if let Some((name, version)) = split_node_lock_key(key) {
            versions.entry(name).or_insert(version);
        }
    }
    Ok(versions)
}

fn yarn_lock_versions(dir: &Path) -> Result<BTreeMap<String, String>, InventoryError> {
    let path = dir.join("yarn.lock");
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let mut versions = BTreeMap::new();
    let mut pending = Vec::new();
    for line in fs::read_to_string(path)?.lines() {
        if !line.starts_with(char::is_whitespace) && line.trim_end().ends_with(':') {
            pending.clear();
            for selector in line.trim_end_matches(':').split(',') {
                let selector = selector.trim().trim_matches(['\'', '"']);
                if let Some(name) = node_name_from_selector(selector) {
                    pending.push(name);
                }
            }
            continue;
        }
        let trimmed = line.trim();
        let version = trimmed
            .strip_prefix("version ")
            .or_else(|| trimmed.strip_prefix("version:"))
            .map(|value| value.trim().trim_matches(['\'', '"']));
        if let Some(version) = version.filter(|version| !version.is_empty()) {
            for name in pending.drain(..) {
                versions.entry(name).or_insert_with(|| version.to_string());
            }
        }
    }
    Ok(versions)
}

fn node_name_from_selector(value: &str) -> Option<String> {
    let separator = value.rfind('@')?;
    if separator == 0 {
        return None;
    }
    let name = value[..separator].trim();
    (!name.is_empty()).then(|| name.to_ascii_lowercase())
}

fn split_node_lock_key(value: &str) -> Option<(String, String)> {
    let value = value.split('(').next()?.trim();
    let separator = value.rfind('@')?;
    if separator == 0 {
        return None;
    }
    let name = value[..separator].trim();
    let version = value[separator + 1..].trim();
    if name.is_empty()
        || version.is_empty()
        || version.starts_with(['^', '~', '*'])
        || version.contains("workspace:")
    {
        return None;
    }
    Some((name.to_ascii_lowercase(), version.to_string()))
}

fn scan_pyproject(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let source = fs::read_to_string(path)?;
    let data: toml::Value = toml::from_str(&source).map_err(|source| InventoryError::Toml {
        path: path.to_path_buf(),
        source,
    })?;
    let resolved = python_lock_versions(path.parent().unwrap_or(root))?;
    let mut out = Vec::new();
    if let Some(items) = data
        .get("project")
        .and_then(|project| project.get("dependencies"))
        .and_then(toml::Value::as_array)
    {
        for raw in items.iter().filter_map(toml::Value::as_str) {
            if let Some((name, requirement)) = parse_python_requirement(raw) {
                out.push(python_dependency(
                    root,
                    path,
                    name,
                    requirement,
                    DependencyScope::Runtime,
                    &resolved,
                ));
            }
        }
    }
    if let Some(table) = data
        .get("tool")
        .and_then(|tool| tool.get("poetry"))
        .and_then(|poetry| poetry.get("dependencies"))
        .and_then(toml::Value::as_table)
    {
        for (name, value) in table {
            if name.eq_ignore_ascii_case("python") {
                continue;
            }
            let requirement = match value {
                toml::Value::String(value) => Some(value.clone()),
                toml::Value::Table(table) => table
                    .get("version")
                    .and_then(toml::Value::as_str)
                    .map(str::to_string),
                _ => None,
            };
            out.push(python_dependency(
                root,
                path,
                name.clone(),
                requirement,
                DependencyScope::Runtime,
                &resolved,
            ));
        }
    }
    Ok(out)
}

fn scan_requirements(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let resolved = python_lock_versions(path.parent().unwrap_or(root))?;
    let scope = if path
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.contains("dev") || name.contains("test"))
    {
        DependencyScope::Development
    } else {
        DependencyScope::Runtime
    };
    let mut out = Vec::new();
    for raw in fs::read_to_string(path)?.lines() {
        let raw = raw.split('#').next().unwrap_or("").trim();
        if raw.is_empty() || raw.starts_with('-') {
            continue;
        }
        if let Some((name, requirement)) = parse_python_requirement(raw) {
            out.push(python_dependency(
                root,
                path,
                name,
                requirement,
                scope,
                &resolved,
            ));
        }
    }
    Ok(out)
}

fn python_dependency(
    root: &Path,
    path: &Path,
    name: String,
    requirement: Option<String>,
    scope: DependencyScope,
    resolved: &BTreeMap<String, String>,
) -> Dependency {
    let mut dependency = Dependency::new(
        PackageCoordinate::new(Ecosystem::Pypi, name),
        relative(root, path),
        scope,
    );
    dependency.declared_requirement = requirement;
    dependency.resolved_version = resolved.get(&dependency.package.name).cloned();
    dependency
}

fn parse_python_requirement(raw: &str) -> Option<(String, Option<String>)> {
    let raw = raw.split(';').next()?.trim();
    if raw.is_empty() {
        return None;
    }
    let name_end = raw
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace() || matches!(ch, '[' | '<' | '>' | '=' | '!' | '~' | '@'))
        .map(|(index, _)| index)
        .unwrap_or(raw.len());
    let name = raw[..name_end].trim();
    if name.is_empty() {
        return None;
    }
    let after_name = &raw[name_end..];
    let requirement_start = if after_name.starts_with('[') {
        after_name.find(']').map(|index| index + 1).unwrap_or(0)
    } else {
        0
    };
    let requirement = after_name[requirement_start..].trim();
    Some((
        name.to_string(),
        (!requirement.is_empty()).then(|| requirement.to_string()),
    ))
}

fn python_lock_versions(dir: &Path) -> Result<BTreeMap<String, String>, InventoryError> {
    let mut versions = BTreeMap::new();
    for filename in ["poetry.lock", "uv.lock"] {
        let path = dir.join(filename);
        if !path.is_file() {
            continue;
        }
        let source = fs::read_to_string(&path)?;
        let data: toml::Value = toml::from_str(&source).map_err(|source| InventoryError::Toml {
            path: path.clone(),
            source,
        })?;
        if let Some(packages) = data.get("package").and_then(toml::Value::as_array) {
            for package in packages {
                let name = package.get("name").and_then(toml::Value::as_str);
                let version = package.get("version").and_then(toml::Value::as_str);
                if let (Some(name), Some(version)) = (name, version) {
                    let name = PackageCoordinate::new(Ecosystem::Pypi, name).name;
                    versions.insert(name, version.to_string());
                }
            }
        }
    }
    Ok(versions)
}

fn scan_go_mod(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let source = fs::read_to_string(path)?;
    let mut out = Vec::new();
    let mut in_require = false;
    for raw in source.lines() {
        let line = raw.split("//").next().unwrap_or("").trim();
        if line == "require (" {
            in_require = true;
            continue;
        }
        if in_require && line == ")" {
            in_require = false;
            continue;
        }
        let fields: Vec<_> = if in_require {
            line.split_whitespace().collect()
        } else if let Some(requirement) = line.strip_prefix("require ") {
            requirement.split_whitespace().collect()
        } else {
            continue;
        };
        if fields.len() < 2 || fields[0].is_empty() || fields[1].is_empty() {
            continue;
        }
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Go, fields[0]),
            relative(root, path),
            DependencyScope::Runtime,
        );
        dependency.declared_requirement = Some(fields[1].to_string());
        dependency.resolved_version = Some(fields[1].to_string());
        out.push(dependency);
    }
    Ok(out)
}

fn scan_cargo_toml(
    root: &Path,
    path: &Path,
    cargo_locks: &CargoLockCache,
) -> Result<Vec<Dependency>, InventoryError> {
    let source = fs::read_to_string(path)?;
    let data: toml::Value = toml::from_str(&source).map_err(|source| InventoryError::Toml {
        path: path.to_path_buf(),
        source,
    })?;
    let resolved = cargo_lock_versions(root, path.parent().unwrap_or(root), cargo_locks)?;
    let mut out = Vec::new();
    collect_cargo_tables(root, path, &data, &resolved, &mut Vec::new(), &mut out);
    Ok(out)
}

fn collect_cargo_tables(
    root: &Path,
    path: &Path,
    value: &toml::Value,
    resolved: &BTreeMap<String, String>,
    table_path: &mut Vec<String>,
    out: &mut Vec<Dependency>,
) {
    let Some(table) = value.as_table() else {
        return;
    };
    let current = table_path.last().map(String::as_str).unwrap_or("");
    let scope = match current {
        "dependencies" => Some(DependencyScope::Runtime),
        "dev-dependencies" => Some(DependencyScope::Development),
        "build-dependencies" => Some(DependencyScope::Development),
        _ => None,
    };
    if let Some(scope) = scope {
        for (alias, value) in table {
            let (name, requirement) = match value {
                toml::Value::String(requirement) => (alias.as_str(), Some(requirement.clone())),
                toml::Value::Table(details) => (
                    details
                        .get("package")
                        .and_then(toml::Value::as_str)
                        .unwrap_or(alias),
                    details
                        .get("version")
                        .and_then(toml::Value::as_str)
                        .map(str::to_string),
                ),
                _ => continue,
            };
            if matches!(value, toml::Value::Table(details) if details.contains_key("path") && requirement.is_none())
            {
                continue;
            }
            let mut dependency = Dependency::new(
                PackageCoordinate::new(Ecosystem::Cargo, name),
                relative(root, path),
                scope,
            );
            dependency.declared_requirement = requirement;
            dependency.resolved_version = resolved.get(&dependency.package.name).cloned();
            out.push(dependency);
        }
        return;
    }
    for (key, child) in table {
        if child.is_table() {
            table_path.push(key.clone());
            collect_cargo_tables(root, path, child, resolved, table_path, out);
            table_path.pop();
        }
    }
}

fn cargo_lock_versions(
    root: &Path,
    dir: &Path,
    cache: &CargoLockCache,
) -> Result<std::rc::Rc<BTreeMap<String, String>>, InventoryError> {
    let path = ancestors_within(root, dir)
        .map(|candidate| candidate.join("Cargo.lock"))
        .find(|candidate| candidate.is_file());
    let Some(path) = path else {
        return Ok(std::rc::Rc::new(BTreeMap::new()));
    };
    if let Some(hit) = cache.borrow().get(&path) {
        return Ok(std::rc::Rc::clone(hit));
    }
    let source = fs::read_to_string(&path)?;
    let data: toml::Value = toml::from_str(&source).map_err(|source| InventoryError::Toml {
        path: path.clone(),
        source,
    })?;
    let mut versions = BTreeMap::new();
    if let Some(packages) = data.get("package").and_then(toml::Value::as_array) {
        for package in packages {
            if let (Some(name), Some(version)) = (
                package.get("name").and_then(toml::Value::as_str),
                package.get("version").and_then(toml::Value::as_str),
            ) {
                versions
                    .entry(name.to_ascii_lowercase())
                    .or_insert_with(|| version.to_string());
            }
        }
    }
    let versions = std::rc::Rc::new(versions);
    cache
        .borrow_mut()
        .insert(path, std::rc::Rc::clone(&versions));
    Ok(versions)
}

fn scan_maven_pom(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let source = fs::read_to_string(path)?;
    let mut properties = BTreeMap::new();
    if let Some(block) = xml_element(&source, "properties") {
        let mut rest = block;
        while let Some(start) = rest.find('<') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('>') else { break };
            let tag = &after[..end];
            if tag.starts_with('/') || tag.contains(char::is_whitespace) {
                rest = &after[end + 1..];
                continue;
            }
            if let Some(value) = xml_element(&rest[start..], tag) {
                properties.insert(tag.to_string(), value.trim().to_string());
            }
            rest = &after[end + 1..];
        }
    }
    let mut out = Vec::new();
    for block in xml_elements(&source, "dependency") {
        let Some(group) = xml_element(block, "groupId").map(str::trim) else {
            continue;
        };
        let Some(artifact) = xml_element(block, "artifactId").map(str::trim) else {
            continue;
        };
        let raw_version = xml_element(block, "version").map(str::trim);
        let version = raw_version.map(|value| resolve_property(value, &properties));
        let scope = match xml_element(block, "scope").map(str::trim) {
            Some("test") => DependencyScope::Development,
            Some("provided") | Some("optional") => DependencyScope::Optional,
            _ => DependencyScope::Runtime,
        };
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Maven, format!("{group}:{artifact}")),
            relative(root, path),
            scope,
        );
        dependency.declared_requirement = raw_version.map(str::to_string);
        dependency.resolved_version = version.filter(|value| !value.contains("${"));
        out.push(dependency);
    }
    Ok(out)
}

fn scan_gradle(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let directory = path.parent().unwrap_or(root);
    let resolved = gradle_lock_versions(root, directory)?;
    let properties = gradle_properties(root, directory)?;
    let mut out = Vec::new();
    for raw in fs::read_to_string(path)?.lines() {
        let line = raw.split("//").next().unwrap_or("").trim();
        let Some(configuration) = line.split_once('(').map(|(name, _)| name.trim()) else {
            continue;
        };
        let configuration = configuration.to_ascii_lowercase();
        let scope = if configuration.contains("test") {
            DependencyScope::Development
        } else if configuration.ends_with("compileonly")
            || configuration.ends_with("runtimeonly")
            || configuration.ends_with("localruntime")
        {
            DependencyScope::Optional
        } else if configuration.ends_with("implementation")
            || configuration.ends_with("api")
            || configuration.ends_with("compile")
            || matches!(
                configuration.as_str(),
                "classpath"
                    | "annotationprocessor"
                    | "kapt"
                    | "ksp"
                    | "minecraft"
                    | "mappings"
                    | "neoforge"
                    | "include"
            )
        {
            DependencyScope::Runtime
        } else {
            continue;
        };
        let Some(coordinate) = gradle_quoted_argument(line) else {
            continue;
        };
        let mut parts = coordinate.split(':');
        let (Some(group), Some(artifact)) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = format!("{group}:{artifact}");
        let declared = parts.next().map(str::to_string);
        let declared_version = declared
            .as_deref()
            .and_then(|value| resolve_gradle_version(value, &properties));
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Maven, &name),
            relative(root, path),
            scope,
        );
        dependency.declared_requirement = declared;
        dependency.resolved_version = resolved.get(&name).cloned().or(declared_version);
        out.push(dependency);
    }
    Ok(out)
}

fn gradle_lock_versions(
    root: &Path,
    dir: &Path,
) -> Result<BTreeMap<String, String>, InventoryError> {
    let Some(path) = ancestors_within(root, dir)
        .map(|candidate| candidate.join("gradle.lockfile"))
        .find(|candidate| candidate.is_file())
    else {
        return Ok(BTreeMap::new());
    };
    let mut versions = BTreeMap::new();
    for line in fs::read_to_string(path)?.lines() {
        let coordinate = line.split('=').next().unwrap_or("").trim();
        let parts: Vec<_> = coordinate.split(':').collect();
        if parts.len() >= 3 {
            versions.insert(format!("{}:{}", parts[0], parts[1]), parts[2].to_string());
        }
    }
    Ok(versions)
}

fn gradle_properties(root: &Path, dir: &Path) -> Result<BTreeMap<String, String>, InventoryError> {
    let mut directories = ancestors_within(root, dir).collect::<Vec<_>>();
    directories.reverse();
    let mut properties = BTreeMap::new();
    for directory in directories {
        let path = directory.join("gradle.properties");
        if !path.is_file() {
            continue;
        }
        for raw in fs::read_to_string(path)?.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                continue;
            }
            let Some((name, value)) = line.split_once('=').or_else(|| line.split_once(':')) else {
                continue;
            };
            let name = name.trim();
            if !name.is_empty() {
                properties.insert(name.to_string(), value.trim().to_string());
            }
        }
    }
    Ok(properties)
}

fn gradle_quoted_argument(line: &str) -> Option<&str> {
    let arguments = line.split_once('(')?.1;
    let (start, quote) = arguments
        .char_indices()
        .find(|(_, character)| matches!(character, '\'' | '"'))?;
    let after = &arguments[start + quote.len_utf8()..];
    let end = after.rfind(quote)?;
    Some(&after[..end])
}

fn resolve_gradle_version(value: &str, properties: &BTreeMap<String, String>) -> Option<String> {
    static PROPERTY: OnceLock<Regex> = OnceLock::new();
    let property_re = PROPERTY.get_or_init(|| {
        Regex::new(
            r#"\$\{(?:(?:rootProject|project)\.)?(?:property|findProperty)\(["']([^"']+)["']\)\}"#,
        )
        .expect("valid Gradle property expression regex")
    });
    let mut unresolved = false;
    let resolved = property_re
        .replace_all(value, |captures: &regex::Captures<'_>| {
            properties.get(&captures[1]).cloned().unwrap_or_else(|| {
                unresolved = true;
                captures[0].to_string()
            })
        })
        .into_owned();
    if unresolved
        || resolved.is_empty()
        || resolved.contains("${")
        || resolved.contains('*')
        || resolved.ends_with('+')
        || resolved.eq_ignore_ascii_case("latest.release")
        || resolved.eq_ignore_ascii_case("latest.integration")
        || resolved.starts_with('[')
        || resolved.starts_with('(')
    {
        None
    } else {
        Some(resolved)
    }
}

fn scan_msbuild_packages(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let source = fs::read_to_string(path)?;
    let resolved = nuget_lock_versions(path.parent().unwrap_or(root))?;
    let mut out = Vec::new();
    for tag in ["PackageReference", "PackageVersion"] {
        for element in xml_start_tags(&source, tag) {
            let Some(name) =
                xml_attribute(element, "Include").or_else(|| xml_attribute(element, "Update"))
            else {
                continue;
            };
            let version = xml_attribute(element, "Version").or_else(|| {
                let end = source.find(element)? + element.len();
                xml_element(&source[end..], "Version").map(str::trim)
            });
            let mut dependency = Dependency::new(
                PackageCoordinate::new(Ecosystem::Nuget, name),
                relative(root, path),
                DependencyScope::Runtime,
            );
            dependency.declared_requirement = version.map(str::to_string);
            dependency.resolved_version = resolved
                .get(&dependency.package.name)
                .cloned()
                .or_else(|| version.map(str::to_string));
            out.push(dependency);
        }
    }
    Ok(out)
}

fn scan_packages_config(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let source = fs::read_to_string(path)?;
    let mut out = Vec::new();
    for element in xml_start_tags(&source, "package") {
        let Some(name) = xml_attribute(element, "id") else {
            continue;
        };
        let version = xml_attribute(element, "version");
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Nuget, name),
            relative(root, path),
            DependencyScope::Runtime,
        );
        dependency.declared_requirement = version.map(str::to_string);
        dependency.resolved_version = version.map(str::to_string);
        out.push(dependency);
    }
    Ok(out)
}

fn nuget_lock_versions(dir: &Path) -> Result<BTreeMap<String, String>, InventoryError> {
    let path = dir.join("packages.lock.json");
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let data: JsonValue =
        serde_json::from_slice(&fs::read(&path)?).map_err(|source| InventoryError::Json {
            path: path.clone(),
            source,
        })?;
    let mut versions = BTreeMap::new();
    if let Some(frameworks) = data.get("dependencies").and_then(JsonValue::as_object) {
        for packages in frameworks.values().filter_map(JsonValue::as_object) {
            for (name, details) in packages {
                if let Some(version) = details.get("resolved").and_then(JsonValue::as_str) {
                    versions.insert(name.to_ascii_lowercase(), version.to_string());
                }
            }
        }
    }
    Ok(versions)
}

fn scan_composer(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let data: JsonValue =
        serde_json::from_slice(&fs::read(path)?).map_err(|source| InventoryError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    let resolved = composer_lock_versions(path.parent().unwrap_or(root))?;
    let mut out = Vec::new();
    for (section, scope) in [
        ("require", DependencyScope::Runtime),
        ("require-dev", DependencyScope::Development),
    ] {
        let Some(packages) = data.get(section).and_then(JsonValue::as_object) else {
            continue;
        };
        for (name, requirement) in packages {
            if !name.contains('/') || name.starts_with("ext-") || name.starts_with("lib-") {
                continue;
            }
            let mut dependency = Dependency::new(
                PackageCoordinate::new(Ecosystem::Composer, name),
                relative(root, path),
                scope,
            );
            dependency.declared_requirement = requirement.as_str().map(str::to_string);
            dependency.resolved_version = resolved.get(&dependency.package.name).cloned();
            out.push(dependency);
        }
    }
    Ok(out)
}

fn composer_lock_versions(dir: &Path) -> Result<BTreeMap<String, String>, InventoryError> {
    let path = dir.join("composer.lock");
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let data: JsonValue =
        serde_json::from_slice(&fs::read(&path)?).map_err(|source| InventoryError::Json {
            path: path.clone(),
            source,
        })?;
    let mut versions = BTreeMap::new();
    for section in ["packages", "packages-dev"] {
        for package in data
            .get(section)
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
        {
            if let (Some(name), Some(version)) = (
                package.get("name").and_then(JsonValue::as_str),
                package.get("version").and_then(JsonValue::as_str),
            ) {
                versions.insert(
                    name.to_ascii_lowercase(),
                    version.trim_start_matches('v').into(),
                );
            }
        }
    }
    Ok(versions)
}

fn scan_gemfile(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    static GEM: OnceLock<Regex> = OnceLock::new();
    let gem_re = GEM.get_or_init(|| {
        Regex::new(r#"(?m)^\s*gem\s+['"]([A-Za-z0-9_.-]+)['"](?:\s*,\s*['"]([^'"]+)['"])?"#)
            .expect("valid Gemfile dependency regex")
    });
    let resolved = gem_lock_versions(path.parent().unwrap_or(root))?;
    let source = fs::read_to_string(path)?;
    let mut out = Vec::new();
    for captures in gem_re.captures_iter(&source) {
        let name = &captures[1];
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Gem, name),
            relative(root, path),
            DependencyScope::Runtime,
        );
        dependency.declared_requirement = captures.get(2).map(|value| value.as_str().to_string());
        dependency.resolved_version = resolved.get(&dependency.package.name).cloned();
        out.push(dependency);
    }
    Ok(out)
}

fn gem_lock_versions(dir: &Path) -> Result<BTreeMap<String, String>, InventoryError> {
    let path = dir.join("Gemfile.lock");
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    static VERSION: OnceLock<Regex> = OnceLock::new();
    let version_re = VERSION.get_or_init(|| {
        Regex::new(r"(?m)^\s{4}([A-Za-z0-9_.-]+)\s+\(([^),\s]+)")
            .expect("valid Gemfile.lock version regex")
    });
    let mut versions = BTreeMap::new();
    for captures in version_re.captures_iter(&fs::read_to_string(path)?) {
        versions
            .entry(captures[1].to_ascii_lowercase())
            .or_insert_with(|| captures[2].to_string());
    }
    Ok(versions)
}

fn scan_swift_package(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    static PACKAGE: OnceLock<Regex> = OnceLock::new();
    let package_re = PACKAGE.get_or_init(|| {
        Regex::new(r#"(?s)\.package\s*\([^)]*?url\s*:\s*"([^"]+)"([^)]*)\)"#)
            .expect("valid SwiftPM package regex")
    });
    static REQUIREMENT: OnceLock<Regex> = OnceLock::new();
    let requirement_re = REQUIREMENT.get_or_init(|| {
        Regex::new(r#"(?:from|exact|branch|revision)\s*:\s*"([^"]+)""#)
            .expect("valid SwiftPM requirement regex")
    });
    let resolved = swift_lock_versions(path.parent().unwrap_or(root))?;
    let source = fs::read_to_string(path)?;
    let mut out = Vec::new();
    for captures in package_re.captures_iter(&source) {
        let Some(identity) = repository_identity(&captures[1]) else {
            continue;
        };
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Swift, identity),
            relative(root, path),
            DependencyScope::Runtime,
        );
        dependency.declared_requirement = requirement_re
            .captures(&captures[2])
            .map(|requirement| requirement[1].to_string());
        dependency.resolved_version = resolved.get(&dependency.package.name).cloned();
        out.push(dependency);
    }
    Ok(out)
}

fn swift_lock_versions(dir: &Path) -> Result<BTreeMap<String, String>, InventoryError> {
    let path = dir.join("Package.resolved");
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let data: JsonValue =
        serde_json::from_slice(&fs::read(&path)?).map_err(|source| InventoryError::Json {
            path: path.clone(),
            source,
        })?;
    let pins = data
        .get("pins")
        .or_else(|| data.get("object").and_then(|object| object.get("pins")))
        .and_then(JsonValue::as_array);
    let mut versions = BTreeMap::new();
    for pin in pins.into_iter().flatten() {
        let identity = pin
            .get("identity")
            .or_else(|| pin.get("package"))
            .and_then(JsonValue::as_str)
            .or_else(|| {
                pin.get("location")
                    .or_else(|| pin.get("repositoryURL"))
                    .and_then(JsonValue::as_str)
                    .and_then(repository_identity)
            });
        let version = pin
            .get("state")
            .and_then(|state| state.get("version").or_else(|| state.get("revision")))
            .and_then(JsonValue::as_str);
        if let (Some(identity), Some(version)) = (identity, version) {
            versions.insert(identity.to_ascii_lowercase(), version.to_string());
        }
    }
    Ok(versions)
}

fn repository_identity(url: &str) -> Option<&str> {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .map(|name| name.trim_end_matches(".git"))
        .filter(|name| !name.is_empty())
}

fn scan_pubspec(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let source = fs::read_to_string(path)?;
    let resolved = pub_lock_versions(path.parent().unwrap_or(root))?;
    let mut section = None;
    let mut out = Vec::new();
    for raw in source.lines() {
        if !raw.starts_with(char::is_whitespace) {
            section = match raw.trim() {
                "dependencies:" => Some(DependencyScope::Runtime),
                "dev_dependencies:" => Some(DependencyScope::Development),
                _ => None,
            };
            continue;
        }
        let Some(scope) = section else { continue };
        if !raw.starts_with("  ") || raw.starts_with("    ") {
            continue;
        }
        let Some((name, requirement)) = raw.trim().split_once(':') else {
            continue;
        };
        if name == "flutter" || name == "sdk" {
            continue;
        }
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Pub, name),
            relative(root, path),
            scope,
        );
        let requirement = requirement.trim();
        dependency.declared_requirement =
            (!requirement.is_empty()).then(|| requirement.trim_matches(['\'', '"']).to_string());
        dependency.resolved_version = resolved.get(&dependency.package.name).cloned();
        out.push(dependency);
    }
    Ok(out)
}

fn pub_lock_versions(dir: &Path) -> Result<BTreeMap<String, String>, InventoryError> {
    let path = dir.join("pubspec.lock");
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let source = fs::read_to_string(path)?;
    let mut current = None;
    let mut versions = BTreeMap::new();
    for raw in source.lines() {
        if raw.starts_with("  ") && !raw.starts_with("    ") && raw.trim_end().ends_with(':') {
            current = Some(raw.trim().trim_end_matches(':').to_ascii_lowercase());
        } else if raw.starts_with("    version:") {
            if let Some(name) = current.take() {
                versions.insert(
                    name,
                    raw.trim()["version:".len()..]
                        .trim()
                        .trim_matches(['\'', '"'])
                        .to_string(),
                );
            }
        }
    }
    Ok(versions)
}

fn scan_mix(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    static DEP: OnceLock<Regex> = OnceLock::new();
    let dep_re = DEP.get_or_init(|| {
        Regex::new(r#"\{\s*:([a-zA-Z0-9_]+)\s*,\s*"([^"]+)"([^}]*)\}"#)
            .expect("valid Mix dependency regex")
    });
    let source = fs::read_to_string(path)?;
    let resolved = mix_lock_versions(path.parent().unwrap_or(root))?;
    let mut out = Vec::new();
    for captures in dep_re.captures_iter(&source) {
        let scope = if captures[3].contains("only: :dev") || captures[3].contains("only: :test") {
            DependencyScope::Development
        } else {
            DependencyScope::Runtime
        };
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Hex, &captures[1]),
            relative(root, path),
            scope,
        );
        dependency.declared_requirement = Some(captures[2].to_string());
        dependency.resolved_version = resolved.get(&dependency.package.name).cloned();
        out.push(dependency);
    }
    Ok(out)
}

fn mix_lock_versions(dir: &Path) -> Result<BTreeMap<String, String>, InventoryError> {
    let path = dir.join("mix.lock");
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    static HEX: OnceLock<Regex> = OnceLock::new();
    let hex_re = HEX.get_or_init(|| {
        Regex::new(r#""([a-zA-Z0-9_]+)"\s*:\s*\{:hex\s*,\s*:[a-zA-Z0-9_]+\s*,\s*"([^"]+)""#)
            .expect("valid mix.lock regex")
    });
    Ok(hex_re
        .captures_iter(&fs::read_to_string(path)?)
        .map(|captures| (captures[1].to_ascii_lowercase(), captures[2].to_string()))
        .collect())
}

fn scan_rockspec(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    static BLOCK: OnceLock<Regex> = OnceLock::new();
    let block_re = BLOCK.get_or_init(|| {
        Regex::new(r"(?s)\bdependencies\s*=\s*\{(.*?)\}")
            .expect("valid rockspec dependency block regex")
    });
    static ENTRY: OnceLock<Regex> = OnceLock::new();
    let entry_re = ENTRY.get_or_init(|| {
        Regex::new(r#"["']([A-Za-z0-9_.-]+)([^"']*)["']"#).expect("valid rockspec dependency regex")
    });
    let source = fs::read_to_string(path)?;
    let resolved = luarocks_lock_versions(path.parent().unwrap_or(root))?;
    let mut out = Vec::new();
    for block in block_re.captures_iter(&source) {
        for captures in entry_re.captures_iter(&block[1]) {
            if &captures[1] == "lua" {
                continue;
            }
            let mut dependency = Dependency::new(
                PackageCoordinate::new(Ecosystem::Luarocks, &captures[1]),
                relative(root, path),
                DependencyScope::Runtime,
            );
            let requirement = captures[2].trim();
            dependency.declared_requirement =
                (!requirement.is_empty()).then(|| requirement.to_string());
            dependency.resolved_version = resolved.get(&dependency.package.name).cloned();
            out.push(dependency);
        }
    }
    Ok(out)
}

fn luarocks_lock_versions(dir: &Path) -> Result<BTreeMap<String, String>, InventoryError> {
    let path = dir.join("luarocks.lock");
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    static VERSION: OnceLock<Regex> = OnceLock::new();
    let version_re = VERSION.get_or_init(|| {
        Regex::new(r#"\["([A-Za-z0-9_.-]+)"\]\s*=\s*\{\s*\["([^"]+)"\]"#)
            .expect("valid luarocks.lock regex")
    });
    Ok(version_re
        .captures_iter(&fs::read_to_string(path)?)
        .map(|captures| (captures[1].to_ascii_lowercase(), captures[2].to_string()))
        .collect())
}

fn scan_julia_project(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let source = fs::read_to_string(path)?;
    let data: toml::Value = toml::from_str(&source).map_err(|source| InventoryError::Toml {
        path: path.to_path_buf(),
        source,
    })?;
    let resolved = julia_manifest_versions(path.parent().unwrap_or(root))?;
    let compat = data.get("compat").and_then(toml::Value::as_table);
    let mut out = Vec::new();
    for name in data
        .get("deps")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|dependencies| dependencies.keys())
    {
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Julia, name),
            relative(root, path),
            DependencyScope::Runtime,
        );
        dependency.declared_requirement = compat
            .and_then(|entries| entries.get(name))
            .and_then(toml::Value::as_str)
            .map(str::to_string);
        dependency.resolved_version = resolved.get(&dependency.package.name).cloned();
        out.push(dependency);
    }
    Ok(out)
}

fn julia_manifest_versions(dir: &Path) -> Result<BTreeMap<String, String>, InventoryError> {
    let path = dir.join("Manifest.toml");
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let source = fs::read_to_string(&path)?;
    let data: toml::Value = toml::from_str(&source).map_err(|source| InventoryError::Toml {
        path: path.clone(),
        source,
    })?;
    let mut versions = BTreeMap::new();
    if let Some(dependencies) = data.get("deps").and_then(toml::Value::as_table) {
        for (name, entries) in dependencies {
            let version = entries
                .as_array()
                .and_then(|entries| entries.first())
                .or(Some(entries))
                .and_then(|entry| entry.get("version"))
                .and_then(toml::Value::as_str);
            if let Some(version) = version {
                versions.insert(name.to_ascii_lowercase(), version.to_string());
            }
        }
    }
    Ok(versions)
}

fn scan_zig_package(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    static DEP: OnceLock<Regex> = OnceLock::new();
    let dep_re = DEP.get_or_init(|| {
        Regex::new(r#"(?s)\.([A-Za-z_]\w*)\s*=\s*\.\{\s*\.url\s*=\s*"([^"]+)"[^}]*\}"#)
            .expect("valid Zig package dependency regex")
    });
    static VERSION: OnceLock<Regex> = OnceLock::new();
    let version_re = VERSION.get_or_init(|| {
        Regex::new(r"(?:^|[/_-])(v?\d+\.\d+\.\d+)(?:[/.?_-]|$)").expect("valid URL version regex")
    });
    let source = fs::read_to_string(path)?;
    let mut out = Vec::new();
    for captures in dep_re.captures_iter(&source) {
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Zig, &captures[1]),
            relative(root, path),
            DependencyScope::Runtime,
        );
        dependency.declared_requirement = Some(captures[2].to_string());
        dependency.resolved_version = version_re
            .captures(&captures[2])
            .map(|version| version[1].to_string());
        out.push(dependency);
    }
    Ok(out)
}

fn scan_podfile(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    static POD: OnceLock<Regex> = OnceLock::new();
    let pod_re = POD.get_or_init(|| {
        Regex::new(r#"(?m)^\s*pod\s+['"]([^'"]+)['"](?:\s*,\s*['"]([^'"]+)['"])?"#)
            .expect("valid Podfile dependency regex")
    });
    let resolved = pod_lock_versions(path.parent().unwrap_or(root))?;
    let source = fs::read_to_string(path)?;
    let mut out = Vec::new();
    for captures in pod_re.captures_iter(&source) {
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Cocoapods, &captures[1]),
            relative(root, path),
            DependencyScope::Runtime,
        );
        dependency.declared_requirement = captures.get(2).map(|value| value.as_str().to_string());
        dependency.resolved_version = resolved.get(&dependency.package.name).cloned();
        out.push(dependency);
    }
    Ok(out)
}

fn pod_lock_versions(dir: &Path) -> Result<BTreeMap<String, String>, InventoryError> {
    let path = dir.join("Podfile.lock");
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    static VERSION: OnceLock<Regex> = OnceLock::new();
    let version_re = VERSION.get_or_init(|| {
        Regex::new(r"(?m)^\s{2}-\s+([^\s/(]+)(?:/[^\s(]+)?\s+\(([^)]+)\)")
            .expect("valid Podfile.lock version regex")
    });
    let mut versions = BTreeMap::new();
    for captures in version_re.captures_iter(&fs::read_to_string(path)?) {
        versions
            .entry(captures[1].to_ascii_lowercase())
            .or_insert_with(|| captures[2].to_string());
    }
    Ok(versions)
}

fn scan_conan(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let source = fs::read_to_string(path)?;
    let mut requirements = Vec::new();
    if path.file_name().is_some_and(|name| name == "conanfile.txt") {
        let mut in_requires = false;
        for raw in source.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.starts_with('[') {
                in_requires = line.eq_ignore_ascii_case("[requires]");
                continue;
            }
            if in_requires && !line.is_empty() {
                requirements.push(line.to_string());
            }
        }
    } else {
        static REQUIRES: OnceLock<Regex> = OnceLock::new();
        let requires_re = REQUIRES.get_or_init(|| {
            Regex::new(r#"["']([A-Za-z0-9_.+-]+/[A-Za-z0-9_.+-]+)["']"#)
                .expect("valid conanfile.py requirement regex")
        });
        requirements.extend(
            requires_re
                .captures_iter(&source)
                .map(|captures| captures[1].to_string()),
        );
    }
    let mut out = Vec::new();
    for requirement in requirements {
        let coordinate = requirement.split('@').next().unwrap_or(&requirement);
        let Some((name, version)) = coordinate.split_once('/') else {
            continue;
        };
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Conan, name),
            relative(root, path),
            DependencyScope::Runtime,
        );
        dependency.declared_requirement = Some(version.to_string());
        dependency.resolved_version = Some(version.to_string());
        out.push(dependency);
    }
    Ok(out)
}

fn scan_vcpkg(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let data: JsonValue =
        serde_json::from_slice(&fs::read(path)?).map_err(|source| InventoryError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    let overrides = data
        .get("overrides")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?.to_ascii_lowercase();
            let version = [
                "version",
                "version-string",
                "version-semver",
                "version-date",
            ]
            .iter()
            .find_map(|field| entry.get(field).and_then(JsonValue::as_str))?;
            Some((name, version.to_string()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut out = Vec::new();
    for entry in data
        .get("dependencies")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
    {
        let (name, requirement) = match entry {
            JsonValue::String(name) => (name.as_str(), None),
            JsonValue::Object(details) => {
                let Some(name) = details.get("name").and_then(JsonValue::as_str) else {
                    continue;
                };
                let requirement = details
                    .get("version>=")
                    .or_else(|| details.get("version"))
                    .and_then(JsonValue::as_str);
                (name, requirement)
            }
            _ => continue,
        };
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Vcpkg, name),
            relative(root, path),
            DependencyScope::Runtime,
        );
        dependency.declared_requirement = requirement.map(str::to_string);
        dependency.resolved_version = overrides
            .get(&dependency.package.name)
            .cloned()
            .or_else(|| requirement.map(str::to_string));
        out.push(dependency);
    }
    Ok(out)
}

fn scan_powershell_manifest(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    static MODULE: OnceLock<Regex> = OnceLock::new();
    let module_re = MODULE.get_or_init(|| {
        Regex::new(
            r#"(?is)ModuleName\s*=\s*['"]([^'"]+)['"][^}]*?ModuleVersion\s*=\s*['"]([^'"]+)['"]"#,
        )
        .expect("valid PowerShell RequiredModules regex")
    });
    let source = fs::read_to_string(path)?;
    let mut out = Vec::new();
    for captures in module_re.captures_iter(&source) {
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Powershell, &captures[1]),
            relative(root, path),
            DependencyScope::Runtime,
        );
        dependency.declared_requirement = Some(captures[2].to_string());
        dependency.resolved_version = Some(captures[2].to_string());
        out.push(dependency);
    }
    Ok(out)
}

fn scan_fpm(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let source = fs::read_to_string(path)?;
    let data: toml::Value = toml::from_str(&source).map_err(|source| InventoryError::Toml {
        path: path.to_path_buf(),
        source,
    })?;
    let mut out = Vec::new();
    for (name, value) in data
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flatten()
    {
        let version = match value {
            toml::Value::String(version) => Some(version.as_str()),
            toml::Value::Table(details) => ["version", "tag", "rev"]
                .iter()
                .find_map(|field| details.get(*field).and_then(toml::Value::as_str)),
            _ => None,
        };
        let mut dependency = Dependency::new(
            PackageCoordinate::new(Ecosystem::Fpm, name),
            relative(root, path),
            DependencyScope::Runtime,
        );
        dependency.declared_requirement = version.map(str::to_string);
        dependency.resolved_version = version.map(str::to_string);
        out.push(dependency);
    }
    Ok(out)
}

fn scan_qlpack(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let source = fs::read_to_string(path)?;
    let entries = yaml_mapping_section(&source, "dependencies");
    Ok(entries
        .into_iter()
        .map(|(name, version)| {
            let mut dependency = Dependency::new(
                PackageCoordinate::new(Ecosystem::Codeql, name),
                relative(root, path),
                DependencyScope::Runtime,
            );
            dependency.declared_requirement = Some(version.clone());
            dependency.resolved_version = Some(version);
            dependency
        })
        .collect())
}

fn yaml_mapping_section(source: &str, section_name: &str) -> Vec<(String, String)> {
    let mut in_section = false;
    let mut out = Vec::new();
    for raw in source.lines() {
        if !raw.starts_with(char::is_whitespace) {
            in_section = raw.trim() == format!("{section_name}:");
            continue;
        }
        if !in_section || !raw.starts_with("  ") || raw.starts_with("    ") {
            continue;
        }
        let Some((name, value)) = raw.trim().rsplit_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['\'', '"']);
        if !name.trim().is_empty() && !value.is_empty() {
            out.push((name.trim().to_string(), value.to_string()));
        }
    }
    out
}

fn scan_salesforce(root: &Path, path: &Path) -> Result<Vec<Dependency>, InventoryError> {
    let data: JsonValue =
        serde_json::from_slice(&fs::read(path)?).map_err(|source| InventoryError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    let mut out = Vec::new();
    for directory in data
        .get("packageDirectories")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
    {
        for entry in directory
            .get("dependencies")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
        {
            let Some(raw) = entry.get("package").and_then(JsonValue::as_str) else {
                continue;
            };
            let (name, version) = raw
                .split_once('@')
                .map_or((raw, None), |(name, version)| (name, Some(version)));
            let mut dependency = Dependency::new(
                PackageCoordinate::new(Ecosystem::Salesforce, name),
                relative(root, path),
                DependencyScope::Runtime,
            );
            dependency.declared_requirement = version.map(str::to_string);
            dependency.resolved_version = version.map(str::to_string);
            out.push(dependency);
        }
    }
    Ok(out)
}

fn ancestors_within<'a>(root: &'a Path, path: &'a Path) -> impl Iterator<Item = &'a Path> {
    path.ancestors()
        .take_while(move |candidate| candidate.starts_with(root))
}

fn resolve_property(value: &str, properties: &BTreeMap<String, String>) -> String {
    value
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
        .and_then(|key| properties.get(key))
        .cloned()
        .unwrap_or_else(|| value.to_string())
}

fn xml_element<'a>(source: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = source.find(&open)? + open.len();
    let end = source[start..].find(&close)? + start;
    Some(&source[start..end])
}

fn xml_elements<'a>(source: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut rest = source;
    let mut out = Vec::new();
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else { break };
        out.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    out
}

fn xml_start_tags<'a>(source: &'a str, tag: &str) -> Vec<&'a str> {
    let needle = format!("<{tag}");
    let mut rest = source;
    let mut out = Vec::new();
    while let Some(start) = rest.find(&needle) {
        let after = &rest[start..];
        let Some(end) = after.find('>') else { break };
        out.push(&after[..=end]);
        rest = &after[end + 1..];
    }
    out
}

fn xml_attribute<'a>(element: &'a str, name: &str) -> Option<&'a str> {
    for quote in ['"', '\''] {
        let needle = format!("{name}={quote}");
        if let Some(start) = element.find(&needle) {
            let value = &element[start + needle.len()..];
            return value.find(quote).map(|end| &value[..end]);
        }
    }
    None
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[derive(Debug, thiserror::Error)]
pub enum InventoryError {
    #[error("API inventory root is not a directory: {0}")]
    InvalidRoot(PathBuf),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON manifest {path}: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("invalid TOML manifest {path}: {source}")]
    Toml {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("unrecognized SBOM JSON document {0}")]
    UnknownSbomFormat(PathBuf),
    #[error("invalid SBOM XML {path}: {message}")]
    InvalidXml { path: PathBuf, message: String },
    #[error("invalid package URL {value:?} in {path}: {message}")]
    InvalidPackageUrl {
        path: PathBuf,
        value: String,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pep_508_names_extras_and_markers() {
        assert_eq!(
            parse_python_requirement("Requests[socks]>=2.0; python_version > '3.9'"),
            Some(("Requests".into(), Some(">=2.0".into())))
        );
    }

    #[test]
    fn gradle_inventory_resolves_root_properties_and_custom_configurations() {
        let repository = tempfile::tempdir().unwrap();
        fs::create_dir_all(repository.path().join("fabric")).unwrap();
        fs::write(
            repository.path().join("gradle.properties"),
            "loader.version=0.19.3\nobj.version=0.4.0\n",
        )
        .unwrap();
        fs::write(
            repository.path().join("fabric/build.gradle.kts"),
            r#"
dependencies {
    modImplementation("net.fabricmc:fabric-loader:${rootProject.property("loader.version")}")
    implementation("de.javagl:obj:${rootProject.property("obj.version")}")
    modCompileOnly("example:optional:1.2.3")
}
"#,
        )
        .unwrap();

        let dependencies = scan_dependencies(repository.path()).unwrap();

        assert!(dependencies.iter().any(|dependency| {
            dependency.package.name == "net.fabricmc:fabric-loader"
                && dependency.resolved_version.as_deref() == Some("0.19.3")
                && dependency.scope == DependencyScope::Runtime
        }));
        assert!(dependencies.iter().any(|dependency| {
            dependency.package.name == "de.javagl:obj"
                && dependency.resolved_version.as_deref() == Some("0.4.0")
        }));
        assert!(dependencies.iter().any(|dependency| {
            dependency.package.name == "example:optional"
                && dependency.resolved_version.as_deref() == Some("1.2.3")
                && dependency.scope == DependencyScope::Optional
        }));
    }

    #[test]
    fn gradle_inventory_does_not_treat_dynamic_or_unresolved_versions_as_pinned() {
        let properties = BTreeMap::new();

        assert_eq!(resolve_gradle_version("1.+", &properties), None);
        assert_eq!(resolve_gradle_version("latest.release", &properties), None);
        assert_eq!(
            resolve_gradle_version(r#"${rootProject.property("missing.version")}"#, &properties),
            None
        );
        assert_eq!(
            resolve_gradle_version("0.116.14+1.21.1", &properties),
            Some("0.116.14+1.21.1".to_string())
        );
    }
}
