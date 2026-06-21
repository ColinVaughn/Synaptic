//! Verify the graph after an agent applied a rename plan. Compares a pre-edit
//! snapshot (saved by `rename`) against a freshly rebuilt post-edit graph and
//! checks four invariants: the definition was renamed, no references were lost,
//! no nodes/unresolved-stubs regressed, and no new dependency cycle appeared.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use serde::Serialize;
use synaptic_core::{Node, NodeId};
use synaptic_graph::{find_import_cycles, norm_source_file, KnowledgeGraph};

use crate::plan::RenamePlan;
use crate::relocate::RelocatePlan;
use crate::resolve::normalize;
use crate::sites::REF_RELATIONS;
use crate::RefactorError;

/// One invariant outcome.
#[derive(Debug, Serialize)]
pub struct VerifyCheck {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

/// The full verify outcome.
#[derive(Debug, Serialize)]
pub struct VerifyReport {
    pub passed: bool,
    pub checks: Vec<VerifyCheck>,
}

/// The renamed definition node in `after`, matched by full (normalized) path +
/// new label + (when known) kind. Full-path matching avoids binding to a
/// same-named symbol in a different directory that shares a basename.
fn find_renamed<'a>(plan: &RenamePlan, after: &'a KnowledgeGraph) -> Option<&'a Node> {
    let file = norm_source_file(&plan.target.file, None);
    let new = normalize(&plan.new_name);
    let kind = plan.target.kind.as_deref();
    after.nodes().find(|n| {
        norm_source_file(&n.source_file, None) == file
            && normalize(&n.label) == new
            && match kind {
                Some(k) => n.kind().map(|nk| nk.as_str()) == Some(k),
                None => true,
            }
    })
}

/// Source files of incoming reference edges to `id` (normalized).
fn referencing_files(kg: &KnowledgeGraph, id: &NodeId) -> BTreeSet<String> {
    kg.incident_edges(id)
        .filter(|e| &e.target == id && REF_RELATIONS.contains(&e.relation.as_str()))
        .map(|e| norm_source_file(&e.source_file, None))
        .collect()
}

/// Verify `plan` against the pre-edit `before` graph by rebuilding the current
/// source under `root`.
pub fn verify_plan(
    plan: &RenamePlan,
    before: &KnowledgeGraph,
    root: &Path,
) -> Result<VerifyReport, RefactorError> {
    let outcome = synaptic_incremental::rebuild(
        &synaptic_incremental::RebuildOptions {
            root: root.to_path_buf(),
            directed: before.directed,
            force: true,
        },
        &synaptic_incremental::ChangeSet::Full,
        None,
    )
    .map_err(|e| RefactorError::Rebuild(e.to_string()))?;
    let after = outcome.kg;

    let checks = vec![
        check_renamed(plan, &after),
        check_refs_preserved(plan, before, &after),
        check_no_lost_nodes(before, &after),
        check_no_new_cycles(before, &after),
    ];
    Ok(VerifyReport {
        passed: checks.iter().all(|c| c.passed),
        checks,
    })
}

/// The old definition is gone and the new one exists, at the target's file.
fn check_renamed(plan: &RenamePlan, after: &KnowledgeGraph) -> VerifyCheck {
    let file = norm_source_file(&plan.target.file, None);
    let old = normalize(&plan.old_name);
    let kind = plan.target.kind.as_deref();
    let kind_ok = |n: &Node| match kind {
        Some(k) => n.kind().map(|nk| nk.as_str()) == Some(k),
        None => true,
    };
    let new_present = find_renamed(plan, after).is_some();
    let old_present = after.nodes().any(|n| {
        norm_source_file(&n.source_file, None) == file && kind_ok(n) && normalize(&n.label) == old
    });
    let (passed, detail) = match (new_present, old_present) {
        (true, false) => (
            true,
            format!(
                "`{}` renamed to `{}` in {}",
                plan.old_name, plan.new_name, file
            ),
        ),
        (false, _) => (
            false,
            format!("new definition `{}` not found in {}", plan.new_name, file),
        ),
        (true, true) => (
            false,
            format!(
                "old definition `{}` still present in {}",
                plan.old_name, file
            ),
        ),
    };
    VerifyCheck {
        name: "definition-renamed".to_string(),
        passed,
        detail,
    }
}

/// No references were lost: every file that referenced the old target still
/// references the renamed node. Comparing the set of referencing files (not a
/// bare edge count) is robust to resolution producing a different edge count
/// after the rename, and names exactly which references went missing.
fn check_refs_preserved(
    plan: &RenamePlan,
    before: &KnowledgeGraph,
    after: &KnowledgeGraph,
) -> VerifyCheck {
    let before_files = referencing_files(before, &NodeId(plan.target.id.clone()));
    let after_files = find_renamed(plan, after)
        .map(|n| referencing_files(after, &n.id))
        .unwrap_or_default();
    let lost: Vec<&String> = before_files.difference(&after_files).collect();
    let passed = lost.is_empty();
    VerifyCheck {
        name: "references-preserved".to_string(),
        passed,
        detail: if passed {
            format!(
                "all {} referencing file(s) still reference the renamed symbol",
                before_files.len()
            )
        } else {
            format!(
                "references lost in {} file(s): {}",
                lost.len(),
                lost.iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
    }
}

/// A rename must not drop located code nodes (an accidental deletion). Unresolved
/// references manifest as dropped edges, which `references-preserved` catches; a
/// missing node is the structural-integrity signal here.
fn check_no_lost_nodes(before: &KnowledgeGraph, after: &KnowledgeGraph) -> VerifyCheck {
    let located = |kg: &KnowledgeGraph| kg.nodes().filter(|n| !n.source_file.is_empty()).count();
    let before_n = located(before);
    let after_n = located(after);
    let passed = after_n >= before_n;
    VerifyCheck {
        name: "no-lost-nodes".to_string(),
        passed,
        detail: if passed {
            format!("no located nodes lost (before {before_n}, after {after_n})")
        } else {
            format!("located node count dropped: before {before_n}, after {after_n}")
        },
    }
}

/// Verify a move/extract: the symbol's definition now lives in the destination
/// file, references are preserved, and no new dependency cycle appeared.
pub fn verify_relocate(
    plan: &RelocatePlan,
    before: &KnowledgeGraph,
    root: &Path,
) -> Result<VerifyReport, RefactorError> {
    let outcome = synaptic_incremental::rebuild(
        &synaptic_incremental::RebuildOptions {
            root: root.to_path_buf(),
            directed: before.directed,
            force: true,
        },
        &synaptic_incremental::ChangeSet::Full,
        None,
    )
    .map_err(|e| RefactorError::Rebuild(e.to_string()))?;
    let after = outcome.kg;

    let checks = vec![
        check_relocated(plan, &after),
        check_relocate_refs(plan, before, &after),
        check_no_new_cycles(before, &after),
    ];
    Ok(VerifyReport {
        passed: checks.iter().all(|c| c.passed),
        checks,
    })
}

/// The symbol's definition node now lives in the destination file (matched by
/// full destination path + same label + kind), and no longer in its old file.
fn check_relocated(plan: &RelocatePlan, after: &KnowledgeGraph) -> VerifyCheck {
    let dest = norm_source_file(&plan.dest_file, None);
    let old_file = norm_source_file(&plan.target.file, None);
    let label = normalize(&plan.symbol);
    let kind = plan.target.kind.as_deref();
    let kind_ok = |n: &Node| match kind {
        Some(k) => n.kind().map(|nk| nk.as_str()) == Some(k),
        None => true,
    };
    let in_dest = after.nodes().any(|n| {
        norm_source_file(&n.source_file, None) == dest && kind_ok(n) && normalize(&n.label) == label
    });
    let still_in_old = after.nodes().any(|n| {
        norm_source_file(&n.source_file, None) == old_file
            && kind_ok(n)
            && normalize(&n.label) == label
    });
    let (passed, detail) = match (in_dest, still_in_old) {
        (true, false) => (true, format!("`{}` now defined in {}", plan.symbol, dest)),
        (false, _) => (
            false,
            format!("`{}` not found in destination {}", plan.symbol, dest),
        ),
        (true, true) => (
            false,
            format!("`{}` still present in its old file", plan.symbol),
        ),
    };
    VerifyCheck {
        name: "definition-relocated".to_string(),
        passed,
        detail,
    }
}

/// References preserved: every file that referenced the symbol still reaches it
/// at its new location.
fn check_relocate_refs(
    plan: &RelocatePlan,
    before: &KnowledgeGraph,
    after: &KnowledgeGraph,
) -> VerifyCheck {
    let before_files = referencing_files(before, &NodeId(plan.target.id.clone()));
    let dest = norm_source_file(&plan.dest_file, None);
    let label = normalize(&plan.symbol);
    let kind = plan.target.kind.as_deref();
    let new_node = after.nodes().find(|n| {
        norm_source_file(&n.source_file, None) == dest
            && normalize(&n.label) == label
            && match kind {
                Some(k) => n.kind().map(|nk| nk.as_str()) == Some(k),
                None => true,
            }
    });
    let after_files = new_node
        .map(|n| referencing_files(after, &n.id))
        .unwrap_or_default();
    let lost: Vec<&String> = before_files.difference(&after_files).collect();
    let passed = lost.is_empty();
    VerifyCheck {
        name: "references-preserved".to_string(),
        passed,
        detail: if passed {
            format!(
                "all {} referencing file(s) still reach the symbol",
                before_files.len()
            )
        } else {
            format!(
                "references lost in {} file(s): {}",
                lost.len(),
                lost.iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
    }
}

/// No dependency cycle exists after the rename that was absent before.
fn check_no_new_cycles(before: &KnowledgeGraph, after: &KnowledgeGraph) -> VerifyCheck {
    let key = |c: &synaptic_graph::ImportCycle| {
        let mut v = c.cycle.clone();
        v.sort();
        v.join("|")
    };
    let before_set: HashSet<String> = find_import_cycles(before, 12, 64).iter().map(key).collect();
    let new: Vec<String> = find_import_cycles(after, 12, 64)
        .iter()
        .map(key)
        .filter(|k| !before_set.contains(k))
        .collect();
    let passed = new.is_empty();
    VerifyCheck {
        name: "no-new-cycles".to_string(),
        passed,
        detail: if passed {
            "no new dependency cycle".to_string()
        } else {
            format!("{} new dependency cycle(s) introduced", new.len())
        },
    }
}
