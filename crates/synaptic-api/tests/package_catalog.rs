use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use synaptic_api::{
    CachedPackageMetadataResolver, PackageMetadata, PackageMetadataError, PackageMetadataResolver,
    PackageUrl,
};

struct FakeResolver<'a> {
    calls: &'a AtomicUsize,
}

impl PackageMetadataResolver for FakeResolver<'_> {
    fn resolve(
        &self,
        package: &PackageUrl,
        version: Option<&str>,
    ) -> Result<Option<PackageMetadata>, PackageMetadataError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some(PackageMetadata {
            purl: package.to_string(),
            version: version.map(str::to_string),
            repository: Some("https://github.com/acme/widgets".into()),
            homepage: Some("https://widgets.acme.test".into()),
            release_source: Some("https://registry.npmjs.org/@acme/widgets".into()),
            artifact_digest: Some("a".repeat(64)),
            provenance: vec!["registry metadata".into()],
        }))
    }
}

#[test]
fn metadata_cache_is_content_addressed_and_survives_resolver_instances() {
    let directory = tempfile::tempdir().unwrap();
    let package = PackageUrl::parse("pkg:npm/%40acme/widgets@2.0.0").unwrap();
    let calls = AtomicUsize::new(0);
    let resolver =
        CachedPackageMetadataResolver::new(directory.path(), FakeResolver { calls: &calls })
            .unwrap();
    let first = resolver.resolve(&package, Some("2.0.0")).unwrap().unwrap();
    assert_eq!(
        first.repository.as_deref(),
        Some("https://github.com/acme/widgets")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(resolver);

    let second_calls = AtomicUsize::new(0);
    let resolver = CachedPackageMetadataResolver::new(
        directory.path(),
        FakeResolver {
            calls: &second_calls,
        },
    )
    .unwrap();
    let second = resolver.resolve(&package, Some("2.0.0")).unwrap();
    assert_eq!(second, Some(first));
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn metadata_validation_rejects_unscoped_or_unpinned_artifacts() {
    let mut metadata = PackageMetadata {
        purl: "pkg:npm/a@1".into(),
        version: Some("1".into()),
        repository: None,
        homepage: None,
        release_source: None,
        artifact_digest: Some("not-a-digest".into()),
        provenance: vec![],
    };
    assert!(metadata.validate().is_err());
    metadata.artifact_digest = Some("b".repeat(64));
    assert!(metadata.validate().is_err(), "provenance is mandatory");
}

#[test]
fn cache_content_tampering_is_detected_and_entries_can_be_revoked() {
    let directory = tempfile::tempdir().unwrap();
    let package = PackageUrl::parse("pkg:npm/%40acme/widgets@2.0.0").unwrap();
    let calls = AtomicUsize::new(0);
    let resolver =
        CachedPackageMetadataResolver::new(directory.path(), FakeResolver { calls: &calls })
            .unwrap();
    resolver.resolve(&package, Some("2.0.0")).unwrap();
    let cache_file = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&cache_file).unwrap()).unwrap();
    value["metadata"]["repository"] = "https://evil.example.test/replaced".into();
    fs::write(&cache_file, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    assert!(matches!(
        resolver.resolve(&package, Some("2.0.0")),
        Err(PackageMetadataError::PoisonedCache(_))
    ));

    fs::remove_file(&cache_file).unwrap();
    resolver.resolve(&package, Some("2.0.0")).unwrap();
    resolver.revoke(&package, Some("2.0.0")).unwrap();
    assert!(!cache_file.exists());
}

#[test]
fn tenant_cache_namespaces_do_not_share_entries() {
    let directory = tempfile::tempdir().unwrap();
    let package = PackageUrl::parse("pkg:npm/%40acme/widgets@2.0.0").unwrap();
    let calls = AtomicUsize::new(0);
    let first = CachedPackageMetadataResolver::new_for_tenant(
        directory.path(),
        "tenant-a",
        FakeResolver { calls: &calls },
    )
    .unwrap();
    let second = CachedPackageMetadataResolver::new_for_tenant(
        directory.path(),
        "tenant-b",
        FakeResolver { calls: &calls },
    )
    .unwrap();
    first.resolve(&package, Some("2.0.0")).unwrap();
    second.resolve(&package, Some("2.0.0")).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        CachedPackageMetadataResolver::<FakeResolver<'_>>::new_for_tenant(
            directory.path(),
            "../escape",
            FakeResolver { calls: &calls }
        )
        .is_err()
    );
}
