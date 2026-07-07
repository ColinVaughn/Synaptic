use std::path::Path;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

fn synaptic(args: &[&str], dir: &Path) -> std::process::Output {
    Command::cargo_bin("synaptic")
        .unwrap()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run synaptic")
}

#[test]
fn readiness_audit_cli_emits_json_and_markdown() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("app.ts"),
        b"export function loadConfig() {\n  return undefined;\n}\n\nexport function compilePlan() {\n  throw new Error(\"TODO wire this\");\n}\n",
    )
    .unwrap();

    let ex = synaptic(&["extract", "."], root);
    assert!(
        ex.status.success(),
        "extract stderr: {}",
        String::from_utf8_lossy(&ex.stderr)
    );

    let audit = synaptic(
        &["audit", "readiness", "--profile", "generic", "--json"],
        root,
    );
    assert!(
        audit.status.success(),
        "audit stderr: {}",
        String::from_utf8_lossy(&audit.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&audit.stdout).unwrap();
    let findings = report["findings"].as_array().unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f["rule_id"] == "READY-SENTINEL-RETURN"),
        "report: {report}"
    );
    assert!(
        findings
            .iter()
            .any(|f| f["rule_id"] == "READY-PLACEHOLDER-001"),
        "report: {report}"
    );

    let md = std::fs::read_to_string(root.join("synaptic-out/readiness/readiness.md")).unwrap();
    assert!(md.contains("impact:"), "{md}");
    assert!(md.contains("fix:"), "{md}");
}
