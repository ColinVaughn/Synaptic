//! `install` command(s) split from main.rs.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use codegraph_skillgen::Platform;

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

pub(crate) fn run_install(platform: &str, global: bool) -> Result<()> {
    let p = Platform::parse(platform)
        .with_context(|| format!("unknown platform '{platform}' ({PLATFORMS})"))?;
    let root = std::env::current_dir().context("resolving current directory")?;

    if global {
        if p != Platform::Codex {
            bail!("--global only applies to `codex` (the desktop app reads ~/.codex/config.toml)");
        }
        let home = codex_home();
        let written = codegraph_skillgen::install_codex_global(&root, &home)
            .context("installing Codex app integration")?;
        println!("Installed CodeGraph for the Codex app (global config):");
        for path in &written {
            println!("  {}", path.display());
        }
        println!(
            "\nNext: build the graph with `codegraph extract .`, then restart the Codex app\n\
             and check Settings > MCP servers for the `codegraph-*` entry."
        );
        return Ok(());
    }

    let written = codegraph_skillgen::install(p, &root).context("installing skill")?;
    println!("Installed the CodeGraph skill:");
    for path in &written {
        println!("  {}", path.display());
    }
    if p == Platform::Codex {
        // The CLI reads project `.codex/` for trusted projects; the desktop app
        // does not (it reads only ~/.codex/config.toml). Point app users at --global.
        println!(
            "\nNote: this wrote project-scoped `.codex/` config (read by the Codex CLI for\n\
             trusted projects). If you use the Codex desktop app, run `codegraph install \
             codex --global`\ninstead. Build the graph first with `codegraph extract .`."
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
        codegraph_skillgen::uninstall_codex_global(&root, &codex_home())
            .context("uninstalling Codex app integration")?;
        println!("Removed CodeGraph from the Codex app global config.");
        return Ok(());
    }

    if all {
        for p in Platform::all() {
            codegraph_skillgen::uninstall(p, &root).context("uninstalling skill")?;
        }
        println!("Removed the CodeGraph skill from all platforms.");
        return Ok(());
    }
    let p = Platform::parse(platform)
        .with_context(|| format!("unknown platform '{platform}' ({PLATFORMS})"))?;
    codegraph_skillgen::uninstall(p, &root).context("uninstalling skill")?;
    println!("Removed the CodeGraph skill for {platform}.");
    Ok(())
}
