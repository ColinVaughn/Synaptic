use std::fs;

use synaptic_api::{
    scan_dependencies, scan_dependencies_and_sbom_evidence, scan_sbom_evidence, Ecosystem,
    PackageUrl, SbomCompleteness,
};

#[test]
fn package_url_maps_known_and_unknown_types_without_losing_identity() {
    let npm = PackageUrl::parse("pkg:npm/%40acme/widgets@2.3.0").unwrap();
    assert_eq!(npm.package_type, "npm");
    assert_eq!(npm.namespace.as_deref(), Some("@acme"));
    assert_eq!(npm.name, "widgets");
    assert_eq!(npm.version.as_deref(), Some("2.3.0"));
    assert_eq!(npm.to_coordinate().ecosystem, Ecosystem::Npm);
    assert_eq!(npm.to_coordinate().name, "@acme/widgets");

    let maven = PackageUrl::parse("pkg:maven/org.apache.kafka/kafka-clients@3.9.0").unwrap();
    assert_eq!(maven.to_coordinate().ecosystem, Ecosystem::Maven);
    assert_eq!(maven.to_coordinate().name, "org.apache.kafka:kafka-clients");

    let unknown = PackageUrl::parse("pkg:hackage/aeson@2.2.3.0").unwrap();
    assert_eq!(unknown.to_coordinate().ecosystem, Ecosystem::Generic);
    assert_eq!(unknown.to_coordinate().name, "hackage/aeson");
    assert_eq!(unknown.to_string(), "pkg:hackage/aeson@2.2.3.0");
}

#[test]
fn cyclonedx_and_spdx_packages_join_the_normal_inventory() {
    let repo = tempfile::tempdir().unwrap();
    fs::write(
        repo.path().join("components.cdx.json"),
        r#"{
          "bomFormat":"CycloneDX",
          "specVersion":"1.6",
          "components":[
            {"type":"library","name":"widgets","version":"2.3.0","scope":"required","purl":"pkg:npm/%40acme/widgets@2.3.0"},
            {"type":"library","name":"aeson","version":"2.2.3.0","scope":"optional","purl":"pkg:hackage/aeson@2.2.3.0"}
          ],
          "services":[{"name":"widgets-api","endpoints":["https://api.acme.test/v1?token=secret"],"authenticated":true}],
          "compositions":[{"aggregate":"incomplete_third_party_only"}]
        }"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("release.spdx.json"),
        r#"{
          "spdxVersion":"SPDX-2.3",
          "packages":[{
            "name":"requests",
            "versionInfo":"2.32.4",
            "externalRefs":[{
              "referenceCategory":"PACKAGE-MANAGER",
              "referenceType":"purl",
              "referenceLocator":"pkg:pypi/requests@2.32.4"
            }]
          }]
        }"#,
    )
    .unwrap();
    fs::write(
        repo.path().join("bom.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
        <bom xmlns="http://cyclonedx.org/schema/bom/1.6" version="1">
          <components>
            <component type="library" bom-ref="pkg:golang/github.com/acme/client@v1.4.0">
              <group>github.com/acme</group>
              <name>client</name>
              <version>v1.4.0</version>
              <scope>required</scope>
              <purl>pkg:golang/github.com/acme/client@v1.4.0</purl>
            </component>
          </components>
          <services><service bom-ref="urn:service:ledger">
            <name>ledger</name><endpoints><endpoint>grpc://ledger.acme.test:443</endpoint></endpoints>
          </service></services>
          <compositions><composition><aggregate>complete</aggregate></composition></compositions>
        </bom>"#,
    )
    .unwrap();

    let dependencies = scan_dependencies(repo.path()).unwrap();

    assert!(dependencies.iter().any(|dependency| {
        dependency.package.ecosystem == Ecosystem::Npm
            && dependency.package.name == "@acme/widgets"
            && dependency.resolved_version.as_deref() == Some("2.3.0")
            && dependency.purl.as_deref() == Some("pkg:npm/%40acme/widgets@2.3.0")
    }));
    assert!(dependencies.iter().any(|dependency| {
        dependency.package.ecosystem == Ecosystem::Generic
            && dependency.package.name == "hackage/aeson"
    }));
    assert!(dependencies.iter().any(|dependency| {
        dependency.package.ecosystem == Ecosystem::Pypi
            && dependency.package.name == "requests"
            && dependency.source_file == "release.spdx.json"
    }));
    assert!(dependencies.iter().any(|dependency| {
        dependency.package.ecosystem == Ecosystem::Go
            && dependency.package.name == "github.com/acme/client"
            && dependency.resolved_version.as_deref() == Some("v1.4.0")
            && dependency.source_file == "bom.xml"
    }));

    let evidence = scan_sbom_evidence(repo.path()).unwrap();
    let (combined_dependencies, combined_evidence) =
        scan_dependencies_and_sbom_evidence(repo.path()).unwrap();
    assert_eq!(combined_dependencies, dependencies);
    assert_eq!(combined_evidence, evidence);
    assert_eq!(evidence.documents.len(), 3);
    assert!(evidence.documents.iter().any(|document| {
        document.source_file == "bom.xml" && document.completeness == SbomCompleteness::Complete
    }));
    assert!(evidence.documents.iter().any(|document| {
        document.source_file == "components.cdx.json"
            && document.completeness == SbomCompleteness::Incomplete
    }));
    assert_eq!(evidence.services.len(), 2);
    assert!(evidence.services.iter().any(|service| {
        service.name == "widgets-api"
            && service.endpoints == ["https://api.acme.test/v1"]
            && service.authenticated == Some(true)
    }));
}

#[test]
fn cyclonedx_xml_rejects_dtds_and_entities() {
    let repo = tempfile::tempdir().unwrap();
    fs::write(
        repo.path().join("bom.xml"),
        r#"<?xml version="1.0"?>
        <!DOCTYPE bom [<!ENTITY secret SYSTEM "file:///etc/passwd">]>
        <bom xmlns="http://cyclonedx.org/schema/bom/1.6"><components>
          <component type="library"><name>&secret;</name></component>
        </components></bom>"#,
    )
    .unwrap();

    let error = scan_dependencies(repo.path()).unwrap_err().to_string();
    assert!(error.contains("DTD or entity"), "unexpected error: {error}");
}

#[test]
fn malformed_purls_fail_closed() {
    for invalid in ["npm:stripe", "pkg:", "pkg:npm/", "pkg:npm/name@"] {
        assert!(PackageUrl::parse(invalid).is_err(), "accepted {invalid}");
    }
}
