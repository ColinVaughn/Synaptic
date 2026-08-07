use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use synaptic_api::{Ecosystem, PackageCoordinate};

/// Identity of one resolved package instance. A lockfile can contain several
/// versions of the same package, so the version is part of the key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PackageKey {
    pub coordinate: PackageCoordinate,
    pub version: String,
}

impl PackageKey {
    pub fn new(coordinate: PackageCoordinate, version: impl Into<String>) -> Self {
        Self {
            coordinate,
            version: version.into(),
        }
    }
}

impl std::fmt::Display for PackageKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.coordinate, self.version)
    }
}

/// What a lockfile says a package is needed for.
///
/// `Unknown` is not a synonym for `Runtime`. Most lockfile formats record no
/// dependency kind at all, and a scanner that turned that silence into
/// "runtime" would be reporting a reading it never made. The two are kept apart
/// so a finding can say which one it is standing on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageScope {
    /// The lockfile does not record what this package is needed for.
    #[default]
    Unknown,
    /// The lockfile records this package as needed at runtime.
    Runtime,
    /// The lockfile records this package as needed only for development,
    /// testing, or the build.
    Development,
}

/// One node of the resolved dependency graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPackage {
    pub key: PackageKey,
    /// Packages this one depends on, as resolved lockfile identities.
    pub dependencies: Vec<PackageKey>,
    /// True when the package is part of this workspace rather than fetched
    /// from a registry. These are the roots of every dependency path.
    pub is_workspace_member: bool,
    /// What the lockfile says this package is needed for, where it says so.
    #[serde(default)]
    pub scope: PackageScope,
}

/// The full resolved dependency graph read from a lockfile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageGraph {
    packages: BTreeMap<PackageKey, ResolvedPackage>,
}

impl PackageGraph {
    /// Parse a `Cargo.lock` into a resolved graph.
    ///
    /// Packages with no `source` are workspace members: Cargo omits the field
    /// for path dependencies and for the workspace's own crates, which makes it
    /// the reliable first-party marker.
    pub fn from_cargo_lock(source: &str) -> Result<Self, LockGraphError> {
        Ok(Self::from_packages(parse_cargo_lock(source)?))
    }

    /// Build a graph from already-parsed packages, keyed for lookup.
    pub fn from_packages(packages: Vec<ResolvedPackage>) -> Self {
        Self {
            packages: packages
                .into_iter()
                .map(|package| (package.key.clone(), package))
                .collect(),
        }
    }

    /// Merge another ecosystem's packages into this graph.
    ///
    /// Coordinates carry their ecosystem, so packages from different ecosystems
    /// never collide even when they share a name.
    pub fn absorb(&mut self, packages: Vec<ResolvedPackage>) {
        for package in packages {
            self.packages.entry(package.key.clone()).or_insert(package);
        }
    }

    /// Discover and read every lockfile in a repository.
    ///
    /// Unreadable or malformed lockfiles are reported rather than aborting the
    /// walk: one broken manifest in a polyglot repository must not blind the
    /// scan to every other ecosystem in it.
    pub fn from_repository(root: &Path) -> (Self, Vec<LockfileRead>) {
        Self::from_lockfiles(root, &discover_repository_files(root).lockfiles)
    }

    /// Read an already-discovered set of lockfiles.
    ///
    /// Split out so a caller that also needs the manifests can walk the
    /// repository once and feed both, rather than traversing it twice.
    pub fn from_lockfiles(root: &Path, lockfiles: &[PathBuf]) -> (Self, Vec<LockfileRead>) {
        let mut graph = Self::default();
        let mut reads = Vec::new();

        for path in lockfiles {
            let Some(kind) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(crate::lockfiles::LockfileKind::for_file_name)
            else {
                continue;
            };
            let relative = path.strip_prefix(root).unwrap_or(path).to_path_buf();
            match std::fs::read_to_string(path) {
                Ok(source) => match crate::lockfiles::parse(kind, &source) {
                    Ok(packages) => {
                        reads.push(LockfileRead {
                            path: relative,
                            kind,
                            packages: packages.len(),
                            error: None,
                        });
                        graph.absorb(packages);
                    }
                    Err(error) => reads.push(LockfileRead {
                        path: relative,
                        kind,
                        packages: 0,
                        error: Some(error.to_string()),
                    }),
                },
                Err(error) => reads.push(LockfileRead {
                    path: relative,
                    kind,
                    packages: 0,
                    error: Some(error.to_string()),
                }),
            }
        }
        (graph, reads)
    }

    fn parse_cargo_lock_inner(source: &str) -> Result<Vec<ResolvedPackage>, LockGraphError> {
        let document: CargoLock = toml::from_str(source)?;

        // Index by name first so dependency references that carry no version
        // can be resolved, then again by (name, version) for the ones that do.
        let mut by_name: BTreeMap<String, Vec<PackageKey>> = BTreeMap::new();
        let mut entries = Vec::new();
        for entry in document.package {
            let name = entry.name;
            let Some(version) = entry.version else {
                return Err(LockGraphError::MalformedPackage(name, "version"));
            };
            let key = PackageKey::new(PackageCoordinate::new(Ecosystem::Cargo, &name), &version);
            by_name.entry(name).or_default().push(key.clone());
            entries.push((key, entry.dependencies, entry.source.is_none()));
        }

        let mut packages = Vec::new();
        for (key, raw_dependencies, is_workspace_member) in entries {
            let dependencies = raw_dependencies
                .iter()
                .filter_map(|reference| resolve_reference(reference, &by_name))
                .collect();
            packages.push(ResolvedPackage {
                key,
                dependencies,
                is_workspace_member,
                // Cargo.lock records no dependency kind. What is known about
                // scope for Cargo comes from the manifests, and is applied by
                // the scanner.
                scope: PackageScope::Unknown,
            });
        }
        Ok(packages)
    }

    /// Every resolved package, in stable order.
    pub fn packages(&self) -> impl Iterator<Item = &ResolvedPackage> {
        self.packages.values()
    }

    /// Every package some runtime path reaches.
    ///
    /// `development` names coordinates a manifest declared development-only,
    /// which is where the answer comes from for formats whose lockfile records
    /// no dependency kind of its own. Cargo is the case that matters: the
    /// lockfile has edges but no scope, and the manifest has scope but no
    /// edges, so neither alone can tell a test-only tree from a shipped one.
    ///
    /// The traversal is deliberately generous, because the cost of the two
    /// errors is not symmetric. Under-reporting reachability quietly de-ranks a
    /// real vulnerability; over-reporting it merely leaves a finding at the
    /// priority it already had. So a package is treated as a runtime root
    /// whenever nothing points at it, which is every package in the formats
    /// that record no edges at all, and a package reached by any runtime path
    /// stays reachable however many development paths also reach it.
    pub fn runtime_reachable_keys(
        &self,
        development: &BTreeSet<PackageCoordinate>,
    ) -> BTreeSet<PackageKey> {
        let is_development = |package: &ResolvedPackage| {
            package.scope == PackageScope::Development
                || development.contains(&package.key.coordinate)
        };

        let mut has_incoming: BTreeSet<&PackageKey> = BTreeSet::new();
        for package in self.packages.values() {
            for dependency in &package.dependencies {
                has_incoming.insert(dependency);
            }
        }

        let mut reached: BTreeSet<PackageKey> = BTreeSet::new();
        let mut queue: VecDeque<&PackageKey> = self
            .packages
            .values()
            .filter(|package| !is_development(package))
            .filter(|package| package.is_workspace_member || !has_incoming.contains(&package.key))
            .map(|package| &package.key)
            .collect();
        for key in &queue {
            reached.insert((*key).clone());
        }

        while let Some(key) = queue.pop_front() {
            let Some(package) = self.packages.get(key) else {
                continue;
            };
            for dependency in &package.dependencies {
                let Some(next) = self.packages.get(dependency) else {
                    continue;
                };
                if is_development(next) || reached.contains(dependency) {
                    continue;
                }
                reached.insert(dependency.clone());
                queue.push_back(&next.key);
            }
        }
        reached
    }

    /// Workspace-member packages, which are the roots of dependency paths.
    pub fn roots(&self) -> impl Iterator<Item = &ResolvedPackage> {
        self.packages
            .values()
            .filter(|package| package.is_workspace_member)
    }

    /// Where dependency-path searches start.
    ///
    /// Cargo and npm name the project inside the lockfile, so its own packages
    /// are the roots. Most other formats list only third-party packages, so the
    /// top-level ones -- those nothing else depends on -- are used instead. That
    /// keeps paths meaningful without inventing a project node the lockfile
    /// never mentioned.
    fn path_roots(&self) -> Vec<PackageKey> {
        let members = self
            .packages
            .values()
            .filter(|package| package.is_workspace_member)
            .map(|package| package.key.clone())
            .collect::<Vec<_>>();
        if !members.is_empty() {
            return members;
        }
        let depended_on = self
            .packages
            .values()
            .flat_map(|package| package.dependencies.iter().cloned())
            .collect::<BTreeSet<_>>();
        self.packages
            .keys()
            .filter(|key| !depended_on.contains(*key))
            .cloned()
            .collect()
    }

    /// Packages a scan should test: everything that is not the project itself.
    pub fn scan_targets(&self) -> impl Iterator<Item = &ResolvedPackage> {
        self.packages
            .values()
            .filter(|package| !package.is_workspace_member)
    }

    /// All resolved instances of a package name, across versions.
    pub fn instances_of(&self, coordinate: &PackageCoordinate) -> Vec<&ResolvedPackage> {
        self.packages
            .values()
            .filter(|package| &package.key.coordinate == coordinate)
            .collect()
    }

    /// The shortest dependency path from any workspace member to `target`.
    ///
    /// Returned paths start at a workspace member and end at the target. A
    /// workspace member is its own path. `None` means the target is not
    /// reachable from any root, which for a lockfile normally means it is not
    /// really in the build.
    pub fn shortest_path_from_root(&self, target: &PackageKey) -> Option<Vec<PackageKey>> {
        if !self.packages.contains_key(target) {
            return None;
        }

        // Breadth-first from every root at once, so the first arrival at the
        // target is the globally shortest path rather than the shortest from
        // whichever root happened to be visited first.
        let mut previous: BTreeMap<PackageKey, PackageKey> = BTreeMap::new();
        let mut visited: BTreeSet<PackageKey> = BTreeSet::new();
        let mut queue: VecDeque<PackageKey> = VecDeque::new();
        for root in self.path_roots() {
            visited.insert(root.clone());
            queue.push_back(root);
        }

        while let Some(current) = queue.pop_front() {
            if &current == target {
                return Some(reconstruct_path(&previous, current));
            }
            let Some(package) = self.packages.get(&current) else {
                continue;
            };
            for dependency in &package.dependencies {
                if visited.insert(dependency.clone()) {
                    previous.insert(dependency.clone(), current.clone());
                    queue.push_back(dependency.clone());
                }
            }
        }
        None
    }
}

fn reconstruct_path(
    previous: &BTreeMap<PackageKey, PackageKey>,
    target: PackageKey,
) -> Vec<PackageKey> {
    let mut path = vec![target];
    while let Some(parent) = previous.get(path.last().expect("path is never empty")) {
        path.push(parent.clone());
    }
    path.reverse();
    path
}

/// Resolve one `dependencies` entry.
///
/// Cargo writes these as `name`, `name version`, or
/// `name version (source)` depending on lockfile version and on whether the
/// name alone is ambiguous.
fn resolve_reference(
    reference: &str,
    by_name: &BTreeMap<String, Vec<PackageKey>>,
) -> Option<PackageKey> {
    let mut parts = reference.split_whitespace();
    let name = parts.next()?;
    let version = parts.next();
    let candidates = by_name.get(name)?;
    match version {
        Some(version) => candidates
            .iter()
            .find(|candidate| candidate.version == version)
            .cloned(),
        // Cargo only omits the version when the name resolves uniquely.
        None => candidates.first().cloned(),
    }
}

/// Parse a `Cargo.lock` into resolved packages.
pub(crate) fn parse_cargo_lock(source: &str) -> Result<Vec<ResolvedPackage>, LockGraphError> {
    PackageGraph::parse_cargo_lock_inner(source)
}

/// Record of one lockfile the repository walk found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockfileRead {
    pub path: PathBuf,
    pub kind: crate::lockfiles::LockfileKind,
    pub packages: usize,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
}

/// Directories that never contain a repository's own lockfiles, only vendored
/// copies of other people's. Walking them produces noise and can be enormous.
const SKIPPED_DIRECTORIES: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "vendor",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".tox",
    ".gradle",
    "Pods",
    "bower_components",
    ".next",
    ".nuxt",
];

const MAX_LOCKFILE_DEPTH: usize = 6;

/// How this crate walks a repository, for every purpose.
///
/// Shared rather than configured per caller, because the settings are not
/// arbitrary: skipping vendored and generated trees is what keeps a scan from
/// reporting other projects' dependencies, and it is also most of the runtime.
/// A second walker configured by hand drifted to 62 ms against this one's
/// 11 ms over the same repository, purely by descending where this does not.
pub(crate) fn repository_walker(root: &Path) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .max_depth(Some(MAX_LOCKFILE_DEPTH))
        .hidden(true)
        .parents(false)
        // Apply ignore files even outside a git repository, so a plain
        // directory with a .gitignore behaves the same way.
        .require_git(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .filter_entry(|entry| {
            entry
                .file_name()
                .to_str()
                .is_none_or(|name| !SKIPPED_DIRECTORIES.contains(&name))
        });
    builder
}

/// Find every lockfile that belongs to this repository.
///
/// `.gitignore` is honoured, because generated and vendored trees carry other
/// projects' lockfiles: auditing them reports dependencies the repository does
/// not actually have, and on a large vendored tree it dominates the runtime.
/// The explicit skip list still applies, so the walk is sane in a directory
/// that is not a git repository at all.
/// Everything a scan needs to find on disk, from a single traversal.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RepositoryFiles {
    /// Lockfiles in any supported format.
    pub lockfiles: Vec<PathBuf>,
    /// `Cargo.toml` manifests, which carry the feature declarations a lockfile
    /// does not record.
    pub cargo_manifests: Vec<PathBuf>,
}

/// Find every lockfile and Cargo manifest that belongs to this repository.
///
/// Both in one pass. The lockfile graph and the feature resolver each need a
/// different subset of the same walk, and traversing twice cost the walk twice
/// for no benefit.
pub fn discover_repository_files(root: &Path) -> RepositoryFiles {
    let mut files = RepositoryFiles::default();
    for entry in repository_walker(root).build().flatten() {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if crate::lockfiles::LockfileKind::for_file_name(name).is_some() {
            files.lockfiles.push(path.to_path_buf());
        } else if name == "Cargo.toml" {
            files.cargo_manifests.push(path.to_path_buf());
        }
    }
    // Sorted so a scan reports the same thing regardless of directory order.
    files.lockfiles.sort();
    files.cargo_manifests.sort();
    files
}

#[derive(Debug, Deserialize)]
struct CargoLock {
    #[serde(default)]
    package: Vec<CargoLockPackage>,
}

#[derive(Debug, Deserialize)]
struct CargoLockPackage {
    name: String,
    version: Option<String>,
    source: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum LockGraphError {
    #[error("lockfile is not valid TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("lockfile package entry {0} is missing a {1} field")]
    MalformedPackage(String, &'static str),
    #[error("lockfile could not be parsed: {0}")]
    Format(String),
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cargo(name: &str, version: &str) -> PackageKey {
        PackageKey::new(PackageCoordinate::new(Ecosystem::Cargo, name), version)
    }

    const WORKSPACE_LOCK: &str = r#"
version = 4

[[package]]
name = "app"
version = "0.9.0"
dependencies = [
 "middle",
]

[[package]]
name = "middle"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaa"
dependencies = [
 "leaf",
]

[[package]]
name = "leaf"
version = "0.9.18"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "bbb"
"#;

    const DEV_LOCK: &str = r#"
version = 4

[[package]]
name = "app"
version = "0.9.0"
dependencies = [
 "middle",
 "harness",
]

[[package]]
name = "middle"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaa"
dependencies = [
 "leaf",
]

[[package]]
name = "harness"
version = "2.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "ccc"
dependencies = [
 "harness-macros",
]

[[package]]
name = "harness-macros"
version = "2.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "ddd"

[[package]]
name = "leaf"
version = "0.9.18"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "bbb"
"#;

    #[test]
    fn a_crate_reached_only_through_a_dev_dependency_is_not_runtime_reachable() {
        // Cargo.lock records no dependency kind, but Cargo.toml does. Given the
        // declared dev dependency, the lockfile's own edges say what else is
        // reached only through it: this is what closes the gap for Rust, where
        // proc-macro test harnesses drag in large trees nothing ships.
        let graph = PackageGraph::from_cargo_lock(DEV_LOCK).unwrap();
        let development = [PackageCoordinate::new(Ecosystem::Cargo, "harness")]
            .into_iter()
            .collect();

        let reachable = graph.runtime_reachable_keys(&development);

        assert!(reachable.contains(&cargo("middle", "1.0.0")));
        assert!(reachable.contains(&cargo("leaf", "0.9.18")));
        assert!(
            !reachable.contains(&cargo("harness", "2.0.0")),
            "the declared dev dependency itself"
        );
        assert!(
            !reachable.contains(&cargo("harness-macros", "2.0.0")),
            "reached only through the dev dependency"
        );
    }

    #[test]
    fn a_crate_reached_both_ways_stays_runtime_reachable() {
        // `leaf` is pulled in by a dev dependency and by a runtime one. Being
        // used by a test harness does not stop it shipping.
        const SHARED: &str = r#"
version = 4

[[package]]
name = "app"
version = "0.9.0"
dependencies = ["middle", "harness"]

[[package]]
name = "middle"
version = "1.0.0"
source = "registry+x"
checksum = "aaa"
dependencies = ["leaf"]

[[package]]
name = "harness"
version = "2.0.0"
source = "registry+x"
checksum = "ccc"
dependencies = ["leaf"]

[[package]]
name = "leaf"
version = "0.9.18"
source = "registry+x"
checksum = "bbb"
"#;
        let graph = PackageGraph::from_cargo_lock(SHARED).unwrap();
        let development = [PackageCoordinate::new(Ecosystem::Cargo, "harness")]
            .into_iter()
            .collect();

        let reachable = graph.runtime_reachable_keys(&development);

        assert!(reachable.contains(&cargo("leaf", "0.9.18")));
    }

    #[test]
    fn a_package_the_lockfile_marked_development_is_not_runtime_reachable() {
        let packages = vec![
            ResolvedPackage {
                key: cargo("app", "1.0.0"),
                dependencies: vec![],
                is_workspace_member: true,
                scope: PackageScope::Runtime,
            },
            ResolvedPackage {
                key: cargo("jest", "29.0.0"),
                dependencies: vec![],
                is_workspace_member: false,
                scope: PackageScope::Development,
            },
        ];
        let graph = PackageGraph::from_packages(packages);

        let reachable = graph.runtime_reachable_keys(&Default::default());

        assert!(!reachable.contains(&cargo("jest", "29.0.0")));
    }

    #[test]
    fn a_package_from_a_format_without_edges_is_assumed_runtime_reachable() {
        // Nothing points at it and its format records no scope, so there is no
        // evidence either way. Assuming it unreachable would quietly de-rank
        // real findings across every edgeless ecosystem.
        let packages = vec![ResolvedPackage {
            key: cargo("orphan", "1.0.0"),
            dependencies: vec![],
            is_workspace_member: false,
            scope: PackageScope::Unknown,
        }];
        let graph = PackageGraph::from_packages(packages);

        let reachable = graph.runtime_reachable_keys(&Default::default());

        assert!(reachable.contains(&cargo("orphan", "1.0.0")));
    }

    #[test]
    fn parses_every_package_with_its_resolved_version() {
        let graph = PackageGraph::from_cargo_lock(WORKSPACE_LOCK).unwrap();

        let keys = graph
            .packages()
            .map(|package| package.key.to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            keys,
            vec![
                "cargo:app@0.9.0".to_string(),
                "cargo:leaf@0.9.18".to_string(),
                "cargo:middle@1.0.0".to_string(),
            ]
        );
    }

    #[test]
    fn packages_without_a_source_are_workspace_members() {
        let graph = PackageGraph::from_cargo_lock(WORKSPACE_LOCK).unwrap();

        let roots = graph
            .roots()
            .map(|package| package.key.to_string())
            .collect::<Vec<_>>();

        assert_eq!(roots, vec!["cargo:app@0.9.0".to_string()]);
    }

    #[test]
    fn resolves_dependency_references_given_only_a_name() {
        let graph = PackageGraph::from_cargo_lock(WORKSPACE_LOCK).unwrap();

        let app = graph.instances_of(&PackageCoordinate::new(Ecosystem::Cargo, "app"))[0];

        assert_eq!(app.dependencies, vec![cargo("middle", "1.0.0")]);
    }

    #[test]
    fn resolves_name_and_version_references_when_versions_coexist() {
        let source = r#"
version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "dup 1.0.0",
 "dup 2.0.0",
]

[[package]]
name = "dup"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "dup"
version = "2.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;

        let graph = PackageGraph::from_cargo_lock(source).unwrap();
        let app = graph.instances_of(&PackageCoordinate::new(Ecosystem::Cargo, "app"))[0];

        assert_eq!(
            app.dependencies,
            vec![cargo("dup", "1.0.0"), cargo("dup", "2.0.0")]
        );
    }

    #[test]
    fn resolves_references_that_carry_a_source_suffix() {
        let source = r#"
version = 3

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "leaf 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)",
]

[[package]]
name = "leaf"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;

        let graph = PackageGraph::from_cargo_lock(source).unwrap();
        let app = graph.instances_of(&PackageCoordinate::new(Ecosystem::Cargo, "app"))[0];

        assert_eq!(app.dependencies, vec![cargo("leaf", "1.0.0")]);
    }

    #[test]
    fn finds_the_transitive_path_from_a_workspace_member_to_a_leaf() {
        let graph = PackageGraph::from_cargo_lock(WORKSPACE_LOCK).unwrap();

        let path = graph
            .shortest_path_from_root(&cargo("leaf", "0.9.18"))
            .expect("leaf is reachable from app");

        assert_eq!(
            path.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec![
                "cargo:app@0.9.0".to_string(),
                "cargo:middle@1.0.0".to_string(),
                "cargo:leaf@0.9.18".to_string(),
            ]
        );
    }

    #[test]
    fn a_workspace_member_is_its_own_dependency_path() {
        let graph = PackageGraph::from_cargo_lock(WORKSPACE_LOCK).unwrap();

        let path = graph
            .shortest_path_from_root(&cargo("app", "0.9.0"))
            .unwrap();

        assert_eq!(path, vec![cargo("app", "0.9.0")]);
    }

    #[test]
    fn picks_the_shortest_path_when_several_exist() {
        let source = r#"
version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "detour",
 "target",
]

[[package]]
name = "detour"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = [
 "target",
]

[[package]]
name = "target"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;

        let graph = PackageGraph::from_cargo_lock(source).unwrap();

        let path = graph
            .shortest_path_from_root(&cargo("target", "1.0.0"))
            .unwrap();

        assert_eq!(path.len(), 2, "direct edge beats the path through detour");
    }

    #[test]
    fn a_package_unreachable_from_any_root_has_no_path() {
        let source = r#"
version = 4

[[package]]
name = "app"
version = "0.1.0"

[[package]]
name = "orphan"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;

        let graph = PackageGraph::from_cargo_lock(source).unwrap();

        assert_eq!(
            graph.shortest_path_from_root(&cargo("orphan", "1.0.0")),
            None
        );
    }

    #[test]
    fn a_dependency_cycle_does_not_hang_the_search() {
        let source = r#"
version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = ["a"]

[[package]]
name = "a"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = ["b"]

[[package]]
name = "b"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = ["a"]
"#;

        let graph = PackageGraph::from_cargo_lock(source).unwrap();

        let path = graph.shortest_path_from_root(&cargo("b", "1.0.0")).unwrap();

        assert_eq!(path.len(), 3);
    }

    #[test]
    fn a_dependency_reference_with_no_matching_package_is_dropped_not_fatal() {
        // Cargo will not emit this, but hand-edited and truncated lockfiles
        // exist, and a scan that dies on one is worse than one that reports
        // what it could resolve.
        let source = r#"
version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = ["missing"]
"#;

        let graph = PackageGraph::from_cargo_lock(source).unwrap();
        let app = graph.instances_of(&PackageCoordinate::new(Ecosystem::Cargo, "app"))[0];

        assert!(app.dependencies.is_empty());
    }

    #[test]
    fn discovery_skips_lockfiles_in_ignored_directories() {
        // Generated and vendored trees carry other projects' lockfiles. Auditing
        // them is wrong (they are not this repository's dependencies) and slow:
        // on Synaptic itself, descending into `synaptic-out/` picked up 24
        // vendored lockfiles including a Sourcegraph `package-lock.json`, and
        // turned a one-second scan into a ten-minute one.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "generated/\n").unwrap();
        std::fs::write(root.join("Cargo.lock"), "version = 4\n").unwrap();

        let vendored = root.join("generated").join("someone-else");
        std::fs::create_dir_all(&vendored).unwrap();
        std::fs::write(vendored.join("Cargo.lock"), "version = 4\n").unwrap();

        let node_modules = root.join("node_modules").join("pkg");
        std::fs::create_dir_all(&node_modules).unwrap();
        std::fs::write(node_modules.join("package-lock.json"), "{}").unwrap();

        let (_graph, reads) = PackageGraph::from_repository(root);
        let found = reads
            .iter()
            .map(|read| read.path.to_string_lossy().replace('\\', "/"))
            .collect::<Vec<_>>();

        assert_eq!(
            found,
            vec!["Cargo.lock".to_string()],
            "only the repository's own lockfile should be audited"
        );
    }

    #[test]
    fn a_package_entry_without_a_version_is_rejected() {
        let source = r#"
version = 4

[[package]]
name = "app"
"#;

        let error = PackageGraph::from_cargo_lock(source).unwrap_err();

        assert!(matches!(
            error,
            LockGraphError::MalformedPackage(_, "version")
        ));
    }
}
