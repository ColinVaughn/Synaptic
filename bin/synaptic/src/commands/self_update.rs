//! `self-update` command: opt-in self-replacement from GitHub Releases.

use std::io::Write;

use anyhow::{Context, Result};
use synaptic_upgrade::config::{config_path, UpdateConfig};
use synaptic_upgrade::{github, releases_url, target, updater, version_is_newer};

const CURRENT: &str = env!("CARGO_PKG_VERSION");

pub(crate) fn run_self_update(enable: bool, disable: bool, check: bool, yes: bool) -> Result<()> {
    if enable || disable {
        let path = config_path();
        let mut cfg = UpdateConfig::load(&path).unwrap_or_default();
        cfg.enabled = enable; // exactly one of enable/disable is set (clap-enforced)
        cfg.save(&path)?;
        println!(
            "Background update check {}.",
            if enable { "enabled" } else { "disabled" }
        );
        return Ok(());
    }

    let release = github::latest_release().context("checking for the latest release")?;
    if !version_is_newer(CURRENT, &release.version) {
        println!("Synaptic is up to date ({CURRENT}).");
        return Ok(());
    }

    let latest = release.version.trim_start_matches('v');
    println!("Update available: {CURRENT} -> {latest}");
    if check {
        return Ok(());
    }

    let triple = match target::current_target() {
        Some(t) => t,
        None => {
            println!(
                "No prebuilt binary is published for this platform.\n\
                 Download or build manually from {}",
                releases_url()
            );
            return Ok(());
        }
    };

    if !release.notes.trim().is_empty() {
        println!("\nRelease notes:\n{}\n", release.notes.trim());
    }

    if !yes && !confirm("Download and replace the current binary? [y/N] ")? {
        println!("Aborted.");
        return Ok(());
    }

    updater::apply_update(&release, triple)?;
    println!("Updated to {latest}. Restart synaptic to use the new version.");
    refresh_installed_skills();
    Ok(())
}

/// Re-render installed skills to the new version. The file at our own exe path was
/// just replaced, so the running (old) process can't render the new content
/// itself — spawn the new binary to do it. Best-effort: on failure, point the user
/// at the manual command.
fn refresh_installed_skills() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let ran = std::process::Command::new(&exe)
        .args(["install", "--refresh"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ran {
        eprintln!("(note) run `synaptic install --refresh` to update installed skill files");
    }
}

/// Prompt on stderr, read a line from stdin, return true only for y/yes.
fn confirm(prompt: &str) -> Result<bool> {
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading confirmation")?;
    let ans = line.trim().to_ascii_lowercase();
    Ok(ans == "y" || ans == "yes")
}
