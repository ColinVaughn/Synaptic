//! `install` command(s) split from main.rs.

use anyhow::{Context, Result};
use codegraph_skillgen::Platform;

pub(crate) fn run_install(platform: &str) -> Result<()> {
    let p = Platform::parse(platform).with_context(|| {
        format!(
            "unknown platform '{platform}' (claude | agents | codex | opencode | gemini | \
                 cursor | copilot | kilo)"
        )
    })?;
    let root = std::env::current_dir().context("resolving current directory")?;
    let written = codegraph_skillgen::install(p, &root).context("installing skill")?;
    println!("Installed the CodeGraph skill:");
    for path in &written {
        println!("  {}", path.display());
    }
    Ok(())
}

pub(crate) fn run_uninstall(platform: &str, all: bool) -> Result<()> {
    let root = std::env::current_dir().context("resolving current directory")?;
    if all {
        for p in Platform::all() {
            codegraph_skillgen::uninstall(p, &root).context("uninstalling skill")?;
        }
        println!("Removed the CodeGraph skill from all platforms.");
        return Ok(());
    }
    let p = Platform::parse(platform).with_context(|| {
        format!(
            "unknown platform '{platform}' (claude | agents | codex | opencode | gemini | \
                 cursor | copilot | kilo)"
        )
    })?;
    codegraph_skillgen::uninstall(p, &root).context("uninstalling skill")?;
    println!("Removed the CodeGraph skill for {platform}.");
    Ok(())
}
