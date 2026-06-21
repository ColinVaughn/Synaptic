//! CLI tests for `self-update` that do not touch the network.
//!
//! The file name avoids the substring "update": Windows force-elevates (UAC) any
//! executable whose name contains "update"/"setup"/"install"/"patch", which would
//! break the compiled test binary.

use assert_cmd::Command;

#[test]
fn enable_then_disable_writes_config() {
    let home = tempfile::tempdir().unwrap();

    Command::cargo_bin("synaptic")
        .unwrap()
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("SYNAPTIC_UPDATE_CHECK", "0")
        .args(["self-update", "--enable"])
        .assert()
        .success();

    let cfg = std::fs::read_to_string(home.path().join(".synaptic/update.toml")).unwrap();
    assert!(cfg.contains("enabled = true"), "config was: {cfg}");

    Command::cargo_bin("synaptic")
        .unwrap()
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("SYNAPTIC_UPDATE_CHECK", "0")
        .args(["self-update", "--disable"])
        .assert()
        .success();

    let cfg = std::fs::read_to_string(home.path().join(".synaptic/update.toml")).unwrap();
    assert!(cfg.contains("enabled = false"), "config was: {cfg}");
}

#[test]
fn enable_and_disable_conflict() {
    Command::cargo_bin("synaptic")
        .unwrap()
        .env("SYNAPTIC_UPDATE_CHECK", "0")
        .args(["self-update", "--enable", "--disable"])
        .assert()
        .failure();
}
