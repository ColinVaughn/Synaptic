//! Every lockfile format this crate claims to support, exercised against a
//! realistic fixture.
//!
//! The coverage matrix at the bottom is the point: it fails if a format is
//! added to `LockfileKind` without a fixture proving it actually parses, so the
//! supported-ecosystem list in the documentation cannot drift away from what
//! the code really does.

use synaptic_api::{Ecosystem, PackageCoordinate};
use synaptic_vuln::{parse_lockfile, LockfileKind, PackageGraph, PackageScope};

/// One fixture per format: the source, and a package it must resolve.
struct Fixture {
    kind: LockfileKind,
    source: &'static str,
    expect_name: &'static str,
    expect_version: &'static str,
}

const CARGO: &str = r#"
version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = ["leaf"]

[[package]]
name = "leaf"
version = "0.9.18"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;

const NPM_V3: &str = r#"{
  "name": "app",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "packages": {
    "": { "name": "app", "version": "1.0.0", "dependencies": { "lodash": "^4.17.0" } },
    "node_modules/lodash": { "version": "4.17.20", "dependencies": { "tiny": "^1.0.0" } },
    "node_modules/tiny": { "version": "1.0.0" }
  }
}"#;

const NPM_V1: &str = r#"{
  "name": "app",
  "lockfileVersion": 1,
  "dependencies": {
    "lodash": { "version": "4.17.20", "requires": { "tiny": "^1.0.0" } },
    "tiny": { "version": "1.0.0" }
  }
}"#;

const PNPM: &str = r#"
lockfileVersion: '6.0'
packages:
  /lodash@4.17.20:
    resolution: {integrity: sha512-abc}
    dependencies:
      tiny: 1.0.0
  /tiny@1.0.0:
    resolution: {integrity: sha512-def}
"#;

const YARN: &str = r#"# yarn lockfile v1

lodash@^4.17.0, lodash@^4.0.0:
  version "4.17.20"
  resolved "https://registry.yarnpkg.com/lodash/-/lodash-4.17.20.tgz"
  dependencies:
    tiny "^1.0.0"

tiny@^1.0.0:
  version "1.0.0"
  resolved "https://registry.yarnpkg.com/tiny/-/tiny-1.0.0.tgz"
"#;

const POETRY: &str = r#"
[[package]]
name = "requests"
version = "2.31.0"
description = "HTTP for Humans"
optional = false

[package.dependencies]
urllib3 = ">=1.21.1,<3"

[[package]]
name = "urllib3"
version = "2.0.7"
optional = false
"#;

const UV: &str = r#"
version = 1

[[package]]
name = "requests"
version = "2.31.0"
dependencies = [{ name = "urllib3" }]

[[package]]
name = "urllib3"
version = "2.0.7"
"#;

const COMPOSER: &str = r#"{
  "packages": [
    {
      "name": "monolog/monolog",
      "version": "2.9.1",
      "require": { "php": ">=7.2", "psr/log": "^1.0" }
    },
    { "name": "psr/log", "version": "1.1.4" }
  ],
  "packages-dev": [
    { "name": "phpunit/phpunit", "version": "9.6.0" }
  ]
}"#;

const GEMFILE: &str = r#"GEM
  remote: https://rubygems.org/
  specs:
    rails (7.0.4)
      actionpack (= 7.0.4)
    actionpack (7.0.4)
      rack (>= 2.2.0)
    rack (2.2.6)

PLATFORMS
  ruby

DEPENDENCIES
  rails
"#;

const NUGET: &str = r#"{
  "version": 1,
  "dependencies": {
    "net6.0": {
      "Newtonsoft.Json": {
        "type": "Direct",
        "resolved": "13.0.1",
        "dependencies": { "System.Text.Json": "6.0.0" }
      },
      "System.Text.Json": { "type": "Transitive", "resolved": "6.0.0" }
    }
  }
}"#;

const GO_MOD: &str = r#"
module github.com/example/app

go 1.21

require (
	github.com/gin-gonic/gin v1.9.1
	golang.org/x/crypto v0.14.0 // indirect
)

require github.com/stretchr/testify v1.8.4
"#;

const SWIFT: &str = r#"{
  "pins": [
    {
      "identity": "swift-nio",
      "kind": "remoteSourceControl",
      "location": "https://github.com/apple/swift-nio.git",
      "state": { "revision": "abc", "version": "2.62.0" }
    }
  ],
  "version": 2
}"#;

const PUBSPEC: &str = r#"
packages:
  http:
    dependency: "direct main"
    description:
      name: http
    source: hosted
    version: "0.13.5"
  meta:
    dependency: transitive
    source: hosted
    version: "1.9.1"
sdks:
  dart: ">=2.18.0 <3.0.0"
"#;

const MIX: &str = r#"%{
  "jason": {:hex, :jason, "1.4.1", "hashhash", [:mix], [], "hexpm", "deadbeef"},
  "plug": {:hex, :plug, "1.14.2", "hashhash", [:mix], [{:mime, "~> 1.0", [hex: :mime]}], "hexpm", "cafe"},
}
"#;

const PODFILE: &str = r#"PODS:
  - Alamofire (5.8.0)
  - Kingfisher (7.9.1):
    - Alamofire

DEPENDENCIES:
  - Kingfisher

SPEC CHECKSUMS:
  Alamofire: abc123
"#;

const GRADLE: &str = r#"# This is a Gradle generated file for dependency locking.
com.google.guava:guava:32.1.2-jre=compileClasspath,runtimeClasspath
org.apache.commons:commons-lang3:3.13.0=runtimeClasspath
empty=annotationProcessor
"#;

fn fixtures() -> Vec<Fixture> {
    vec![
        Fixture {
            kind: LockfileKind::CargoLock,
            source: CARGO,
            expect_name: "leaf",
            expect_version: "0.9.18",
        },
        Fixture {
            kind: LockfileKind::NpmPackageLock,
            source: NPM_V3,
            expect_name: "lodash",
            expect_version: "4.17.20",
        },
        Fixture {
            kind: LockfileKind::PnpmLock,
            source: PNPM,
            expect_name: "lodash",
            expect_version: "4.17.20",
        },
        Fixture {
            kind: LockfileKind::YarnLock,
            source: YARN,
            expect_name: "lodash",
            expect_version: "4.17.20",
        },
        Fixture {
            kind: LockfileKind::PoetryLock,
            source: POETRY,
            expect_name: "requests",
            expect_version: "2.31.0",
        },
        Fixture {
            kind: LockfileKind::UvLock,
            source: UV,
            expect_name: "requests",
            expect_version: "2.31.0",
        },
        Fixture {
            kind: LockfileKind::ComposerLock,
            source: COMPOSER,
            expect_name: "monolog/monolog",
            expect_version: "2.9.1",
        },
        Fixture {
            kind: LockfileKind::GemfileLock,
            source: GEMFILE,
            expect_name: "rails",
            expect_version: "7.0.4",
        },
        Fixture {
            kind: LockfileKind::NuGetPackagesLock,
            source: NUGET,
            expect_name: "newtonsoft.json",
            expect_version: "13.0.1",
        },
        Fixture {
            kind: LockfileKind::GoMod,
            source: GO_MOD,
            expect_name: "github.com/gin-gonic/gin",
            expect_version: "1.9.1",
        },
        Fixture {
            kind: LockfileKind::SwiftPackageResolved,
            source: SWIFT,
            expect_name: "swift-nio",
            expect_version: "2.62.0",
        },
        Fixture {
            kind: LockfileKind::PubspecLock,
            source: PUBSPEC,
            expect_name: "http",
            expect_version: "0.13.5",
        },
        Fixture {
            kind: LockfileKind::MixLock,
            source: MIX,
            expect_name: "jason",
            expect_version: "1.4.1",
        },
        Fixture {
            kind: LockfileKind::PodfileLock,
            source: PODFILE,
            expect_name: "alamofire",
            expect_version: "5.8.0",
        },
        Fixture {
            kind: LockfileKind::GradleLockfile,
            source: GRADLE,
            expect_name: "com.google.guava:guava",
            expect_version: "32.1.2-jre",
        },
    ]
}

#[test]
fn every_supported_format_has_a_fixture() {
    let covered = fixtures()
        .iter()
        .map(|fixture| fixture.kind)
        .collect::<std::collections::BTreeSet<_>>();
    let missing = LockfileKind::all()
        .iter()
        .filter(|kind| !covered.contains(kind))
        .collect::<Vec<_>>();

    assert!(
        missing.is_empty(),
        "these formats are advertised but never proven to parse: {missing:?}"
    );
}

#[test]
fn every_format_resolves_its_expected_package() {
    for fixture in fixtures() {
        let packages = parse_lockfile(fixture.kind, fixture.source)
            .unwrap_or_else(|error| panic!("{:?} failed to parse: {error}", fixture.kind));
        let wanted = PackageCoordinate::new(fixture.kind.ecosystem(), fixture.expect_name);

        let found = packages.iter().find(|package| {
            package.key.coordinate == wanted && package.key.version == fixture.expect_version
        });
        assert!(
            found.is_some(),
            "{:?} did not resolve {}@{}; got {:?}",
            fixture.kind,
            fixture.expect_name,
            fixture.expect_version,
            packages
                .iter()
                .map(|package| package.key.to_string())
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn formats_that_claim_dependency_edges_actually_produce_them() {
    for fixture in fixtures() {
        if !fixture.kind.provides_dependency_edges() {
            continue;
        }
        let packages = parse_lockfile(fixture.kind, fixture.source).unwrap();
        let edges: usize = packages.iter().map(|p| p.dependencies.len()).sum();
        assert!(
            edges > 0,
            "{:?} claims to provide dependency edges but produced none",
            fixture.kind
        );
    }
}

#[test]
fn a_dependency_path_is_reported_for_edge_bearing_formats() {
    for fixture in fixtures() {
        if !fixture.kind.provides_dependency_edges() {
            continue;
        }
        let packages = parse_lockfile(fixture.kind, fixture.source).unwrap();
        let graph = PackageGraph::from_packages(packages);
        let target = graph
            .packages()
            .find(|package| !package.dependencies.is_empty())
            .expect("an edge-bearing format has a package with dependencies")
            .dependencies[0]
            .clone();

        let path = graph.shortest_path_from_root(&target);
        assert!(
            path.as_ref().is_some_and(|path| path.len() >= 2),
            "{:?} produced no dependency path to {target}",
            fixture.kind
        );
    }
}

#[test]
fn go_versions_drop_the_leading_v_so_they_order_against_osv_ranges() {
    let packages = parse_lockfile(LockfileKind::GoMod, GO_MOD).unwrap();

    assert!(
        packages
            .iter()
            .all(|package| !package.key.version.starts_with('v')),
        "go.mod versions must be normalized: {:?}",
        packages
            .iter()
            .map(|package| package.key.to_string())
            .collect::<Vec<_>>()
    );
}

#[test]
fn npm_lockfile_version_one_is_still_read() {
    let packages = parse_lockfile(LockfileKind::NpmPackageLock, NPM_V1).unwrap();

    assert!(packages.iter().any(|package| {
        package.key.coordinate.name == "lodash" && package.key.version == "4.17.20"
    }));
}

#[test]
fn the_npm_project_entry_is_the_workspace_member_not_a_dependency() {
    let packages = parse_lockfile(LockfileKind::NpmPackageLock, NPM_V3).unwrap();
    let roots = packages
        .iter()
        .filter(|package| package.is_workspace_member)
        .collect::<Vec<_>>();

    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].key.coordinate.name, "app");
}

#[test]
fn composer_platform_requirements_are_not_treated_as_packages() {
    let packages = parse_lockfile(LockfileKind::ComposerLock, COMPOSER).unwrap();
    let monolog = packages
        .iter()
        .find(|package| package.key.coordinate.name == "monolog/monolog")
        .unwrap();

    assert!(
        monolog
            .dependencies
            .iter()
            .all(|dependency| dependency.coordinate.name != "php"),
        "`php` is a platform requirement, not a package"
    );
    assert_eq!(monolog.dependencies.len(), 1);
}

#[test]
fn composer_dev_packages_are_included() {
    let packages = parse_lockfile(LockfileKind::ComposerLock, COMPOSER).unwrap();

    assert!(packages
        .iter()
        .any(|package| package.key.coordinate.name == "phpunit/phpunit"));
}

#[test]
fn pypi_dependency_names_are_normalized_before_linking() {
    // Poetry writes `[package.dependencies]` keys in their published spelling,
    // which may differ from the PEP 503 normal form used for the package name.
    // Note the key is deliberately dot-free: a bare dotted key in TOML is a
    // nested table, not a literal name, so `My_Dep.Name = ...` would not test
    // what it looks like it tests.
    let source = r#"
[[package]]
name = "my-package"
version = "1.0.0"

[package.dependencies]
My_Dep = ">=1.0"

[[package]]
name = "my-dep"
version = "2.0.0"
"#;

    let packages = parse_lockfile(LockfileKind::PoetryLock, source).unwrap();
    let parent = packages
        .iter()
        .find(|package| package.key.coordinate.name == "my-package")
        .unwrap();

    assert_eq!(parent.dependencies.len(), 1, "normalized name must link");
}

#[test]
fn an_unrecognized_file_name_is_not_a_lockfile() {
    assert_eq!(LockfileKind::for_file_name("README.md"), None);
    assert_eq!(LockfileKind::for_file_name("Cargo.toml"), None);
    assert!(LockfileKind::for_file_name("Cargo.lock").is_some());
}

#[test]
fn a_malformed_lockfile_reports_an_error_rather_than_pretending_it_is_empty() {
    let error = parse_lockfile(LockfileKind::NpmPackageLock, "{ not json");

    assert!(
        error.is_err(),
        "a corrupt lockfile must not silently scan as zero packages"
    );
}

// ------------------------------------------------------------ dependency scope

const NPM_V3_DEV: &str = r#"{
  "name": "app",
  "lockfileVersion": 3,
  "packages": {
    "": { "name": "app", "version": "1.0.0" },
    "node_modules/lodash": { "version": "4.17.20" },
    "node_modules/jest": { "version": "29.0.0", "dev": true }
  }
}"#;

const PUBSPEC_DEV: &str = r#"
packages:
  http:
    dependency: "direct main"
    source: hosted
    version: "0.13.5"
  test:
    dependency: "direct dev"
    source: hosted
    version: "1.24.0"
  meta:
    dependency: transitive
    source: hosted
    version: "1.9.1"
"#;

const POETRY_DEV: &str = r#"
[[package]]
name = "requests"
version = "2.31.0"

[[package]]
name = "pytest"
version = "7.4.0"
groups = ["dev"]

[[package]]
name = "black"
version = "23.0.0"
category = "dev"
"#;

fn scope_of(packages: &[synaptic_vuln::ResolvedPackage], name: &str) -> PackageScope {
    packages
        .iter()
        .find(|package| package.key.coordinate.name == name)
        .unwrap_or_else(|| panic!("fixture resolves {name}"))
        .scope
}

#[test]
fn npm_marks_a_dev_dependency_as_development() {
    let packages = parse_lockfile(LockfileKind::NpmPackageLock, NPM_V3_DEV).unwrap();

    assert_eq!(scope_of(&packages, "jest"), PackageScope::Development);
    assert_eq!(scope_of(&packages, "lodash"), PackageScope::Runtime);
}

#[test]
fn composer_marks_the_dev_section_as_development() {
    let packages = parse_lockfile(LockfileKind::ComposerLock, COMPOSER).unwrap();

    assert_eq!(
        scope_of(&packages, "phpunit/phpunit"),
        PackageScope::Development
    );
    assert_eq!(
        scope_of(&packages, "monolog/monolog"),
        PackageScope::Runtime
    );
}

#[test]
fn pubspec_marks_a_direct_dev_dependency_but_not_a_transitive_one() {
    let packages = parse_lockfile(LockfileKind::PubspecLock, PUBSPEC_DEV).unwrap();

    assert_eq!(scope_of(&packages, "test"), PackageScope::Development);
    assert_eq!(scope_of(&packages, "http"), PackageScope::Runtime);
    // A transitive entry does not say whose transitive it is, so nothing is
    // known about it. Guessing "runtime" here would be an assumption dressed as
    // a reading of the file.
    assert_eq!(scope_of(&packages, "meta"), PackageScope::Unknown);
}

#[test]
fn poetry_marks_dev_groups_in_both_spellings() {
    let packages = parse_lockfile(LockfileKind::PoetryLock, POETRY_DEV).unwrap();

    // Poetry 1.5 and later.
    assert_eq!(scope_of(&packages, "pytest"), PackageScope::Development);
    // Poetry before 1.5.
    assert_eq!(scope_of(&packages, "black"), PackageScope::Development);
    assert_eq!(scope_of(&packages, "requests"), PackageScope::Runtime);
}

#[test]
fn a_format_without_a_scope_field_reports_unknown_not_runtime() {
    // The distinction matters: "we read that it is runtime" and "this file
    // cannot tell us" must not collapse into the same value, or an absence of
    // evidence starts reading as evidence.
    let packages = parse_lockfile(LockfileKind::GemfileLock, GEMFILE).unwrap();

    assert!(
        packages
            .iter()
            .all(|package| package.scope == PackageScope::Unknown),
        "Gemfile.lock records no dependency kind"
    );
}

#[test]
fn every_format_claiming_to_record_scope_actually_produces_one() {
    // The mirror of the dependency-edge coverage test: a format cannot claim a
    // capability in the documentation table without a fixture proving it.
    for fixture in fixtures() {
        if !fixture.kind.records_dependency_scope() {
            continue;
        }
        let packages = parse_lockfile(fixture.kind, fixture.source).unwrap();
        assert!(
            packages
                .iter()
                .any(|package| package.scope != PackageScope::Unknown),
            "{:?} claims to record dependency scope but resolved none",
            fixture.kind
        );
    }
}

#[test]
fn ecosystems_map_to_the_right_package_coordinates() {
    assert_eq!(LockfileKind::CargoLock.ecosystem(), Ecosystem::Cargo);
    assert_eq!(LockfileKind::YarnLock.ecosystem(), Ecosystem::Npm);
    assert_eq!(LockfileKind::UvLock.ecosystem(), Ecosystem::Pypi);
    assert_eq!(LockfileKind::GradleLockfile.ecosystem(), Ecosystem::Maven);
    assert_eq!(LockfileKind::MixLock.ecosystem(), Ecosystem::Hex);
}
