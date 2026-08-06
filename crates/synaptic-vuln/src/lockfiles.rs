//! Lockfile readers, one per ecosystem.
//!
//! Every reader turns a lockfile into resolved `package@version` nodes. Where
//! the format records what each package depends on, the edges come with it and
//! findings can report a dependency path. Where it does not, the packages are
//! still returned: knowing a vulnerable version is installed matters even when
//! the path to it cannot be shown, and silently skipping those ecosystems would
//! be the worst of the options.

use serde::{Deserialize, Serialize};
use synaptic_api::{Ecosystem, PackageCoordinate};

use crate::lockgraph::{LockGraphError, PackageKey, ResolvedPackage};

/// A lockfile format this crate can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockfileKind {
    CargoLock,
    NpmPackageLock,
    PnpmLock,
    YarnLock,
    PoetryLock,
    UvLock,
    ComposerLock,
    GemfileLock,
    NuGetPackagesLock,
    GoMod,
    SwiftPackageResolved,
    PubspecLock,
    MixLock,
    PodfileLock,
    GradleLockfile,
}

impl LockfileKind {
    /// Recognise a lockfile by file name.
    pub fn for_file_name(name: &str) -> Option<Self> {
        Some(match name {
            "Cargo.lock" => Self::CargoLock,
            "package-lock.json" => Self::NpmPackageLock,
            "pnpm-lock.yaml" => Self::PnpmLock,
            "yarn.lock" => Self::YarnLock,
            "poetry.lock" => Self::PoetryLock,
            "uv.lock" => Self::UvLock,
            "composer.lock" => Self::ComposerLock,
            "Gemfile.lock" => Self::GemfileLock,
            "packages.lock.json" => Self::NuGetPackagesLock,
            "go.mod" => Self::GoMod,
            "Package.resolved" => Self::SwiftPackageResolved,
            "pubspec.lock" => Self::PubspecLock,
            "mix.lock" => Self::MixLock,
            "Podfile.lock" => Self::PodfileLock,
            "gradle.lockfile" => Self::GradleLockfile,
            _ => return None,
        })
    }

    pub fn file_name(self) -> &'static str {
        match self {
            Self::CargoLock => "Cargo.lock",
            Self::NpmPackageLock => "package-lock.json",
            Self::PnpmLock => "pnpm-lock.yaml",
            Self::YarnLock => "yarn.lock",
            Self::PoetryLock => "poetry.lock",
            Self::UvLock => "uv.lock",
            Self::ComposerLock => "composer.lock",
            Self::GemfileLock => "Gemfile.lock",
            Self::NuGetPackagesLock => "packages.lock.json",
            Self::GoMod => "go.mod",
            Self::SwiftPackageResolved => "Package.resolved",
            Self::PubspecLock => "pubspec.lock",
            Self::MixLock => "mix.lock",
            Self::PodfileLock => "Podfile.lock",
            Self::GradleLockfile => "gradle.lockfile",
        }
    }

    pub fn ecosystem(self) -> Ecosystem {
        match self {
            Self::CargoLock => Ecosystem::Cargo,
            Self::NpmPackageLock | Self::PnpmLock | Self::YarnLock => Ecosystem::Npm,
            Self::PoetryLock | Self::UvLock => Ecosystem::Pypi,
            Self::ComposerLock => Ecosystem::Composer,
            Self::GemfileLock => Ecosystem::Gem,
            Self::NuGetPackagesLock => Ecosystem::Nuget,
            Self::GoMod => Ecosystem::Go,
            Self::SwiftPackageResolved => Ecosystem::Swift,
            Self::PubspecLock => Ecosystem::Pub,
            Self::MixLock => Ecosystem::Hex,
            Self::PodfileLock => Ecosystem::Cocoapods,
            Self::GradleLockfile => Ecosystem::Maven,
        }
    }

    /// Whether the format records per-package dependency lists, and therefore
    /// whether findings from it can show a dependency path.
    pub fn provides_dependency_edges(self) -> bool {
        matches!(
            self,
            Self::CargoLock
                | Self::NpmPackageLock
                | Self::PnpmLock
                | Self::YarnLock
                | Self::PoetryLock
                | Self::UvLock
                | Self::ComposerLock
                | Self::GemfileLock
                | Self::NuGetPackagesLock
                | Self::PodfileLock
        )
    }

    /// Every format, for coverage reporting.
    pub fn all() -> &'static [LockfileKind] {
        &[
            Self::CargoLock,
            Self::NpmPackageLock,
            Self::PnpmLock,
            Self::YarnLock,
            Self::PoetryLock,
            Self::UvLock,
            Self::ComposerLock,
            Self::GemfileLock,
            Self::NuGetPackagesLock,
            Self::GoMod,
            Self::SwiftPackageResolved,
            Self::PubspecLock,
            Self::MixLock,
            Self::PodfileLock,
            Self::GradleLockfile,
        ]
    }
}

/// Parse a lockfile into resolved packages.
pub fn parse(kind: LockfileKind, source: &str) -> Result<Vec<ResolvedPackage>, LockGraphError> {
    match kind {
        LockfileKind::CargoLock => crate::lockgraph::parse_cargo_lock(source),
        LockfileKind::NpmPackageLock => parse_npm_package_lock(source),
        LockfileKind::PnpmLock => parse_pnpm_lock(source),
        LockfileKind::YarnLock => Ok(parse_yarn_lock(source)),
        LockfileKind::PoetryLock => parse_poetry_lock(source),
        LockfileKind::UvLock => parse_uv_lock(source),
        LockfileKind::ComposerLock => parse_composer_lock(source),
        LockfileKind::GemfileLock => Ok(parse_gemfile_lock(source)),
        LockfileKind::NuGetPackagesLock => parse_nuget_lock(source),
        LockfileKind::GoMod => Ok(parse_go_mod(source)),
        LockfileKind::SwiftPackageResolved => parse_swift_resolved(source),
        LockfileKind::PubspecLock => parse_pubspec_lock(source),
        LockfileKind::MixLock => Ok(parse_mix_lock(source)),
        LockfileKind::PodfileLock => parse_podfile_lock(source),
        LockfileKind::GradleLockfile => Ok(parse_gradle_lockfile(source)),
    }
}

fn key(ecosystem: Ecosystem, name: &str, version: &str) -> PackageKey {
    PackageKey::new(PackageCoordinate::new(ecosystem, name), version.trim())
}

fn package(ecosystem: Ecosystem, name: &str, version: &str) -> ResolvedPackage {
    ResolvedPackage {
        key: key(ecosystem, name, version),
        dependencies: Vec::new(),
        is_workspace_member: false,
    }
}

/// Resolve dependency names to keys using a name index, dropping names that the
/// lockfile does not resolve. A dangling name is not worth aborting a scan for.
///
/// Both sides go through the ecosystem's name normalization. Lockfiles spell a
/// dependency the way its author typed it (`Alamofire`, `My_Dep`) while the
/// index is keyed on the canonical form, so skipping this silently produces a
/// graph with no edges at all.
fn link(
    packages: &mut [ResolvedPackage],
    named: &[(String, Vec<String>)],
    index: &std::collections::BTreeMap<String, PackageKey>,
    ecosystem: Ecosystem,
) {
    for (package, (_, dependency_names)) in packages.iter_mut().zip(named) {
        package.dependencies = dependency_names
            .iter()
            .filter_map(|name| {
                let normalized = PackageCoordinate::new(ecosystem, name).name;
                index.get(normalized.as_str()).cloned()
            })
            .collect();
    }
}

// ---------------------------------------------------------------- node

#[derive(Debug, Deserialize)]
struct NpmLock {
    #[serde(default)]
    packages: std::collections::BTreeMap<String, NpmLockEntry>,
    #[serde(default)]
    dependencies: std::collections::BTreeMap<String, NpmV1Entry>,
}

#[derive(Debug, Deserialize)]
struct NpmLockEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    dependencies: std::collections::BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct NpmV1Entry {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    requires: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    dependencies: std::collections::BTreeMap<String, NpmV1Entry>,
}

fn parse_npm_package_lock(source: &str) -> Result<Vec<ResolvedPackage>, LockGraphError> {
    let lock: NpmLock =
        serde_json::from_str(source).map_err(|error| LockGraphError::Format(error.to_string()))?;

    if !lock.packages.is_empty() {
        let mut packages = Vec::new();
        let mut named = Vec::new();
        let mut index = std::collections::BTreeMap::new();

        for (path, entry) in &lock.packages {
            // "" is the project itself; nested paths end with the real name.
            let is_root = path.is_empty();
            let name = if is_root {
                entry.name.clone().unwrap_or_else(|| "(root)".into())
            } else {
                path.rsplit("node_modules/")
                    .next()
                    .unwrap_or(path)
                    .to_string()
            };
            let Some(version) = entry.version.clone() else {
                continue;
            };
            let mut resolved = package(Ecosystem::Npm, &name, &version);
            resolved.is_workspace_member = is_root;
            // Shallower paths win, matching npm's own resolution order.
            index
                .entry(name.clone())
                .or_insert_with(|| resolved.key.clone());
            let dependency_names = entry
                .dependencies
                .keys()
                .chain(entry.optional_dependencies.keys())
                .cloned()
                .collect::<Vec<_>>();
            packages.push(resolved);
            named.push((name, dependency_names));
        }
        link(&mut packages, &named, &index, Ecosystem::Npm);
        return Ok(packages);
    }

    // lockfileVersion 1: a recursive `dependencies` tree.
    let mut packages = Vec::new();
    let mut named = Vec::new();
    collect_npm_v1(&lock.dependencies, &mut packages, &mut named);
    let index = packages
        .iter()
        .map(|entry| (entry.key.coordinate.name.clone(), entry.key.clone()))
        .collect();
    link(&mut packages, &named, &index, Ecosystem::Npm);
    Ok(packages)
}

fn collect_npm_v1(
    entries: &std::collections::BTreeMap<String, NpmV1Entry>,
    packages: &mut Vec<ResolvedPackage>,
    named: &mut Vec<(String, Vec<String>)>,
) {
    for (name, entry) in entries {
        if let Some(version) = &entry.version {
            packages.push(package(Ecosystem::Npm, name, version));
            named.push((name.clone(), entry.requires.keys().cloned().collect()));
        }
        collect_npm_v1(&entry.dependencies, packages, named);
    }
}

fn parse_pnpm_lock(source: &str) -> Result<Vec<ResolvedPackage>, LockGraphError> {
    let document: serde_json::Value = serde_norway::from_str(source)
        .map_err(|error| LockGraphError::Format(error.to_string()))?;

    let mut packages = Vec::new();
    let mut named = Vec::new();
    // v6 and earlier key on `packages`; v9 moves resolved trees to `snapshots`.
    for section in ["snapshots", "packages"] {
        let Some(entries) = document.get(section).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (raw, entry) in entries {
            let Some((name, version)) = split_pnpm_key(raw) else {
                continue;
            };
            if packages.iter().any(|existing: &ResolvedPackage| {
                existing.key.coordinate.name == name && existing.key.version == version
            }) {
                continue;
            }
            let dependency_names = entry
                .get("dependencies")
                .and_then(serde_json::Value::as_object)
                .map(|deps| deps.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            packages.push(package(Ecosystem::Npm, &name, &version));
            named.push((name, dependency_names));
        }
    }
    let index = packages
        .iter()
        .map(|entry| (entry.key.coordinate.name.clone(), entry.key.clone()))
        .collect();
    link(&mut packages, &named, &index, Ecosystem::Npm);
    Ok(packages)
}

/// `/lodash@4.17.21`, `/@scope/pkg@1.0.0`, or the unprefixed v9 spelling.
fn split_pnpm_key(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim_start_matches('/');
    // Drop peer-dependency suffixes such as `(react@18.0.0)`.
    let trimmed = trimmed.split('(').next().unwrap_or(trimmed);
    let at = trimmed.rfind('@')?;
    if at == 0 {
        return None;
    }
    let (name, version) = trimmed.split_at(at);
    let version = version.trim_start_matches('@');
    if name.is_empty() || version.is_empty() {
        return None;
    }
    Some((name.to_string(), version.to_string()))
}

fn parse_yarn_lock(source: &str) -> Vec<ResolvedPackage> {
    let mut packages = Vec::new();
    let mut named = Vec::new();
    let mut current: Option<(String, Option<String>, Vec<String>)> = None;
    let mut in_dependencies = false;

    let flush = |slot: &mut Option<(String, Option<String>, Vec<String>)>,
                 packages: &mut Vec<ResolvedPackage>,
                 named: &mut Vec<(String, Vec<String>)>| {
        if let Some((name, Some(version), dependencies)) = slot.take() {
            packages.push(package(Ecosystem::Npm, &name, &version));
            named.push((name, dependencies));
        }
    };

    for line in source.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            flush(&mut current, &mut packages, &mut named);
            in_dependencies = false;
            // `lodash@^4.0.0, lodash@^4.17.0:` -> lodash
            let header = line.trim_end().trim_end_matches(':');
            let first = header.split(',').next().unwrap_or(header).trim();
            let first = first.trim_matches('"');
            if let Some(at) = first.rfind('@') {
                if at > 0 {
                    current = Some((first[..at].to_string(), None, Vec::new()));
                }
            }
            continue;
        }
        let trimmed = line.trim();
        if indent == 2 {
            in_dependencies = trimmed.starts_with("dependencies:");
            if let Some(rest) = trimmed.strip_prefix("version ") {
                if let Some(entry) = current.as_mut() {
                    entry.1 = Some(rest.trim().trim_matches('"').to_string());
                }
            }
            continue;
        }
        if in_dependencies && indent >= 4 {
            let name = trimmed.split_whitespace().next().unwrap_or_default();
            let name = name.trim_matches('"').trim_end_matches(':');
            if !name.is_empty() {
                if let Some(entry) = current.as_mut() {
                    entry.2.push(name.to_string());
                }
            }
        }
    }
    flush(&mut current, &mut packages, &mut named);

    let index = packages
        .iter()
        .map(|entry| (entry.key.coordinate.name.clone(), entry.key.clone()))
        .collect();
    link(&mut packages, &named, &index, Ecosystem::Npm);
    packages
}

// ---------------------------------------------------------------- python

fn parse_poetry_lock(source: &str) -> Result<Vec<ResolvedPackage>, LockGraphError> {
    let document: toml::Value =
        toml::from_str(source).map_err(|error| LockGraphError::Format(error.to_string()))?;
    let entries = document
        .get("package")
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut packages = Vec::new();
    let mut named = Vec::new();
    for entry in entries {
        let (Some(name), Some(version)) = (
            entry.get("name").and_then(toml::Value::as_str),
            entry.get("version").and_then(toml::Value::as_str),
        ) else {
            continue;
        };
        let dependency_names = entry
            .get("dependencies")
            .and_then(toml::Value::as_table)
            .map(|table| table.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        packages.push(package(Ecosystem::Pypi, name, version));
        named.push((name.to_string(), dependency_names));
    }
    let index = packages
        .iter()
        .map(|entry| (entry.key.coordinate.name.clone(), entry.key.clone()))
        .collect();
    link(&mut packages, &named, &index, Ecosystem::Pypi);
    Ok(packages)
}

fn parse_uv_lock(source: &str) -> Result<Vec<ResolvedPackage>, LockGraphError> {
    let document: toml::Value =
        toml::from_str(source).map_err(|error| LockGraphError::Format(error.to_string()))?;
    let entries = document
        .get("package")
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut packages = Vec::new();
    let mut named = Vec::new();
    for entry in entries {
        let (Some(name), Some(version)) = (
            entry.get("name").and_then(toml::Value::as_str),
            entry.get("version").and_then(toml::Value::as_str),
        ) else {
            continue;
        };
        // uv spells dependencies as an array of tables: [{ name = "x" }].
        let dependency_names = entry
            .get("dependencies")
            .and_then(toml::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("name").and_then(toml::Value::as_str))
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        packages.push(package(Ecosystem::Pypi, name, version));
        named.push((name.to_string(), dependency_names));
    }
    let index = packages
        .iter()
        .map(|entry| (entry.key.coordinate.name.clone(), entry.key.clone()))
        .collect();
    link(&mut packages, &named, &index, Ecosystem::Pypi);
    Ok(packages)
}

// ---------------------------------------------------------------- php / ruby

fn parse_composer_lock(source: &str) -> Result<Vec<ResolvedPackage>, LockGraphError> {
    let document: serde_json::Value =
        serde_json::from_str(source).map_err(|error| LockGraphError::Format(error.to_string()))?;

    let mut packages = Vec::new();
    let mut named = Vec::new();
    for section in ["packages", "packages-dev"] {
        let Some(entries) = document.get(section).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for entry in entries {
            let (Some(name), Some(version)) = (
                entry.get("name").and_then(serde_json::Value::as_str),
                entry.get("version").and_then(serde_json::Value::as_str),
            ) else {
                continue;
            };
            let dependency_names = entry
                .get("require")
                .and_then(serde_json::Value::as_object)
                .map(|table| {
                    table
                        .keys()
                        // `php`, `ext-*` and `lib-*` are platform requirements.
                        .filter(|requirement| {
                            requirement.contains('/')
                                && !requirement.starts_with("ext-")
                                && !requirement.starts_with("lib-")
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            packages.push(package(Ecosystem::Composer, name, version));
            named.push((name.to_string(), dependency_names));
        }
    }
    let index = packages
        .iter()
        .map(|entry| (entry.key.coordinate.name.clone(), entry.key.clone()))
        .collect();
    link(&mut packages, &named, &index, Ecosystem::Composer);
    Ok(packages)
}

fn parse_gemfile_lock(source: &str) -> Vec<ResolvedPackage> {
    let mut packages = Vec::new();
    let mut named: Vec<(String, Vec<String>)> = Vec::new();
    let mut in_specs = false;

    for line in source.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            continue;
        }
        let indent = trimmed.len() - trimmed.trim_start().len();
        if indent == 0 {
            in_specs = false;
            continue;
        }
        if trimmed.trim() == "specs:" {
            in_specs = true;
            continue;
        }
        if !in_specs {
            continue;
        }
        let content = trimmed.trim();
        // 4 spaces: `name (version)`. 6 spaces: `dependency (requirement)`.
        if indent == 4 {
            let mut parts = content.splitn(2, " (");
            let name = parts.next().unwrap_or_default().trim();
            let version = parts
                .next()
                .unwrap_or_default()
                .trim_end_matches(')')
                .trim();
            if !name.is_empty() && !version.is_empty() {
                packages.push(package(Ecosystem::Gem, name, version));
                named.push((name.to_string(), Vec::new()));
            }
        } else if indent >= 6 {
            let name = content.split(" (").next().unwrap_or_default().trim();
            if !name.is_empty() {
                if let Some(last) = named.last_mut() {
                    last.1.push(name.to_string());
                }
            }
        }
    }
    let index = packages
        .iter()
        .map(|entry| (entry.key.coordinate.name.clone(), entry.key.clone()))
        .collect();
    link(&mut packages, &named, &index, Ecosystem::Gem);
    packages
}

// ---------------------------------------------------------------- dotnet

fn parse_nuget_lock(source: &str) -> Result<Vec<ResolvedPackage>, LockGraphError> {
    let document: serde_json::Value =
        serde_json::from_str(source).map_err(|error| LockGraphError::Format(error.to_string()))?;
    let frameworks = document
        .get("dependencies")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();

    let mut packages = Vec::new();
    let mut named = Vec::new();
    for entries in frameworks.values() {
        let Some(entries) = entries.as_object() else {
            continue;
        };
        for (name, entry) in entries {
            let Some(version) = entry.get("resolved").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if packages.iter().any(|existing: &ResolvedPackage| {
                existing.key.coordinate.name == PackageCoordinate::new(Ecosystem::Nuget, name).name
                    && existing.key.version == version
            }) {
                continue;
            }
            let dependency_names = entry
                .get("dependencies")
                .and_then(serde_json::Value::as_object)
                .map(|table| table.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            packages.push(package(Ecosystem::Nuget, name, version));
            named.push((name.clone(), dependency_names));
        }
    }
    let index = packages
        .iter()
        .map(|entry| (entry.key.coordinate.name.clone(), entry.key.clone()))
        .collect();
    link(&mut packages, &named, &index, Ecosystem::Nuget);
    Ok(packages)
}

// ------------------------------------------------- formats without edges

fn parse_go_mod(source: &str) -> Vec<ResolvedPackage> {
    let mut packages = Vec::new();
    let mut in_block = false;
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("require (") {
            in_block = true;
            continue;
        }
        if in_block && trimmed == ")" {
            in_block = false;
            continue;
        }
        let entry = if in_block {
            trimmed
        } else if let Some(rest) = trimmed.strip_prefix("require ") {
            rest.trim()
        } else {
            continue;
        };
        let entry = entry.split("//").next().unwrap_or(entry).trim();
        let mut parts = entry.split_whitespace();
        let (Some(name), Some(version)) = (parts.next(), parts.next()) else {
            continue;
        };
        if name.is_empty() || !version.starts_with('v') {
            continue;
        }
        // Go versions carry a leading `v` that OSV ranges do not.
        packages.push(package(
            Ecosystem::Go,
            name,
            version.trim_start_matches('v'),
        ));
    }
    packages
}

fn parse_swift_resolved(source: &str) -> Result<Vec<ResolvedPackage>, LockGraphError> {
    let document: serde_json::Value =
        serde_json::from_str(source).map_err(|error| LockGraphError::Format(error.to_string()))?;
    // v1 nests pins under `object`; v2 and v3 hoist them to the top level.
    let pins = document
        .get("pins")
        .or_else(|| document.get("object").and_then(|object| object.get("pins")))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();

    Ok(pins
        .iter()
        .filter_map(|pin| {
            let name = pin
                .get("identity")
                .or_else(|| pin.get("package"))
                .and_then(serde_json::Value::as_str)?;
            let version = pin
                .get("state")
                .and_then(|state| state.get("version"))
                .and_then(serde_json::Value::as_str)?;
            Some(package(Ecosystem::Swift, name, version))
        })
        .collect())
}

fn parse_pubspec_lock(source: &str) -> Result<Vec<ResolvedPackage>, LockGraphError> {
    let document: serde_json::Value = serde_norway::from_str(source)
        .map_err(|error| LockGraphError::Format(error.to_string()))?;
    let entries = document
        .get("packages")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();

    Ok(entries
        .iter()
        .filter_map(|(name, entry)| {
            let version = entry.get("version").and_then(serde_json::Value::as_str)?;
            Some(package(Ecosystem::Pub, name, version))
        })
        .collect())
}

fn parse_mix_lock(source: &str) -> Vec<ResolvedPackage> {
    let mut packages = Vec::new();
    // `"jason": {:hex, :jason, "1.4.0", ...}` -- the third field is the version.
    for line in source.lines() {
        let trimmed = line.trim();
        let Some((raw_name, rest)) = trimmed.split_once(':') else {
            continue;
        };
        let name = raw_name.trim().trim_matches('"');
        if name.is_empty() || !rest.trim_start().starts_with('{') {
            continue;
        }
        let quoted = rest
            .split('"')
            .nth(1)
            .filter(|value| value.chars().next().is_some_and(|c| c.is_ascii_digit()));
        if let Some(version) = quoted {
            packages.push(package(Ecosystem::Hex, name, version));
        }
    }
    packages
}

fn parse_podfile_lock(source: &str) -> Result<Vec<ResolvedPackage>, LockGraphError> {
    let mut packages = Vec::new();
    let mut named: Vec<(String, Vec<String>)> = Vec::new();
    let mut in_pods = false;

    for line in source.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            continue;
        }
        if !trimmed.starts_with(' ') {
            in_pods = trimmed.starts_with("PODS:");
            continue;
        }
        if !in_pods {
            continue;
        }
        let indent = trimmed.len() - trimmed.trim_start().len();
        let content = trimmed.trim().trim_start_matches("- ").trim();
        let content = content.trim_end_matches(':').trim_matches('"');
        let name = content.split(" (").next().unwrap_or_default().trim();
        if name.is_empty() {
            continue;
        }
        if indent <= 2 {
            let version = content
                .split_once(" (")
                .map(|(_, rest)| rest.trim_end_matches(')').to_string())
                .unwrap_or_default();
            if version.is_empty() {
                continue;
            }
            packages.push(package(Ecosystem::Cocoapods, name, &version));
            named.push((name.to_string(), Vec::new()));
        } else if let Some(last) = named.last_mut() {
            last.1.push(name.to_string());
        }
    }
    let index = packages
        .iter()
        .map(|entry| (entry.key.coordinate.name.clone(), entry.key.clone()))
        .collect();
    link(&mut packages, &named, &index, Ecosystem::Cocoapods);
    Ok(packages)
}

fn parse_gradle_lockfile(source: &str) -> Vec<ResolvedPackage> {
    source
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("empty=") {
                return None;
            }
            // group:artifact:version=configurations
            let coordinate = trimmed.split('=').next().unwrap_or(trimmed);
            let mut parts = coordinate.split(':');
            let (group, artifact, version) = (parts.next()?, parts.next()?, parts.next()?);
            if group.is_empty() || artifact.is_empty() || version.is_empty() {
                return None;
            }
            Some(package(
                Ecosystem::Maven,
                &format!("{group}:{artifact}"),
                version,
            ))
        })
        .collect()
}
