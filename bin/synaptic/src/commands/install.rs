//! `install` command(s) split from main.rs.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use synaptic_skillgen::Platform;

const PLATFORMS: &str = "claude | agents | codex | opencode | gemini | cursor | copilot | kilo";

/// Resolve Codex's config home: `CODEX_HOME` if set (Codex's own override),
/// else `~/.codex` (`HOME` then `USERPROFILE`, matching the global graph store).
fn codex_home() -> PathBuf {
    if let Some(h) = std::env::var_os("CODEX_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    match home {
        Some(h) => h.join(".codex"),
        None => PathBuf::from(".codex"),
    }
}

pub(crate) fn run_install(platform: &str, global: bool, refresh: bool) -> Result<()> {
    if refresh {
        return run_refresh();
    }
    let p = Platform::parse(platform)
        .with_context(|| format!("unknown platform '{platform}' ({PLATFORMS})"))?;
    let root = std::env::current_dir().context("resolving current directory")?;

    if global {
        if p != Platform::Codex {
            bail!("--global only applies to `codex` (the desktop app reads ~/.codex/config.toml)");
        }
        let home = codex_home();
        let written = synaptic_skillgen::install_codex_global(&root, &home)
            .context("installing Codex app integration")?;
        println!("Installed Synaptic for the Codex app (global config):");
        for path in &written {
            println!("  {}", path.display());
        }
        println!(
            "\nNext: build the graph with `synaptic extract .`, then restart the Codex app\n\
             and check Settings > MCP servers for the `synaptic-*` entry."
        );
        return Ok(());
    }

    let written = synaptic_skillgen::install(p, &root).context("installing skill")?;
    // Record the install so `self-update` / `install --refresh` can re-render it.
    synaptic_skillgen::record_install(&synaptic_skillgen::registry_path(), p, &root);
    println!("Installed the Synaptic skill:");
    for path in &written {
        println!("  {}", path.display());
    }
    if p == Platform::Codex {
        // The CLI reads project `.codex/` for trusted projects; the desktop app
        // does not (it reads only ~/.codex/config.toml). Point app users at --global.
        println!(
            "\nNote: this wrote project-scoped `.codex/` config (read by the Codex CLI for\n\
             trusted projects). If you use the Codex desktop app, run `synaptic install \
             codex --global`\ninstead. Build the graph first with `synaptic extract .`."
        );
    }
    Ok(())
}

pub(crate) fn run_uninstall(platform: &str, all: bool, global: bool) -> Result<()> {
    let root = std::env::current_dir().context("resolving current directory")?;

    if all && global {
        bail!("--all and --global cannot be combined (--global is codex-only and per-repo)");
    }

    if global {
        let p = Platform::parse(platform)
            .with_context(|| format!("unknown platform '{platform}' ({PLATFORMS})"))?;
        if p != Platform::Codex {
            bail!("--global only applies to `codex`");
        }
        synaptic_skillgen::uninstall_codex_global(&root, &codex_home())
            .context("uninstalling Codex app integration")?;
        println!("Removed Synaptic from the Codex app global config.");
        return Ok(());
    }

    let registry = synaptic_skillgen::registry_path();
    if all {
        for p in Platform::all() {
            synaptic_skillgen::uninstall(p, &root).context("uninstalling skill")?;
            synaptic_skillgen::record_uninstall(&registry, p, &root);
        }
        println!("Removed the Synaptic skill from all platforms.");
        return Ok(());
    }
    let p = Platform::parse(platform)
        .with_context(|| format!("unknown platform '{platform}' ({PLATFORMS})"))?;
    synaptic_skillgen::uninstall(p, &root).context("uninstalling skill")?;
    synaptic_skillgen::record_uninstall(&registry, p, &root);
    println!("Removed the Synaptic skill for {platform}.");
    Ok(())
}

/// `synaptic install --refresh`: re-render every recorded skill to the current
/// version. Shared by `self-update`.
fn run_refresh() -> Result<()> {
    let summary = synaptic_skillgen::refresh_all(&synaptic_skillgen::registry_path());
    print_refresh_summary(&summary);
    Ok(())
}

/// Print a refresh summary: a one-line roll-up plus a note per skill left
/// untouched because it was hand-edited.
pub(crate) fn print_refresh_summary(summary: &synaptic_skillgen::RefreshSummary) {
    match summary.line() {
        Some(line) => println!("{line}"),
        None => {
            println!("No installed skills recorded yet (run `synaptic install <host>`).");
            return;
        }
    }
    for (repo, host) in &summary.refreshed {
        println!("  refreshed {host} in {repo}");
    }
    for (repo, host, file) in &summary.skipped_edited {
        println!("  modified, left as-is: {host} {file} in {repo} (run `synaptic install {host}` to overwrite)");
    }
}
