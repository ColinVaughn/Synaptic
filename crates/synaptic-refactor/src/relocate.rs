//! Plan moving (or extracting) a symbol's definition to another module. Like
//! rename, Synaptic emits a plan for an agent to apply, then verifies the graph.
//! `move` targets an existing file; `extract` a new file. The symbol name is
//! unchanged; what changes is where it lives and the imports that reach it.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use synaptic_core::{Confidence, NodeId, Span};
use synaptic_graph::{norm_source_file, KnowledgeGraph};

use crate::plan::{BlastRadius, Collision, RenameOptions};
use crate::resolve::{self, Candidate, Selection};
use crate::sites::{self, EditSite};
use crate::RefactorError;

/// A move/extract plan. Round-trips into `verify`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelocatePlan {
    pub version: u32,
    pub operation: String, // "move" | "extract"
    pub symbol: String,
    pub target: Candidate,
    pub dest_file: String,
    pub dest_exists: bool,
    pub ambiguous_target: bool,
    pub candidates: Vec<Candidate>,
    /// The definition block to cut from the source file.
    pub def_span: Option<Span>,
    pub blast_radius: BlastRadius,
    /// One per referencing file: its import of the symbol must point at `dest_file`.
    pub import_updates: Vec<EditSite>,
    /// Resolved usages, for context (their text does not change on a move).
    pub references: Vec<EditSite>,
    pub collision: Collision,
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Plan relocating `name`'s definition to `dest_file`. `operation` is "move"
/// (existing file) or "extract" (new file); both share this machinery.
pub fn plan_relocate(
    kg: &KnowledgeGraph,
    name: &str,
    dest_file: &str,
    operation: &str,
    root: &Path,
    opts: &RenameOptions,
) -> Result<RelocatePlan, RefactorError> {
    let cands = resolve::find_candidates(kg, name);
    if cands.is_empty() {
        return Err(RefactorError::NotFound(name.to_string()));
    }
    let ambiguous = cands.len() > 1;
    let target = match resolve::select_target(&cands, opts.id.as_deref(), opts.file.as_deref()) {
        Selection::One(c) => c,
        Selection::None => return Err(RefactorError::NotFound(name.to_string())),
        Selection::Ambiguous(v) => {
            return Err(RefactorError::Ambiguous {
                name: name.to_string(),
                count: v.len(),
            })
        }
    };

    let dest = norm_source_file(dest_file, None);
    let dest_exists = root.join(&dest).exists();

    // Recover usages (name unchanged, so old==new==name); drop the definition site.
    let cache_dir = root.join("synaptic-out/cache");
    let cache = cache_dir.exists().then_some(cache_dir.as_path());
    let references: Vec<EditSite> =
        sites::recover_sites(kg, &target, name, name, ambiguous, root, cache)
            .into_iter()
            .filter(|s| s.reason != "definition")
            .collect();

    // One import-update edit per distinct referencing file. The def's own file is
    // included only when it still uses the symbol after the move (i.e. it has a
    // non-definition reference) -- in that case it too needs an import added.
    let files: BTreeSet<String> = references.iter().map(|s| s.file.clone()).collect();
    let target_repo = kg
        .node(&NodeId(target.id.clone()))
        .and_then(|n| n.repo.clone());
    let import_updates: Vec<EditSite> = files
        .into_iter()
        .map(|f| {
            let repo = references
                .iter()
                .find(|s| s.file == f)
                .and_then(|s| s.repo.clone());
            EditSite {
                file: f,
                span: None,
                line: None,
                old: name.to_string(),
                new: name.to_string(),
                confidence: Confidence::Inferred,
                reason: format!("update import of `{name}` to `{dest}`"),
                needs_review: true,
                repo,
            }
        })
        .collect();

    // Collision: a different definition named `name` already lives in dest_file.
    let dest_base = basename(&dest);
    let collision_locs: Vec<String> = cands
        .iter()
        .filter(|c| c.id != target.id && basename(&c.file) == dest_base)
        .map(|c| format!("{} ({})", c.label, c.file))
        .collect();
    let collision = Collision {
        exists: !collision_locs.is_empty(),
        severity: if collision_locs.is_empty() {
            "none"
        } else {
            "high"
        }
        .to_string(),
        locations: collision_locs,
    };

    let hits = synaptic_query::affected_nodes(
        kg,
        &NodeId(target.id.clone()),
        synaptic_query::DEFAULT_AFFECTED_RELATIONS,
        opts.depth,
    );

    let edit_count = 1 + import_updates.len(); // the def move + the import updates
    let mut radius_files: BTreeSet<&str> = import_updates.iter().map(|s| s.file.as_str()).collect();
    radius_files.insert(target.file.as_str());

    let def_span = target.span;
    let _ = target_repo; // repo is carried per-site; kept for future cross-repo verify

    Ok(RelocatePlan {
        version: 1,
        operation: operation.to_string(),
        symbol: name.to_string(),
        dest_file: dest,
        dest_exists,
        ambiguous_target: ambiguous,
        candidates: cands,
        def_span,
        blast_radius: BlastRadius {
            edit_count,
            file_count: radius_files.len(),
            affected_node_count: hits.len(),
            affected_node_ids: hits.iter().map(|h| h.node_id.0.clone()).collect(),
        },
        import_updates,
        references,
        collision,
        target,
    })
}
