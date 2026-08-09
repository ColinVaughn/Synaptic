use std::fs;

use assert_cmd::Command;

#[test]
fn scans_pinned_gradle_declarations_without_a_lockfile() {
    let repository = tempfile::tempdir().unwrap();
    let advisories = tempfile::tempdir().unwrap();
    fs::write(
        repository.path().join("gradle.properties"),
        "log4j.version=2.14.1\n",
    )
    .unwrap();
    fs::write(
        repository.path().join("build.gradle.kts"),
        r#"
dependencies {
    implementation("org.apache.logging.log4j:log4j-core:${rootProject.property("log4j.version")}")
}
"#,
    )
    .unwrap();
    fs::write(
        advisories.path().join("GHSA-maven-0001.json"),
        r#"{
  "id": "GHSA-maven-0001",
  "summary": "log4j-core is vulnerable",
  "affected": [{
    "package": { "ecosystem": "Maven", "name": "org.apache.logging.log4j:log4j-core" },
    "ranges": [{
      "type": "ECOSYSTEM",
      "events": [{ "introduced": "0" }, { "fixed": "2.17.1" }]
    }]
  }]
}"#,
    )
    .unwrap();

    let output = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["vuln", "scan", "--root"])
        .arg(repository.path())
        .arg("--advisories")
        .arg(advisories.path())
        .args(["--offline", "--json"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "scan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["packages_scanned"], 0);
    assert_eq!(report["packages_partially_audited"], 1);
    assert_eq!(report["coverage"]["maven"], "direct_only");
    assert_eq!(
        report["findings"][0]["package"],
        "maven:org.apache.logging.log4j:log4j-core"
    );
    assert_eq!(report["findings"][0]["resolved_version"], "2.14.1");
}

#[test]
fn reports_a_stable_reason_when_no_dependency_version_is_auditable() {
    let repository = tempfile::tempdir().unwrap();
    fs::write(
        repository.path().join("build.gradle.kts"),
        r#"dependencies { implementation("example:library:${missing.version}") }"#,
    )
    .unwrap();

    let output = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["vuln", "scan", "--root"])
        .arg(repository.path())
        .args(["--offline", "--json"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("[synaptic:vuln:no-auditable-dependencies]"));
}

#[test]
fn exposes_the_verified_vulnerability_repair_and_draft_publication_lifecycle() {
    let output = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["vuln", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    for command in [
        "repair",
        "verify",
        "publish",
        "run",
        "export-run",
        "import-run",
    ] {
        assert!(
            help.contains(command),
            "vulnerability help omitted {command}"
        );
    }

    let publish = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["vuln", "publish", "--help"])
        .output()
        .unwrap();
    assert!(publish.status.success());
    let publish_help = String::from_utf8_lossy(&publish.stdout);
    assert!(publish_help.contains("draft PR or MR"));
    assert!(publish_help.contains("--provider"));
    assert!(publish_help.contains("--target-branch"));
}
