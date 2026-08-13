use std::fs;

use synaptic_api::{
    ApiMaintenanceConfig, ApiOperationAnchor, Dependency, DependencyScope, Ecosystem,
    PackageCoordinate, VendorMatch, VendorRegistry, inventory, scan_dependencies,
};

fn config() -> ApiMaintenanceConfig {
    ApiMaintenanceConfig::parse(
        r#"
schema = 1

[[vendors]]
id = "stripe"
packages = ["npm:stripe", "pypi:stripe"]
hosts = ["api.stripe.com"]

[[vendors]]
id = "acme_pay"
packages = ["npm:@acme/payments"]
hosts = ["payments.example.test"]
"#,
    )
    .unwrap()
}

#[test]
fn config_and_registry_are_vendor_neutral() {
    let config = config();
    assert_eq!(config.schema, 1);
    assert_eq!(config.vendors.len(), 2);
    assert!(config.vendors.iter().all(|vendor| vendor.enabled));

    let registry = VendorRegistry::new(config).unwrap();
    let stripe = Dependency::new(
        PackageCoordinate::new(Ecosystem::Npm, "stripe"),
        "package.json",
        DependencyScope::Runtime,
    );
    let acme = Dependency::new(
        PackageCoordinate::new(Ecosystem::Npm, "@acme/payments"),
        "package.json",
        DependencyScope::Runtime,
    );
    assert_eq!(
        registry.match_dependency(&stripe),
        VendorMatch::Matched {
            vendor_id: "stripe".into()
        }
    );
    assert_eq!(
        registry.match_dependency(&acme),
        VendorMatch::Matched {
            vendor_id: "acme_pay".into()
        }
    );
}

#[test]
fn overlapping_package_rules_fail_closed_as_ambiguous() {
    let config = ApiMaintenanceConfig::parse(
        r#"
schema = 1
[[vendors]]
id = "one"
packages = ["npm:shared-sdk"]
[[vendors]]
id = "two"
packages = ["npm:shared-sdk"]
"#,
    )
    .unwrap();
    let registry = VendorRegistry::new(config).unwrap();
    let dep = Dependency::new(
        PackageCoordinate::new(Ecosystem::Npm, "shared-sdk"),
        "package.json",
        DependencyScope::Runtime,
    );
    assert_eq!(
        registry.match_dependency(&dep),
        VendorMatch::Ambiguous {
            vendor_ids: vec!["one".into(), "two".into()]
        }
    );
}

#[test]
fn operation_ids_are_stable_but_vendor_scoped() {
    let stripe_a = ApiOperationAnchor::new("stripe", "https", "get", "/v1/customers");
    let stripe_b = ApiOperationAnchor::new("STRIPE", "HTTPS", "GET", "v1/customers/");
    let other = ApiOperationAnchor::new("other", "https", "GET", "/v1/customers");

    assert_eq!(stripe_a.id, stripe_b.id, "equivalent spellings normalize");
    assert_eq!(stripe_a.method, "GET");
    assert_eq!(stripe_a.canonical_path, "/v1/customers");
    assert_ne!(
        stripe_a.id, other.id,
        "the vendor is part of operation identity"
    );
}

#[test]
fn inventory_resolves_node_and_python_versions_for_multiple_vendors() {
    let repo = tempfile::tempdir().unwrap();
    fs::write(
        repo.path().join("package.json"),
        r#"{
          "dependencies": {"stripe": "^18.0.0", "@acme/payments": "^2.0.0"},
          "devDependencies": {"eslint": "^9.0.0"}
        }"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("package-lock.json"),
        r#"{
          "lockfileVersion": 3,
          "packages": {
            "node_modules/stripe": {"version": "18.2.1"},
            "node_modules/@acme/payments": {"version": "2.4.0"},
            "node_modules/eslint": {"version": "9.1.0"}
          }
        }"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("pyproject.toml"),
        r#"
[project]
dependencies = ["stripe>=10,<11", "httpx>=0.27"]
"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("poetry.lock"),
        r#"
[[package]]
name = "stripe"
version = "10.1.0"

[[package]]
name = "httpx"
version = "0.27.2"
"#,
    )
    .unwrap();

    let registry = VendorRegistry::new(config()).unwrap();
    let report = inventory(repo.path(), &registry).unwrap();

    assert_eq!(report.dependencies.len(), 5, "runtime + dev dependencies");
    assert_eq!(
        report
            .matched
            .iter()
            .map(|entry| (
                entry.vendor_id.as_str(),
                entry.dependency.package.to_string(),
                entry.dependency.resolved_version.as_deref()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("acme_pay", "npm:@acme/payments".to_string(), Some("2.4.0")),
            ("stripe", "npm:stripe".to_string(), Some("18.2.1")),
            ("stripe", "pypi:stripe".to_string(), Some("10.1.0")),
        ]
    );
    assert_eq!(
        report
            .unmatched
            .iter()
            .map(|dependency| dependency.package.to_string())
            .collect::<Vec<_>>(),
        vec!["npm:eslint", "pypi:httpx"]
    );
    assert!(report.ambiguous.is_empty());
}

#[test]
fn inventory_skips_dependency_and_build_directories() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join("node_modules/noise")).unwrap();
    fs::create_dir_all(repo.path().join("target/noise")).unwrap();
    fs::write(
        repo.path().join("node_modules/noise/package.json"),
        r#"{"dependencies":{"stripe":"1"}}"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("target/noise/pyproject.toml"),
        "[project]\ndependencies=['stripe==1']\n",
    )
    .unwrap();

    let registry = VendorRegistry::new(config()).unwrap();
    let report = inventory(repo.path(), &registry).unwrap();
    assert!(report.dependencies.is_empty());
    assert!(report.matched.is_empty());
}

#[test]
fn inventory_reads_every_applicable_language_package_ecosystem() {
    let repo = tempfile::tempdir().unwrap();
    let write = |relative: &str, contents: &str| {
        let path = repo.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    };

    write(
        "php/composer.json",
        r#"{"require":{"php":"^8.2","stripe/stripe-php":"^17"}}"#,
    );
    write(
        "php/composer.lock",
        r#"{"packages":[{"name":"stripe/stripe-php","version":"17.4.0"}]}"#,
    );
    write(
        "ruby/Gemfile",
        "source 'https://rubygems.org'\ngem 'stripe', '~> 13.0'\n",
    );
    write(
        "ruby/Gemfile.lock",
        "GEM\n  specs:\n    stripe (13.1.0)\n\nDEPENDENCIES\n  stripe (~> 13.0)\n",
    );
    write(
        "swift/Package.swift",
        r#"let package = Package(name: "App", dependencies: [.package(url: "https://github.com/stripe/stripe-ios.git", from: "24.0.0")])"#,
    );
    write(
        "swift/Package.resolved",
        r#"{"version":2,"pins":[{"identity":"stripe-ios","location":"https://github.com/stripe/stripe-ios.git","state":{"version":"24.3.0","revision":"abc"}}]}"#,
    );
    write("dart/pubspec.yaml", "dependencies:\n  stripe_sdk: ^1.0.0\n");
    write(
        "dart/pubspec.lock",
        "packages:\n  stripe_sdk:\n    dependency: direct main\n    version: 1.2.0\n",
    );
    write(
        "elixir/mix.exs",
        r#"defp deps do
  [{:stripity_stripe, "~> 3.2"}]
end"#,
    );
    write(
        "elixir/mix.lock",
        r#"%{"stripity_stripe": {:hex, :stripity_stripe, "3.2.0", "checksum", [:mix], [], "hexpm", "checksum"}}"#,
    );
    write(
        "lua/client.rockspec",
        r#"package = "client"
version = "1.0-1"
dependencies = { "lua >= 5.1", "stripe >= 1.2" }"#,
    );
    write(
        "lua/luarocks.lock",
        r#"return { ["stripe"] = { ["1.2.3-1"] = {} } }"#,
    );
    write("julia/Project.toml", "[deps]\nStripe = \"uuid\"\n");
    write(
        "julia/Manifest.toml",
        "[[deps.Stripe]]\nuuid = \"uuid\"\nversion = \"0.4.0\"\n",
    );
    write(
        "zig/build.zig.zon",
        r#".{ .name = .app, .dependencies = .{ .stripe = .{ .url = "https://example.test/stripe-zig/archive/v1.2.3.tar.gz", .hash = "hash" } } }"#,
    );
    write("objc/Podfile", "pod 'StripePaymentSheet', '~> 24.0'\n");
    write(
        "objc/Podfile.lock",
        "PODS:\n  - StripePaymentSheet (24.3.0)\nDEPENDENCIES:\n  - StripePaymentSheet (~> 24.0)\n",
    );
    write("cpp/conanfile.txt", "[requires]\nstripe-cpp/1.2.3\n");
    write(
        "vcpkg/vcpkg.json",
        r#"{"dependencies":["stripe"],"overrides":[{"name":"stripe","version":"1.2.3"}]}"#,
    );
    write(
        "powershell/Client.psd1",
        "@{ RequiredModules = @(@{ ModuleName = 'Stripe'; ModuleVersion = '1.2.3' }) }",
    );
    write(
        "fortran/fpm.toml",
        "[dependencies]\nstripe = { git = \"https://example.test/stripe-fpm\", tag = \"v1.2.3\" }\n",
    );
    write(
        "codeql/qlpack.yml",
        "name: app/queries\ndependencies:\n  codeql/javascript-all: 2.16.0\n",
    );
    write(
        "apex/sfdx-project.json",
        r#"{"packageDirectories":[{"path":"force-app","dependencies":[{"package":"StripeSDK@1.2.3-1"}]}]}"#,
    );

    let dependencies = scan_dependencies(repo.path()).unwrap();
    let observed = dependencies
        .iter()
        .map(|dependency| {
            (
                dependency.package.to_string(),
                dependency.resolved_version.as_deref(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    for expected in [
        ("cocoapods:stripepaymentsheet", "24.3.0"),
        ("codeql:codeql/javascript-all", "2.16.0"),
        ("composer:stripe/stripe-php", "17.4.0"),
        ("conan:stripe-cpp", "1.2.3"),
        ("fpm:stripe", "v1.2.3"),
        ("gem:stripe", "13.1.0"),
        ("hex:stripity_stripe", "3.2.0"),
        ("julia:stripe", "0.4.0"),
        ("luarocks:stripe", "1.2.3-1"),
        ("powershell:stripe", "1.2.3"),
        ("pub:stripe_sdk", "1.2.0"),
        ("salesforce:stripesdk", "1.2.3-1"),
        ("swift:stripe-ios", "24.3.0"),
        ("vcpkg:stripe", "1.2.3"),
        ("zig:stripe", "v1.2.3"),
    ] {
        assert_eq!(
            observed.get(expected.0).copied().flatten(),
            Some(expected.1),
            "missing or unresolved {expected:?}; observed={observed:?}"
        );
    }
}

#[test]
fn inventory_reads_pnpm_yarn_uv_go_cargo_jvm_and_dotnet_versions() {
    let repo = tempfile::tempdir().unwrap();

    let pnpm = repo.path().join("apps/pnpm");
    fs::create_dir_all(&pnpm).unwrap();
    fs::write(
        pnpm.join("package.json"),
        r#"{"dependencies":{"pnpm-sdk":"^4"}}"#,
    )
    .unwrap();
    fs::write(
        pnpm.join("pnpm-lock.yaml"),
        "lockfileVersion: '9.0'\npackages:\n  pnpm-sdk@4.3.2:\n    resolution: {}\n",
    )
    .unwrap();

    let yarn = repo.path().join("apps/yarn");
    fs::create_dir_all(&yarn).unwrap();
    fs::write(
        yarn.join("package.json"),
        r#"{"dependencies":{"yarn-sdk":"^5"}}"#,
    )
    .unwrap();
    fs::write(
        yarn.join("yarn.lock"),
        "\"yarn-sdk@^5\":\n  version \"5.2.0\"\n",
    )
    .unwrap();

    let python = repo.path().join("apps/python");
    fs::create_dir_all(&python).unwrap();
    fs::write(
        python.join("pyproject.toml"),
        "[project]\ndependencies = ['uv-sdk>=2']\n",
    )
    .unwrap();
    fs::write(
        python.join("uv.lock"),
        "version = 1\n[[package]]\nname = 'uv-sdk'\nversion = '2.4.1'\n",
    )
    .unwrap();

    fs::write(
        repo.path().join("go.mod"),
        "module example.test/app\n\ngo 1.24\nrequire (\n  example.test/go-sdk v1.8.0\n)\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n[dependencies]\nrust-sdk='0.7'\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("Cargo.lock"),
        "version = 4\n[[package]]\nname = 'rust-sdk'\nversion = '0.7.3'\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("pom.xml"),
        r#"<project><properties><acme.version>3.1.4</acme.version></properties><dependencies><dependency><groupId>com.acme</groupId><artifactId>jvm-sdk</artifactId><version>${acme.version}</version></dependency></dependencies></project>"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("app.csproj"),
        r#"<Project><ItemGroup><PackageReference Include="Acme.DotnetSdk" Version="6.2.0" /></ItemGroup></Project>"#,
    )
    .unwrap();

    let dependencies = synaptic_api::scan_dependencies(repo.path()).unwrap();
    let versions = dependencies
        .iter()
        .map(|dependency| {
            (
                dependency.package.to_string(),
                dependency.resolved_version.clone(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();

    for (package, version) in [
        ("npm:pnpm-sdk", "4.3.2"),
        ("npm:yarn-sdk", "5.2.0"),
        ("pypi:uv-sdk", "2.4.1"),
        ("go:example.test/go-sdk", "v1.8.0"),
        ("cargo:rust-sdk", "0.7.3"),
        ("maven:com.acme:jvm-sdk", "3.1.4"),
        ("nuget:acme.dotnetsdk", "6.2.0"),
    ] {
        assert_eq!(
            versions.get(package).and_then(Option::as_deref),
            Some(version),
            "resolved version for {package}: {versions:?}"
        );
    }
}
