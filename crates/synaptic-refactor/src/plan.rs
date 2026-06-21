//! Assemble a rename plan: resolve the target, recover edit sites, score
//! confidence, detect a name collision, and compute the blast radius.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use synaptic_core::{Confidence, NodeId};
use synaptic_graph::KnowledgeGraph;

use crate::resolve::{self, Candidate, Selection};
use crate::sites::{self, EditSite};
use crate::RefactorError;

/// Options for planning a rename.
#[derive(Debug, Clone)]
pub struct RenameOptions {
    /// Disambiguate by exact node id.
    pub id: Option<String>,
    /// Disambiguate by file-path substring.
    pub file: Option<String>,
    /// Minimum per-site confidence score [0,1] to land in `edits` (else `review`).
    pub min_confidence: f32,
    /// Max reverse-reachability depth for the blast radius.
    pub depth: usize,
    /// Also enumerate whole-word textual occurrences (type uses the graph does not
    /// record as edges). These land in `review` for the agent to confirm.
    pub scan_text: bool,
    /// Cap on textual occurrences when `scan_text` is on.
    pub max_text_sites: usize,
}

impl Default for RenameOptions {
    fn default() -> Self {
        RenameOptions {
            id: None,
            file: None,
            min_confidence: 0.8,
            depth: 6,
            scan_text: true,
            max_text_sites: 200,
        }
    }
}

/// Transitive impact of the target, for context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlastRadius {
    pub edit_count: usize,
    pub file_count: usize,
    pub affected_node_count: usize,
    pub affected_node_ids: Vec<String>,
}

/// Whether the new name already exists, and how severe that is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collision {
    pub exists: bool,
    pub severity: String,
    pub locations: Vec<String>,
}

/// A complete, serializable rename plan. Round-trips into `verify`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenamePlan {
    pub version: u32,
    pub operation: String,
    pub old_name: String,
    pub new_name: String,
    pub target: Candidate,
    pub ambiguous_target: bool,
    pub candidates: Vec<Candidate>,
    pub overall_confidence: Confidence,
    pub overall_score: f32,
    pub blast_radius: BlastRadius,
    pub edits: Vec<EditSite>,
    pub review: Vec<EditSite>,
    pub collision: Collision,
}

/// Aggregate per-site confidence into an overall level + numeric score.
fn aggregate(
    edits: &[EditSite],
    review: &[EditSite],
    ambiguous: bool,
    collision: bool,
) -> (Confidence, f32) {
    let all: Vec<&EditSite> = edits.iter().chain(review.iter()).collect();
    let score = if all.is_empty() {
        0.0
    } else {
        all.iter()
            .map(|s| s.confidence.default_score())
            .sum::<f32>()
            / all.len() as f32
    };
    let any_ambiguous = all.iter().any(|s| s.confidence == Confidence::Ambiguous);
    let any_soft = all
        .iter()
        .any(|s| s.confidence == Confidence::Inferred || s.needs_review);
    // If the threshold forced every site to review, the headline must not claim
    // a clean Extracted rename.
    let all_routed_to_review = edits.is_empty() && !review.is_empty();
    let level = if ambiguous || collision || any_ambiguous {
        Confidence::Ambiguous
    } else if any_soft || all_routed_to_review {
        Confidence::Inferred
    } else {
        Confidence::Extracted
    };
    (level, score)
}

/// Plan renaming `old` to `new` in the graph rooted at `root`. Reads referencing
/// files from disk to recover column-accurate call sites; never edits anything.
pub fn plan_rename(
    kg: &KnowledgeGraph,
    old: &str,
    new: &str,
    root: &Path,
    opts: &RenameOptions,
) -> Result<RenamePlan, RefactorError> {
    let cands = resolve::find_candidates(kg, old);
    if cands.is_empty() {
        return Err(RefactorError::NotFound(old.to_string()));
    }
    let name_ambiguous = cands.len() > 1;
    let target = match resolve::select_target(&cands, opts.id.as_deref(), opts.file.as_deref()) {
        Selection::One(c) => c,
        Selection::None => return Err(RefactorError::NotFound(old.to_string())),
        Selection::Ambiguous(v) => {
            return Err(RefactorError::Ambiguous {
                name: old.to_string(),
                count: v.len(),
            })
        }
    };

    let cache_dir = root.join("synaptic-out/cache");
    let cache = if cache_dir.exists() {
        Some(cache_dir.as_path())
    } else {
        None
    };
    let mut sites = sites::recover_sites(kg, &target, old, new, name_ambiguous, root, cache);

    // Textual fallback for references the graph does not record as edges (type
    // uses, enum-variant paths). Add a textual site only where no resolved site
    // already covers the same (file, line, start_col).
    if opts.scan_text {
        let covered: BTreeSet<(String, Option<u32>, Option<u32>)> = sites
            .iter()
            .map(|s| (s.file.clone(), s.line, s.span.map(|sp| sp.start_col)))
            .collect();
        let textual = sites::text_scan(kg, old, new, name_ambiguous, root, opts.max_text_sites);
        for t in textual {
            let key = (t.file.clone(), t.line, t.span.map(|sp| sp.start_col));
            if !covered.contains(&key) {
                sites.push(t);
            }
        }
    }

    // Collision: does the new name already resolve to a definition?
    let new_cands = resolve::find_candidates(kg, new);
    let collision = Collision {
        exists: !new_cands.is_empty(),
        severity: if new_cands.iter().any(|c| c.file == target.file) {
            "high"
        } else if new_cands.is_empty() {
            "none"
        } else {
            "medium"
        }
        .to_string(),
        locations: new_cands
            .iter()
            .map(|c| format!("{} ({})", c.label, c.file))
            .collect(),
    };

    // Module-level importers: files that import this symbol's module through a stub
    // edge the symbol-level walk cannot reach (e.g. a test that imports and calls
    // it at top level). Add a review site for any such file not already covered, so
    // a rename never silently drops a module-level user. The token's exact span is
    // unknown here, so it is flagged for the agent to locate.
    let target_node_id = NodeId(target.id.clone());
    let importers = synaptic_query::module_importers(kg, &target_node_id);
    {
        let covered_files: BTreeSet<String> = sites.iter().map(|s| s.file.clone()).collect();
        for mi in &importers {
            let Some(n) = kg.node(&mi.node_id) else {
                continue;
            };
            let file = synaptic_graph::norm_source_file(&n.source_file, None);
            if file.is_empty() || covered_files.contains(&file) {
                continue;
            }
            sites.push(EditSite {
                file,
                span: None,
                line: None,
                old: old.to_string(),
                new: new.to_string(),
                confidence: Confidence::Inferred,
                reason: "module-level import of the symbol; locate and rename the reference"
                    .to_string(),
                needs_review: true,
                repo: n.repo.clone(),
            });
        }
    }

    // Blast radius: reverse-reachable affected nodes, plus the module importers the
    // symbol-level walk misses.
    let hits = synaptic_query::affected_nodes(
        kg,
        &target_node_id,
        synaptic_query::DEFAULT_AFFECTED_RELATIONS,
        opts.depth,
    );
    let mut affected_ids: Vec<String> = hits.iter().map(|h| h.node_id.0.clone()).collect();
    {
        let mut seen: BTreeSet<String> = affected_ids.iter().cloned().collect();
        for mi in &importers {
            if seen.insert(mi.node_id.0.clone()) {
                affected_ids.push(mi.node_id.0.clone());
            }
        }
    }

    // Route each site: confident + no review flag -> edits; else review.
    // Guard against a NaN/out-of-range --min-confidence.
    let min = if opts.min_confidence.is_finite() {
        opts.min_confidence.clamp(0.0, 1.0)
    } else {
        RenameOptions::default().min_confidence
    };
    let (edits, review): (Vec<EditSite>, Vec<EditSite>) = sites
        .into_iter()
        .partition(|s| !s.needs_review && s.confidence.default_score() >= min);

    let files: BTreeSet<&str> = edits
        .iter()
        .chain(review.iter())
        .map(|s| s.file.as_str())
        .collect();
    let (overall_confidence, overall_score) =
        aggregate(&edits, &review, name_ambiguous, collision.exists);

    Ok(RenamePlan {
        version: 1,
        operation: "rename".to_string(),
        old_name: old.to_string(),
        new_name: new.to_string(),
        target,
        ambiguous_target: name_ambiguous,
        candidates: cands,
        overall_confidence,
        overall_score,
        blast_radius: BlastRadius {
            edit_count: edits.len() + review.len(),
            file_count: files.len(),
            affected_node_count: affected_ids.len(),
            affected_node_ids: affected_ids,
        },
        edits,
        review,
        collision,
    })
}
