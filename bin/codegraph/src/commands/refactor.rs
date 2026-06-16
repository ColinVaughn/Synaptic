//! `refactor` command: plan a safe rename, and verify the graph after edits.
//! CodeGraph never edits source; `rename` emits a plan for an AI agent to apply.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

use codegraph_graph::KnowledgeGraph;
use codegraph_refactor::{
    plan_relocate, plan_rename, verify_plan, verify_relocate, RefactorError, RelocatePlan,
    RenameOptions, RenamePlan,
};

use crate::cli::RefactorAction;
use crate::commands::common::{default_graph_path, load_graph};

pub(crate) fn run_refactor(action: RefactorAction) -> Result<()> {
    match action {
        RefactorAction::Rename {
            name,
            to,
            id,
            file,
            root,
            graph,
            out,
            min_confidence,
            no_text_scan,
            max_text_sites,
            json,
        } => run_rename(RenameArgs {
            name,
            to,
            id,
            file,
            root,
            graph,
            out,
            min_confidence,
            no_text_scan,
            max_text_sites,
            json,
        }),
        RefactorAction::Move {
            name,
            to,
            id,
            file,
            root,
            graph,
            out,
            json,
        } => run_relocate(RelocateArgs {
            name,
            to,
            operation: "move",
            id,
            file,
            root,
            graph,
            out,
            json,
        }),
        RefactorAction::Extract {
            name,
            to,
            id,
            file,
            root,
            graph,
            out,
            json,
        } => run_relocate(RelocateArgs {
            name,
            to,
            operation: "extract",
            id,
            file,
            root,
            graph,
            out,
            json,
        }),
        RefactorAction::Verify {
            plan,
            root,
            relocate,
            json,
        } => run_verify(plan, root, relocate, json),
    }
}

struct RelocateArgs {
    name: String,
    to: String,
    operation: &'static str,
    id: Option<String>,
    file: Option<String>,
    root: PathBuf,
    graph: Option<PathBuf>,
    out: Option<PathBuf>,
    json: bool,
}

fn run_relocate(a: RelocateArgs) -> Result<()> {
    let kg = load_graph(&default_graph_path(a.graph))?;
    let opts = RenameOptions {
        id: a.id,
        file: a.file,
        ..Default::default()
    };
    let plan = match plan_relocate(&kg, &a.name, &a.to, a.operation, &a.root, &opts) {
        Ok(p) => p,
        Err(RefactorError::Ambiguous { name, count }) => {
            eprintln!("`{name}` is ambiguous: {count} definitions match. Disambiguate with --id or --file:");
            for c in codegraph_refactor::resolve::find_candidates(&kg, &a.name) {
                let kind = c.kind.as_deref().unwrap_or("symbol");
                let line = c.span.map(|s| s.start_line).unwrap_or(0);
                eprintln!("  --id {}  ({kind} at {}:{})", c.id, c.file, line);
            }
            bail!("ambiguous symbol");
        }
        Err(e) => return Err(anyhow!("{e}")),
    };

    let out_dir = a
        .out
        .unwrap_or_else(|| PathBuf::from("codegraph-out/refactor"));
    let (json_path, md_path) = codegraph_refactor::emit::write_relocate_plan(&plan, &out_dir)
        .map_err(|e| anyhow!("{e}"))?;
    save_before_graph(&kg, &out_dir)?;

    if a.json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        println!(
            "{} {} -> {} [{} import update(s), {} affected]",
            plan.operation,
            plan.symbol,
            plan.dest_file,
            plan.import_updates.len(),
            plan.blast_radius.affected_node_count
        );
        if plan.collision.exists {
            println!(
                "  WARNING ({}): {} already in destination: {}",
                plan.collision.severity,
                plan.symbol,
                plan.collision.locations.join(", ")
            );
        }
        println!("  plan: {}", json_path.display());
        println!("  guide: {}", md_path.display());
        println!(
            "Apply, then: codegraph refactor verify --plan {} --relocate",
            json_path.display()
        );
    }
    Ok(())
}

struct RenameArgs {
    name: String,
    to: String,
    id: Option<String>,
    file: Option<String>,
    root: PathBuf,
    graph: Option<PathBuf>,
    out: Option<PathBuf>,
    min_confidence: f32,
    no_text_scan: bool,
    max_text_sites: usize,
    json: bool,
}

fn run_rename(a: RenameArgs) -> Result<()> {
    let RenameArgs {
        name,
        to,
        id,
        file,
        root,
        graph,
        out,
        min_confidence,
        no_text_scan,
        max_text_sites,
        json,
    } = a;
    let kg = load_graph(&default_graph_path(graph))?;

    // An explicit --id always wins (`name` is then the label to rename). Only when
    // --id is absent do we interpret `name` itself as a node id and pin it.
    let (old, opt_id) = match (&id, kg.node(&codegraph_core::NodeId(name.clone()))) {
        (Some(_), _) => (name.clone(), id),
        (None, Some(n)) => (n.label.clone(), Some(n.id.0.clone())),
        (None, None) => (name.clone(), None),
    };

    let opts = RenameOptions {
        id: opt_id,
        file,
        min_confidence,
        scan_text: !no_text_scan,
        max_text_sites,
        ..Default::default()
    };

    let plan = match plan_rename(&kg, &old, &to, &root, &opts) {
        Ok(p) => p,
        Err(RefactorError::Ambiguous { name, count }) => {
            eprintln!("`{name}` is ambiguous: {count} definitions match. Disambiguate with --id or --file:");
            for c in codegraph_refactor::resolve::find_candidates(&kg, &old) {
                let kind = c.kind.as_deref().unwrap_or("symbol");
                let line = c.span.map(|s| s.start_line).unwrap_or(0);
                eprintln!("  --id {}  ({kind} at {}:{})", c.id, c.file, line);
            }
            bail!("ambiguous symbol");
        }
        Err(e) => return Err(anyhow!("{e}")),
    };

    let out_dir = out.unwrap_or_else(|| PathBuf::from("codegraph-out/refactor"));
    let (json_path, md_path) =
        codegraph_refactor::emit::write_plan(&plan, &out_dir).map_err(|e| anyhow!("{e}"))?;
    // Snapshot the pre-edit graph next to the plan so `verify` is robust to a
    // later `extract` overwriting graph.json.
    save_before_graph(&kg, &out_dir)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        print_rename_summary(&plan, &json_path, &md_path);
    }
    Ok(())
}

fn save_before_graph(kg: &KnowledgeGraph, out_dir: &Path) -> Result<()> {
    let gd = kg.to_graph_data();
    let path = out_dir.join("before-graph.json");
    std::fs::write(&path, serde_json::to_string(&gd)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn print_rename_summary(plan: &RenamePlan, json_path: &Path, md_path: &Path) {
    println!(
        "Rename {} -> {} [{:?}, score {:.2}]",
        plan.old_name, plan.new_name, plan.overall_confidence, plan.overall_score
    );
    println!("  target: {} ({})", plan.target.label, plan.target.file);
    if plan.ambiguous_target {
        println!(
            "  note: {} definitions share `{}`; targeting the one above",
            plan.candidates.len(),
            plan.old_name
        );
    }
    if plan.collision.exists {
        println!(
            "  WARNING ({}): `{}` already exists at {}",
            plan.collision.severity,
            plan.new_name,
            plan.collision.locations.join(", ")
        );
    }
    println!(
        "  {} edit(s) across {} file(s); {} to review; {} affected node(s)",
        plan.blast_radius.edit_count,
        plan.blast_radius.file_count,
        plan.review.len(),
        plan.blast_radius.affected_node_count
    );
    println!("  plan: {}", json_path.display());
    println!("  guide: {}", md_path.display());
    println!(
        "Apply the edits, then: codegraph refactor verify --plan {}",
        json_path.display()
    );
}

fn run_verify(plan_path: PathBuf, root: PathBuf, relocate: bool, json: bool) -> Result<()> {
    let text = std::fs::read_to_string(&plan_path)
        .with_context(|| format!("reading {}", plan_path.display()))?;

    let before_path = plan_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("before-graph.json");
    let before = load_graph(&before_path).with_context(|| {
        format!(
            "loading pre-edit snapshot {} (was the plan produced by `refactor`?)",
            before_path.display()
        )
    })?;

    let (label, report) = if relocate {
        let plan: RelocatePlan =
            serde_json::from_str(&text).context("parsing relocate plan.json")?;
        let r = verify_relocate(&plan, &before, &root).map_err(|e| anyhow!("{e}"))?;
        (format!("{} {}", plan.operation, plan.symbol), r)
    } else {
        let plan: RenamePlan = serde_json::from_str(&text).context("parsing plan.json")?;
        let r = verify_plan(&plan, &before, &root).map_err(|e| anyhow!("{e}"))?;
        (format!("rename {} -> {}", plan.old_name, plan.new_name), r)
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Verify {}: {}",
            label,
            if report.passed { "PASS" } else { "FAIL" }
        );
        for c in &report.checks {
            println!(
                "  [{}] {}: {}",
                if c.passed { "PASS" } else { "FAIL" },
                c.name,
                c.detail
            );
        }
    }
    if !report.passed {
        bail!("verify failed");
    }
    Ok(())
}
