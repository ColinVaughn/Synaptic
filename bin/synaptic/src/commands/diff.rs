//! `diff` command: time-travel graph diff between two git revisions.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

use synaptic_history::{diff, git, to_html, DiffOptions, DiffReport};

pub(crate) struct DiffArgs {
    pub rev1: Option<String>,
    pub rev2: Option<String>,
    pub since: Option<String>,
    pub root: PathBuf,
    pub directed: bool,
    pub scope: Option<String>,
    pub top: usize,
    pub module_depth: usize,
    pub json: bool,
    pub report_path: Option<PathBuf>,
    pub html_path: Option<PathBuf>,
    pub no_cache: bool,
}

pub(crate) fn run_diff(a: DiffArgs) -> Result<()> {
    let root = a
        .root
        .canonicalize()
        .with_context(|| format!("resolving {}", a.root.display()))?;

    // The base revision: a positional rev1, or one resolved from --since.
    let base = match (a.rev1, &a.since) {
        (Some(_), Some(_)) => bail!("pass either a base revision or --since, not both"),
        (Some(r), None) => r,
        (None, Some(date)) => git::rev_before(&root, date).map_err(|e| anyhow::anyhow!("{e}"))?,
        (None, None) => bail!("provide a base revision (e.g. HEAD~10) or --since <date>"),
    };

    let opts = DiffOptions {
        directed: a.directed,
        scope: a.scope,
        top: a.top,
        module_depth: a.module_depth,
        no_cache: a.no_cache,
        ..DiffOptions::default()
    };
    let report =
        diff(&root, &base, a.rev2.as_deref(), &opts).map_err(|e| anyhow::anyhow!("{e}"))?;

    if a.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_terminal(&report);
    }
    if let Some(p) = a.report_path {
        std::fs::write(&p, markdown(&report))
            .with_context(|| format!("writing {}", p.display()))?;
        println!("Wrote {}", p.display());
    }
    if let Some(p) = a.html_path {
        std::fs::write(&p, to_html(&report)).with_context(|| format!("writing {}", p.display()))?;
        println!("Wrote {}", p.display());
    }
    Ok(())
}

fn short(s: &str) -> &str {
    // Only abbreviate hex SHAs; leave labels like WORKING_TREE intact.
    if s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        &s[..8]
    } else {
        s
    }
}

fn print_terminal(r: &DiffReport) {
    println!("Diff {} -> {}", short(&r.rev1), short(&r.rev2));
    println!("  {}", r.summary);
    println!();
    println!("Added dependencies ({}):", r.added_dependencies.len());
    for d in &r.added_dependencies {
        println!("  + {} -> {}", d.from, d.to);
    }
    println!("Removed dependencies ({}):", r.removed_dependencies.len());
    for d in &r.removed_dependencies {
        println!("  - {} -> {}", d.from, d.to);
    }
    println!("Removed APIs ({}):", r.removed_apis.len());
    for a in &r.removed_apis {
        println!(
            "  - {} ({}) referenced by {}",
            a.label, a.source_file, a.referenced_by
        );
    }
    println!(
        "Architectural drift: coupling {:.3} -> {:.3}, communities {} -> {}",
        r.drift.coupling_before,
        r.drift.coupling_after,
        r.drift.communities_before,
        r.drift.communities_after
    );
    for m in &r.drift.modules {
        println!(
            "  {} {:+.3} ({:.3} -> {:.3})",
            m.module, m.delta, m.coupling_before, m.coupling_after
        );
    }
    println!("New cycles ({}):", r.new_cycles.len());
    for c in &r.new_cycles {
        println!("  * {}", c.join(" -> "));
    }
    println!("Hotspots ({}):", r.hotspots.len());
    for h in &r.hotspots {
        println!(
            "  {} (+{}/-{} lines, +{}/-{} nodes)",
            h.file, h.lines_added, h.lines_removed, h.nodes_added, h.nodes_removed
        );
    }
}

fn markdown(r: &DiffReport) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "# Synaptic diff: `{}` -> `{}`\n\n",
        r.rev1, r.rev2
    ));
    s.push_str(&format!("{}\n\n", r.summary));
    s.push_str("## Added dependencies\n\n");
    for d in &r.added_dependencies {
        s.push_str(&format!("- `{}` -> `{}`\n", d.from, d.to));
    }
    s.push_str("\n## Removed dependencies\n\n");
    for d in &r.removed_dependencies {
        s.push_str(&format!("- `{}` -> `{}`\n", d.from, d.to));
    }
    s.push_str("\n## Removed APIs\n\n");
    for a in &r.removed_apis {
        s.push_str(&format!(
            "- `{}` ({}) referenced by {}\n",
            a.label, a.source_file, a.referenced_by
        ));
    }
    s.push_str(&format!(
        "\n## Architectural drift\n\nCoupling {:.3} -> {:.3}; communities {} -> {}\n\n",
        r.drift.coupling_before,
        r.drift.coupling_after,
        r.drift.communities_before,
        r.drift.communities_after
    ));
    for m in &r.drift.modules {
        s.push_str(&format!("- `{}` {:+.3}\n", m.module, m.delta));
    }
    s.push_str("\n## New cycles\n\n");
    for c in &r.new_cycles {
        s.push_str(&format!("- {}\n", c.join(" -> ")));
    }
    s.push_str("\n## Hotspots\n\n");
    for h in &r.hotspots {
        s.push_str(&format!(
            "- `{}` (+{}/-{} lines, +{}/-{} nodes)\n",
            h.file, h.lines_added, h.lines_removed, h.nodes_added, h.nodes_removed
        ));
    }
    s
}
