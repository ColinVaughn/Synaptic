//! `skill` command(s) split from main.rs.

use crate::cli::SkillAction;
use anyhow::{Context, Result};

pub(crate) fn run_skill(action: SkillAction) -> Result<()> {
    match action {
        SkillAction::Check => match codegraph_skillgen::check_drift() {
            Ok(()) => {
                println!("skill artifacts are in sync with expected/.");
                Ok(())
            }
            Err(problems) => {
                for p in &problems {
                    eprintln!("  drift: {p}");
                }
                anyhow::bail!("{} skill artifact(s) drifted", problems.len())
            }
        },
        SkillAction::Bless => {
            let written = codegraph_skillgen::bless().context("blessing skill artifacts")?;
            println!("Blessed {} skill artifact(s):", written.len());
            for p in &written {
                println!("  {}", p.display());
            }
            Ok(())
        }
    }
}
