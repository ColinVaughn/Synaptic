use synaptic_api::{
    ApiMaintenanceConfig, BreakingChangeKind, ConfiguredVendorAdapter, Ecosystem, FetchedArtifact,
    OpenApiAdapter, PackageReleaseAdapter, SourceArtifact, VendorAdapter, VendorRegistry,
    VersionRange,
};

fn fetched(uri: &str, revision: &str, body: &str) -> FetchedArtifact {
    FetchedArtifact::new(
        uri,
        revision,
        "application/json",
        body.as_bytes().to_vec(),
        42,
    )
}

#[test]
fn package_release_adapter_diffs_removed_exports_and_signature_changes() {
    let adapter = PackageReleaseAdapter::new("acme", "npm:@acme/sdk");
    let old = adapter
        .normalize_surface(&fetched(
            "https://acme.example/sdk.json",
            "1",
            r#"{"version":"1.0.0","minimum_supported_version":"1.0.0","exports":{"widgets.create":"(name, opts)","widgets.legacy":"()"}}"#,
        ))
        .unwrap();
    let new_artifact = fetched(
        "https://acme.example/sdk.json",
        "2",
        r#"{"version":"2.0.0","minimum_supported_version":"1.5.0","exports":{"widgets.create":"(request)"}}"#,
    );
    let new = adapter.normalize_surface(&new_artifact).unwrap();
    let changes = adapter
        .diff_surfaces(
            &old,
            &new,
            SourceArtifact {
                uri: new_artifact.uri,
                revision: new_artifact.revision,
                etag: None,
                last_modified: None,
                content_digest: new_artifact.content_digest,
                fetched_at: 42,
                adapter_version: 1,
                evidence_kind: "package_release".into(),
            },
            VersionRange::parse(">=1.0.0, <2.0.0").unwrap(),
        )
        .unwrap();

    assert_eq!(changes.len(), 3);
    assert!(
        changes
            .iter()
            .any(|change| change.kind == BreakingChangeKind::SdkExportRemoved)
    );
    assert!(
        changes
            .iter()
            .any(|change| change.kind == BreakingChangeKind::SdkSignatureChanged)
    );
    assert!(
        changes
            .iter()
            .any(|change| change.kind == BreakingChangeKind::MinimumSupportedVersionRaised)
    );
    assert!(changes.iter().all(|change| change.confidence == 1.0));
}

#[test]
fn configured_adapter_exposes_mappings_for_any_vendor() {
    let registry = VendorRegistry::new(
        ApiMaintenanceConfig::parse(
            r#"
schema = 1
[[vendors]]
id = "pager"
packages = ["npm:pager-sdk"]
hosts = ["api.pager.example"]
[[vendors.sdk_bindings]]
package = "npm:pager-sdk"
member = "incidents.resolve"
method = "POST"
path = "/v2/incidents/{id}/resolve"
"#,
        )
        .unwrap(),
    )
    .unwrap();
    let adapter = ConfiguredVendorAdapter::new(registry.vendor("pager").unwrap());
    assert_eq!(adapter.id(), "pager");
    assert_eq!(adapter.host_matchers(), &["api.pager.example"]);
    assert_eq!(adapter.sdk_bindings(Ecosystem::Npm).len(), 1);

    let openapi = OpenApiAdapter::new(adapter.id());
    let contract = openapi
        .normalize_contract(&fetched(
            "fixture.json",
            "1",
            r#"{"openapi":"3.0.0","paths":{}}"#,
        ))
        .unwrap();
    assert_eq!(contract.vendor, "pager");
}
