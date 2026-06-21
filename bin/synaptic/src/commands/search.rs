//! `search` command: structural SYNQL queries + named architectural patterns.

use anyhow::{anyhow, bail, Context, Result};
use std::path::PathBuf;

use serde_json::{json, Value};
use synaptic_graph::KnowledgeGraph;
use synaptic_synql::{explain, patterns, run, QueryResult};

use crate::commands::common::{default_graph_path, load_scoped_graph};

pub(crate) struct SearchArgs {
    pub query: Option<String>,
    pub pattern: Option<String>,
    pub list_patterns: bool,
    pub explain: bool,
    pub save: Option<String>,
    pub saved: Option<String>,
    pub list_saved: bool,
    pub graph: Option<PathBuf>,
    pub repo: Option<String>,
    pub json: bool,
    pub limit: usize,
}

fn saved_dir() -> PathBuf {
    PathBuf::from("synaptic-out/synql")
}

/// Reject names that could escape the saved-query directory.
fn valid_saved_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn saved_path(name: &str) -> Result<PathBuf> {
    if !valid_saved_name(name) {
        bail!("invalid saved-query name '{name}' (use letters, digits, '_' or '-')");
    }
    Ok(saved_dir().join(format!("{name}.synql")))
}

pub(crate) fn run_search(a: SearchArgs) -> Result<()> {
    if a.list_patterns {
        for (name, desc) in patterns::list_patterns() {
            println!("{name:<16} {desc}");
        }
        return Ok(());
    }
    if a.list_saved {
        return list_saved();
    }

    // Resolve the query text: an inline query, or a saved one by name.
    let query_text: Option<String> = match (&a.query, &a.saved) {
        (Some(q), _) => Some(q.clone()),
        (None, Some(name)) => Some(load_saved(name)?),
        (None, None) => None,
    };

    // Save the inline query under a name, then continue to run it.
    if let Some(name) = &a.save {
        let q = query_text
            .as_ref()
            .ok_or_else(|| anyhow!("--save needs a SYNQL query to save"))?;
        save_query(name, q)?;
    }

    if a.explain {
        let q = query_text
            .as_ref()
            .ok_or_else(|| anyhow!("--explain needs a SYNQL query"))?;
        println!("{}", explain(q).map_err(|e| anyhow!("{e}"))?);
        return Ok(());
    }

    let kg = load_scoped_graph(&default_graph_path(a.graph), a.repo.as_deref())?;

    let mut result = if let Some(p) = a.pattern {
        patterns::run_pattern(&kg, &p).map_err(|e| anyhow!("{e}"))?
    } else if let Some(q) = &query_text {
        run(&kg, q).map_err(|e| anyhow!("{e}"))?
    } else {
        bail!("provide a SYNQL query, --saved <name>, or --pattern <name> (see --list-patterns)");
    };
    // A CLI-level safety cap on top of any LIMIT in the query.
    result.rows.truncate(a.limit);
    if let Some(agg) = result.aggregates.as_mut() {
        agg.truncate(a.limit);
    }

    if a.json {
        println!("{}", serde_json::to_string_pretty(&to_json(&kg, &result))?);
    } else {
        print_table(&kg, &result);
    }
    Ok(())
}

fn save_query(name: &str, query: &str) -> Result<()> {
    let path = saved_path(name)?;
    std::fs::create_dir_all(saved_dir())?;
    std::fs::write(&path, query).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("Saved query '{name}' to {}", path.display());
    Ok(())
}

fn load_saved(name: &str) -> Result<String> {
    let path = saved_path(name)?;
    std::fs::read_to_string(&path)
        .with_context(|| format!("reading saved query '{name}' ({})", path.display()))
}

fn list_saved() -> Result<()> {
    let dir = saved_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        println!("(no saved queries in {})", dir.display());
        return Ok(());
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("synql") {
                p.file_stem().map(|s| s.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect();
    names.sort();
    for n in names {
        println!("{n}");
    }
    Ok(())
}

fn print_aggregates(r: &QueryResult, agg: &[Vec<String>]) {
    println!("{} group(s) [{}]", agg.len(), r.columns.join(", "));
    for row in agg {
        println!("  {}", row.join("  |  "));
    }
}

fn node_json(kg: &KnowledgeGraph, id: &synaptic_core::NodeId) -> Value {
    match kg.node(id) {
        Some(n) => json!({
            "id": n.id.0,
            "label": n.label,
            "kind": n.kind().map(|k| k.as_str()),
            "visibility": n.visibility().map(|v| v.as_str()),
            "file": n.source_file,
            "location": n.source_location,
            "loc": n.loc(),
        }),
        None => json!({ "id": id.0 }),
    }
}

fn to_json(kg: &KnowledgeGraph, r: &QueryResult) -> Value {
    if let Some(agg) = &r.aggregates {
        let rows: Vec<Value> = agg
            .iter()
            .map(|row| {
                let obj: serde_json::Map<String, Value> = r
                    .columns
                    .iter()
                    .zip(row.iter())
                    .map(|(col, cell)| (col.clone(), json!(cell)))
                    .collect();
                Value::Object(obj)
            })
            .collect();
        return json!({ "columns": r.columns, "count": agg.len(), "groups": rows });
    }
    let rows: Vec<Value> = r
        .rows
        .iter()
        .map(|row| {
            let obj: serde_json::Map<String, Value> = r
                .columns
                .iter()
                .zip(row.iter())
                .map(|(col, id)| (col.clone(), node_json(kg, id)))
                .collect();
            Value::Object(obj)
        })
        .collect();
    json!({ "columns": r.columns, "count": r.rows.len(), "rows": rows })
}

fn print_table(kg: &KnowledgeGraph, r: &QueryResult) {
    if let Some(agg) = &r.aggregates {
        print_aggregates(r, agg);
        return;
    }
    println!("{} result(s) [{}]", r.rows.len(), r.columns.join(", "));
    for row in &r.rows {
        let cells: Vec<String> = r
            .columns
            .iter()
            .zip(row.iter())
            .map(|(col, id)| {
                let n = kg.node(id);
                let label = n.map(|n| n.label.as_str()).unwrap_or(&id.0);
                let kind = n.and_then(|n| n.kind()).map(|k| k.as_str()).unwrap_or("-");
                let vis = n
                    .and_then(|n| n.visibility())
                    .map(|v| v.as_str())
                    .unwrap_or("-");
                let file = n.map(|n| n.source_file.as_str()).unwrap_or("");
                let loc = n
                    .and_then(|n| n.source_location.clone())
                    .unwrap_or_default();
                format!("{col}={label} [{kind}/{vis}] {file}:{loc}")
            })
            .collect();
        println!("  {}", cells.join("  |  "));
    }
}
