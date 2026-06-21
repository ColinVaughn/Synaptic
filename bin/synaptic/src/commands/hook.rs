//! `hook` command(s) split from main.rs.

use crate::cli::HookAction;
use anyhow::{Context, Result};
use synaptic_incremental::hooks;

pub(crate) fn run_hook(action: HookAction) -> Result<()> {
    let cwd = std::env::current_dir().context("resolving current directory")?;
    let root = hooks::repo_root(&cwd).map_err(|e| anyhow::anyhow!("{e}"))?;
    let states = match action {
        HookAction::Install => {
            let s = hooks::install(&root).map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("Installed hooks + registered the graph.json merge driver.");
            s
        }
        HookAction::Uninstall => {
            let s = hooks::uninstall(&root).map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("Removed Synaptic hooks.");
            s
        }
        HookAction::Status => hooks::status(&root).map_err(|e| anyhow::anyhow!("{e}"))?,
    };
    for st in &states {
        println!(
            "  {} — {}",
            st.name,
            if st.installed {
                "installed"
            } else {
                "not installed"
            }
        );
    }
    Ok(())
}
