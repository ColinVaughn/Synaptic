//! `global` command(s) split from main.rs.

use crate::cli::GlobalAction;
use anyhow::Result;

pub(crate) fn run_global(action: GlobalAction) -> Result<()> {
    use codegraph_workspace::global::{AddOutcome, GlobalStore};
    let store = GlobalStore::at(GlobalStore::default_dir());
    match action {
        GlobalAction::Add { graph, tag } => {
            let tag = tag.unwrap_or_else(|| codegraph_workspace::merge_graphs::tag_for(&graph));
            match store
                .add(&graph, &tag)
                .map_err(|e| anyhow::anyhow!("{e}"))?
            {
                AddOutcome::Added { tag, nodes_added } => println!(
                    "Added {tag} (+{nodes_added} nodes) → {}",
                    store.graph_path().display()
                ),
                AddOutcome::Skipped { tag } => println!("Skipped {tag} (source unchanged)."),
            }
            Ok(())
        }
        GlobalAction::Remove { tag } => {
            let n = store.remove(&tag).map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("Removed {tag} ({n} nodes).");
            Ok(())
        }
        GlobalAction::List => {
            let repos = store.list();
            if repos.is_empty() {
                println!("Global store is empty.");
            }
            for (tag, e) in &repos {
                println!(
                    "  {tag} — {} nodes, {} edges ({})",
                    e.node_count, e.edge_count, e.source_path
                );
            }
            Ok(())
        }
        GlobalAction::Path => {
            println!("{}", store.graph_path().display());
            Ok(())
        }
    }
}
