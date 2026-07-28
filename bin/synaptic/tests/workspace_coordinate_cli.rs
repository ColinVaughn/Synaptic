use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

#[test]
fn workspace_coordinate_reports_the_engine_detected_package_identity_as_json() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("package.json"),
        r#"{"name":"@acme/shared","version":"1.0.0"}"#,
    )
    .unwrap();

    let output = Command::cargo_bin("synaptic")
        .unwrap()
        .args(["workspace", "coordinate", ".", "--json"])
        .current_dir(root.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ecosystem"], "npm");
    assert_eq!(value["name"], "@acme/shared");
}
