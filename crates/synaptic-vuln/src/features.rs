//! Which optional Cargo dependencies a default build actually compiles.
//!
//! A dependency behind a disabled feature is still in `Cargo.lock`, because the
//! lockfile records what *could* be resolved rather than what gets built. A
//! scanner that stops at the lockfile therefore reports vulnerabilities in code
//! this repository never compiles.
//!
//! The resolution here is deliberately shallow: it reads the repository's own
//! manifests and decides only about the dependencies they declare directly.
//! Following features through the manifests of registry crates would need the
//! registry index, which this crate has no offline access to, so that stays an
//! open limitation rather than a half-implemented guess.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use synaptic_api::{Ecosystem, PackageCoordinate};

/// What one manifest says about its own optional dependencies.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ManifestFeatures {
    /// Dependencies a default build compiles.
    pub enabled: BTreeSet<String>,
    /// Optional dependencies no enabled feature turns on.
    pub gated: BTreeSet<String>,
}

/// Read one `Cargo.toml` and decide which of its optional dependencies a
/// default build leaves out.
pub fn manifest_features(manifest: &str) -> ManifestFeatures {
    let Ok(document) = toml::from_str::<toml::Value>(manifest) else {
        return ManifestFeatures::default();
    };

    let mut declared: BTreeMap<String, bool> = BTreeMap::new();
    for table in dependency_tables(&document) {
        for (name, spec) in table {
            let optional = spec
                .get("optional")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false);
            // A dependency required by any table is required, whatever another
            // table says: being optional for one target does not make the code
            // unreachable when another target compiles it unconditionally.
            let entry = declared.entry(name.clone()).or_insert(optional);
            *entry = *entry && optional;
        }
    }

    let features = document
        .get("features")
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    let active = active_features(&features);

    // An optional dependency is compiled when some active feature names it,
    // either explicitly as `dep:name`, through `name/feature`, or by the
    // implicit feature Cargo derives from the dependency itself.
    let mut activated: BTreeSet<String> = BTreeSet::new();
    for name in &active {
        if let Some(values) = features.get(name).and_then(toml::Value::as_array) {
            for value in values.iter().filter_map(toml::Value::as_str) {
                if let Some(dependency) = value.strip_prefix("dep:") {
                    activated.insert(dependency.to_string());
                } else if let Some((dependency, _)) = value.split_once('/') {
                    activated.insert(dependency.trim_end_matches('?').to_string());
                }
            }
        }
        // `default = ["serde"]` where `serde` is an optional dependency and no
        // `[features]` entry shadows it: Cargo's implicit feature.
        if !features.contains_key(name) {
            activated.insert(name.clone());
        }
    }

    let mut result = ManifestFeatures::default();
    for (name, optional) in declared {
        if !optional || activated.contains(&name) {
            result.enabled.insert(name);
        } else {
            result.gated.insert(name);
        }
    }
    result
}

/// Every dependency table in a manifest, including target-specific ones.
fn dependency_tables(document: &toml::Value) -> Vec<toml::map::Map<String, toml::Value>> {
    const SECTIONS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];
    let mut tables = Vec::new();
    for section in SECTIONS {
        if let Some(table) = document.get(section).and_then(toml::Value::as_table) {
            tables.push(table.clone());
        }
    }
    if let Some(targets) = document.get("target").and_then(toml::Value::as_table) {
        for target in targets.values() {
            for section in SECTIONS {
                if let Some(table) = target.get(section).and_then(toml::Value::as_table) {
                    tables.push(table.clone());
                }
            }
        }
    }
    tables
}

/// The feature closure a build enables when nothing is asked for explicitly.
fn active_features(features: &toml::map::Map<String, toml::Value>) -> BTreeSet<String> {
    let mut active = BTreeSet::new();
    let mut queue = vec!["default".to_string()];
    while let Some(name) = queue.pop() {
        if !active.insert(name.clone()) {
            continue;
        }
        let Some(values) = features.get(&name).and_then(toml::Value::as_array) else {
            continue;
        };
        for value in values.iter().filter_map(toml::Value::as_str) {
            // `dep:x` and `x/y` activate dependencies, not features of this
            // crate, so they do not extend the closure.
            if !value.starts_with("dep:") && !value.contains('/') {
                queue.push(value.to_string());
            }
        }
    }
    active
}

/// Optional dependencies no manifest in the repository compiles by default.
///
/// A dependency that any manifest requires unconditionally is never reported,
/// so a crate that is optional in one workspace member and required in another
/// counts as compiled.
pub fn feature_gated_dependencies(root: &Path) -> BTreeSet<PackageCoordinate> {
    feature_gated_in(&crate::lockgraph::discover_repository_files(root).cargo_manifests)
}

/// The same answer from an already-discovered set of manifests.
///
/// Split out so a caller that also reads the lockfiles walks the repository
/// once and feeds both, rather than traversing it twice.
pub fn feature_gated_in(manifests: &[std::path::PathBuf]) -> BTreeSet<PackageCoordinate> {
    let mut enabled = BTreeSet::new();
    let mut gated = BTreeSet::new();

    for manifest in manifests {
        let Ok(body) = std::fs::read_to_string(manifest) else {
            continue;
        };
        let features = manifest_features(&body);
        enabled.extend(features.enabled);
        gated.extend(features.gated);
    }

    gated
        .difference(&enabled)
        .map(|name| PackageCoordinate::new(Ecosystem::Cargo, name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_optional_dependency_no_feature_enables_is_gated() {
        let manifest = r#"
[package]
name = "app"

[dependencies]
serde = { version = "1", optional = true }
"#;

        let features = manifest_features(manifest);

        assert!(features.gated.contains("serde"));
        assert!(features.enabled.is_empty());
    }

    #[test]
    fn an_optional_dependency_a_default_feature_enables_is_compiled() {
        let manifest = r#"
[package]
name = "app"

[dependencies]
serde = { version = "1", optional = true }

[features]
default = ["json"]
json = ["dep:serde"]
"#;

        let features = manifest_features(manifest);

        assert!(features.enabled.contains("serde"));
        assert!(features.gated.is_empty());
    }

    #[test]
    fn a_default_feature_naming_the_dependency_directly_enables_it() {
        // Cargo derives an implicit feature from every optional dependency
        // unless a `[features]` entry shadows the name.
        let manifest = r#"
[package]
name = "app"

[dependencies]
serde = { version = "1", optional = true }

[features]
default = ["serde"]
"#;

        let features = manifest_features(manifest);

        assert!(features.enabled.contains("serde"));
    }

    #[test]
    fn the_slash_spelling_enables_the_dependency_too() {
        let manifest = r#"
[package]
name = "app"

[dependencies]
serde = { version = "1", optional = true }

[features]
default = ["json"]
json = ["serde/derive"]
"#;

        let features = manifest_features(manifest);

        assert!(features.enabled.contains("serde"));
    }

    #[test]
    fn a_feature_reachable_only_through_a_non_default_feature_does_not_enable_it() {
        let manifest = r#"
[package]
name = "app"

[dependencies]
serde = { version = "1", optional = true }

[features]
default = []
json = ["dep:serde"]
"#;

        let features = manifest_features(manifest);

        assert!(features.gated.contains("serde"));
    }

    #[test]
    fn a_required_dependency_is_never_gated() {
        let manifest = r#"
[package]
name = "app"

[dependencies]
serde = "1"
"#;

        let features = manifest_features(manifest);

        assert!(features.enabled.contains("serde"));
        assert!(features.gated.is_empty());
    }

    #[test]
    fn a_dependency_required_by_one_table_is_not_gated_by_another_making_it_optional() {
        let manifest = r#"
[package]
name = "app"

[dependencies]
serde = { version = "1", optional = true }

[dev-dependencies]
serde = "1"
"#;

        let features = manifest_features(manifest);

        assert!(
            features.enabled.contains("serde"),
            "it is compiled unconditionally somewhere"
        );
    }

    #[test]
    fn a_transitive_feature_chain_is_followed() {
        let manifest = r#"
[package]
name = "app"

[dependencies]
serde = { version = "1", optional = true }

[features]
default = ["outer"]
outer = ["inner"]
inner = ["dep:serde"]
"#;

        let features = manifest_features(manifest);

        assert!(features.enabled.contains("serde"));
    }

    #[test]
    fn a_manifest_that_does_not_parse_reports_nothing_rather_than_guessing() {
        let features = manifest_features("[dependencies\nbroken");

        assert_eq!(features, ManifestFeatures::default());
    }

    #[test]
    fn a_dependency_optional_in_one_manifest_and_required_in_another_is_not_gated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"a\"\n\n[dependencies]\nserde = { version = \"1\", optional = true }\n",
        )
        .unwrap();
        let member = dir.path().join("member");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"b\"\n\n[dependencies]\nserde = \"1\"\n",
        )
        .unwrap();

        let gated = feature_gated_dependencies(dir.path());

        assert!(
            !gated.contains(&PackageCoordinate::new(Ecosystem::Cargo, "serde")),
            "another member compiles it unconditionally"
        );
    }

    #[test]
    fn a_gated_dependency_is_reported_with_its_ecosystem() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"a\"\n\n[dependencies]\nfancy = { version = \"1\", optional = true }\n",
        )
        .unwrap();

        let gated = feature_gated_dependencies(dir.path());

        assert!(gated.contains(&PackageCoordinate::new(Ecosystem::Cargo, "fancy")));
    }
}
