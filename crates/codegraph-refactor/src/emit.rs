//! Emit the rename plan as `plan.json` (machine-readable) and `plan.md` (an
//! agent-readable narrative). The markdown is written for an AI agent to execute
//! step by step: definition first, references grouped by file, review list last.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::plan::RenamePlan;
use crate::sites::EditSite;
use crate::RefactorError;

/// Write `plan.json` + `plan.md` into `out_dir`, returning their paths.
pub fn write_plan(plan: &RenamePlan, out_dir: &Path) -> Result<(PathBuf, PathBuf), RefactorError> {
    std::fs::create_dir_all(out_dir)?;
    let json_path = out_dir.join("plan.json");
    let md_path = out_dir.join("plan.md");
    std::fs::write(&json_path, serde_json::to_string_pretty(plan)?)?;
    std::fs::write(&md_path, render_md(plan, &json_path))?;
    Ok((json_path, md_path))
}

fn loc_str(s: &EditSite) -> String {
    match (s.span, s.line) {
        (Some(sp), _) => format!("{}:{}:{}", s.file, sp.start_line, sp.start_col),
        (None, Some(l)) => format!("{}:{}", s.file, l),
        (None, None) => s.file.clone(),
    }
}

fn repo_str(s: &EditSite) -> String {
    match &s.repo {
        Some(r) => format!(" [repo: {r}]"),
        None => String::new(),
    }
}

/// A rename edit line: the symbol's text changes, so show `old -> new`.
fn site_line(s: &EditSite) -> String {
    format!(
        "- `{}` -- rename `{}` -> `{}` ({}, {}){}",
        loc_str(s),
        s.old,
        s.new,
        s.reason,
        crate::confidence_str(s.confidence),
        repo_str(s)
    )
}

/// A relocate (move/extract) line: the symbol name is unchanged, so a `rename`
/// arrow would read as `X -> X`. The action is in the reason (e.g. "update import
/// of `X` to ...") or it is a usage that needs no text change at all.
fn relocate_site_line(s: &EditSite) -> String {
    format!(
        "- `{}` -- {} ({}){}",
        loc_str(s),
        s.reason,
        crate::confidence_str(s.confidence),
        repo_str(s)
    )
}

/// Render the agent-facing markdown narrative. `plan_json_path` is the location
/// the plan.json was written to, so the verify command points at the real file.
pub fn render_md(plan: &RenamePlan, plan_json_path: &Path) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "# Rename plan: {} -> {}", plan.old_name, plan.new_name);
    let _ = writeln!(o);
    let _ = writeln!(
        o,
        "Overall confidence: {} (score {:.2}). {} edit(s) across {} file(s); {} need review.",
        crate::confidence_str(plan.overall_confidence),
        plan.overall_score,
        plan.blast_radius.edit_count,
        plan.blast_radius.file_count,
        plan.review.len()
    );
    let _ = writeln!(
        o,
        "Transitive blast radius: {} affected node(s).",
        plan.blast_radius.affected_node_count
    );
    let _ = writeln!(o);
    let _ = writeln!(
        o,
        "CodeGraph does not edit source. Apply the edits below, then run the verify command at the end."
    );
    let _ = writeln!(o);

    // Target.
    let _ = writeln!(o, "## Target");
    let kind = plan.target.kind.as_deref().unwrap_or("symbol");
    let line = plan.target.span.map(|s| s.start_line).unwrap_or(0);
    let _ = writeln!(
        o,
        "{} `{}` at `{}:{}`.",
        kind, plan.target.label, plan.target.file, line
    );
    if plan.ambiguous_target {
        let _ = writeln!(o);
        let _ = writeln!(
            o,
            "NOTE: `{}` matches {} definitions; this plan targets the one above. Other definitions:",
            plan.old_name,
            plan.candidates.len()
        );
        for c in &plan.candidates {
            if c.id != plan.target.id {
                let _ = writeln!(o, "- `{}` ({})", c.label, c.file);
            }
        }
    }
    let _ = writeln!(o);

    // Collision.
    if plan.collision.exists {
        let _ = writeln!(o, "## Collision");
        let _ = writeln!(
            o,
            "WARNING ({}): `{}` already exists. Renaming into it may merge or shadow symbols:",
            plan.collision.severity, plan.new_name
        );
        for loc in &plan.collision.locations {
            let _ = writeln!(o, "- {}", loc);
        }
        let _ = writeln!(o);
    }

    // Edits, grouped by file with the definition file first.
    let _ = writeln!(o, "## Edits (apply these)");
    if plan.edits.is_empty() {
        let _ = writeln!(o, "(none above the confidence threshold; see Review)");
    } else {
        let mut files: Vec<&str> = plan.edits.iter().map(|s| s.file.as_str()).collect();
        files.sort();
        files.dedup();
        // Definition file first.
        files.sort_by_key(|f| (*f != plan.target.file, *f));
        for f in files {
            let _ = writeln!(o, "\n`{}`:", f);
            for s in plan.edits.iter().filter(|s| s.file == f) {
                let _ = writeln!(o, "{}", site_line(s));
            }
        }
    }
    let _ = writeln!(o);

    // Review.
    if !plan.review.is_empty() {
        let _ = writeln!(o, "## Review (verify before applying)");
        let _ = writeln!(
            o,
            "Lower confidence or ambiguous: confirm each is really `{}` before renaming.",
            plan.old_name
        );
        for s in &plan.review {
            let why = if s.span.is_none() {
                "column unknown; locate the token on this line"
            } else if s.confidence == codegraph_core::Confidence::Ambiguous {
                "ambiguous resolution"
            } else {
                "lower confidence"
            };
            let _ = writeln!(o, "{} -- {}", site_line(s), why);
        }
        let _ = writeln!(o);
    }

    // Verify.
    let _ = writeln!(o, "## Verify");
    // Note cross-repo sites: verify rebuilds a single repo in v1.
    let def_repo = plan
        .edits
        .iter()
        .chain(plan.review.iter())
        .find(|s| s.reason == "definition")
        .and_then(|s| s.repo.clone());
    let cross_repo = plan
        .edits
        .iter()
        .chain(plan.review.iter())
        .any(|s| s.repo.is_some() && s.repo != def_repo);
    if cross_repo {
        let _ = writeln!(
            o,
            "NOTE: this rename spans multiple repos (see [repo: ...] tags). `verify` rebuilds a single repo; verify each member separately.\n"
        );
    }
    let _ = writeln!(
        o,
        "After applying the edits, run:\n\n    codegraph refactor verify --plan {}",
        plan_json_path.display()
    );
    o
}

/// Write a relocate plan's `plan.json` + `plan.md` into `out_dir`.
pub fn write_relocate_plan(
    plan: &crate::relocate::RelocatePlan,
    out_dir: &Path,
) -> Result<(PathBuf, PathBuf), RefactorError> {
    std::fs::create_dir_all(out_dir)?;
    let json_path = out_dir.join("plan.json");
    let md_path = out_dir.join("plan.md");
    std::fs::write(&json_path, serde_json::to_string_pretty(plan)?)?;
    std::fs::write(&md_path, render_relocate_md(plan, &json_path))?;
    Ok((json_path, md_path))
}

/// Render the agent-facing markdown for a move/extract.
pub fn render_relocate_md(plan: &crate::relocate::RelocatePlan, plan_json_path: &Path) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "# {} plan: `{}` -> `{}`",
        plan.operation, plan.symbol, plan.dest_file
    );
    let _ = writeln!(o);
    let _ = writeln!(
        o,
        "Move the definition of `{}` to `{}` ({}). {} import update(s); {} affected node(s).",
        plan.symbol,
        plan.dest_file,
        if plan.dest_exists {
            "existing file"
        } else {
            "new file"
        },
        plan.import_updates.len(),
        plan.blast_radius.affected_node_count
    );
    let _ = writeln!(o);
    let _ = writeln!(
        o,
        "CodeGraph does not edit source. Apply the steps below, then run verify."
    );
    let _ = writeln!(o);

    // Step 1: move the definition.
    let line = plan.def_span.map(|s| s.start_line).unwrap_or(0);
    let _ = writeln!(o, "## 1. Move the definition");
    let _ = writeln!(
        o,
        "Cut `{}` from `{}:{}` and paste it into `{}` (create it if missing).",
        plan.symbol, plan.target.file, line, plan.dest_file
    );
    if plan.ambiguous_target {
        let _ = writeln!(
            o,
            "\nNOTE: `{}` matches {} definitions; this plan targets the one in `{}`.",
            plan.symbol,
            plan.candidates.len(),
            plan.target.file
        );
    }
    let _ = writeln!(o);

    // Collision.
    if plan.collision.exists {
        let _ = writeln!(o, "## Collision");
        let _ = writeln!(
            o,
            "WARNING ({}): `{}` already exists in the destination: {}",
            plan.collision.severity,
            plan.symbol,
            plan.collision.locations.join(", ")
        );
        let _ = writeln!(o);
    }

    // Step 2: update imports.
    let _ = writeln!(o, "## 2. Update imports");
    if plan.import_updates.is_empty() {
        let _ = writeln!(o, "(no referencing files import the symbol)");
    } else {
        for s in &plan.import_updates {
            let _ = writeln!(o, "{}", relocate_site_line(s));
        }
    }
    let _ = writeln!(o);

    // References for context.
    if !plan.references.is_empty() {
        let _ = writeln!(
            o,
            "## References (no text change; confirm they still resolve)"
        );
        for s in &plan.references {
            let _ = writeln!(o, "{}", relocate_site_line(s));
        }
        let _ = writeln!(o);
    }

    let _ = writeln!(o, "## Verify");
    let _ = writeln!(
        o,
        "After applying, run:\n\n    codegraph refactor verify --plan {} --relocate",
        plan_json_path.display()
    );
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{BlastRadius, Collision};
    use crate::resolve::Candidate;
    use codegraph_core::{Confidence, Span};

    fn sample_plan() -> RenamePlan {
        RenamePlan {
            version: 1,
            operation: "rename".into(),
            old_name: "User".into(),
            new_name: "Account".into(),
            target: Candidate {
                id: "models::User".into(),
                label: "User".into(),
                kind: Some("class".into()),
                visibility: Some("public".into()),
                file: "models.py".into(),
                span: Some(Span {
                    start_line: 1,
                    start_col: 1,
                    end_line: 5,
                    end_col: 2,
                }),
            },
            ambiguous_target: true,
            candidates: vec![],
            overall_confidence: Confidence::Inferred,
            overall_score: 0.75,
            blast_radius: BlastRadius {
                edit_count: 2,
                file_count: 2,
                affected_node_count: 3,
                affected_node_ids: vec![],
            },
            edits: vec![EditSite {
                file: "models.py".into(),
                span: Some(Span {
                    start_line: 1,
                    start_col: 7,
                    end_line: 1,
                    end_col: 11,
                }),
                line: Some(1),
                old: "User".into(),
                new: "Account".into(),
                confidence: Confidence::Extracted,
                reason: "definition".into(),
                needs_review: false,
                repo: None,
            }],
            review: vec![EditSite {
                file: "service.py".into(),
                span: None,
                line: Some(9),
                old: "User".into(),
                new: "Account".into(),
                confidence: Confidence::Inferred,
                reason: "call site".into(),
                needs_review: true,
                repo: None,
            }],
            collision: Collision {
                exists: false,
                severity: "none".into(),
                locations: vec![],
            },
        }
    }

    #[test]
    fn writes_both_artifacts_and_json_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let plan = sample_plan();
        let (json, md) = write_plan(&plan, dir.path()).unwrap();
        assert!(json.exists() && md.exists());
        let back: RenamePlan =
            serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        assert_eq!(back.new_name, "Account");
        assert_eq!(back.edits.len(), 1);
    }

    #[test]
    fn markdown_has_sections_and_verify_command() {
        let md = render_md(&sample_plan(), Path::new("custom/out/plan.json"));
        assert!(md.contains("# Rename plan: User -> Account"));
        assert!(md.contains("## Target"));
        assert!(md.contains("## Edits (apply these)"));
        assert!(md.contains("## Review"));
        // verify command points at the real (custom) plan path, not a default
        assert!(md.contains("codegraph refactor verify --plan custom/out/plan.json"));
        // ambiguity note present
        assert!(md.contains("matches"));
    }

    fn sample_relocate_plan() -> crate::relocate::RelocatePlan {
        let edit = |file: &str, reason: String| EditSite {
            file: file.into(),
            span: None,
            line: Some(2),
            // name unchanged on a move: old == new
            old: "getCached".into(),
            new: "getCached".into(),
            confidence: Confidence::Inferred,
            reason,
            needs_review: true,
            repo: None,
        };
        crate::relocate::RelocatePlan {
            version: 1,
            operation: "extract".into(),
            symbol: "getCached".into(),
            target: Candidate {
                id: "lib::cache::getCached".into(),
                label: "getCached()".into(),
                kind: Some("function".into()),
                visibility: None,
                file: "src/lib/cache.ts".into(),
                span: Some(Span {
                    start_line: 19,
                    start_col: 17,
                    end_line: 19,
                    end_col: 26,
                }),
            },
            dest_file: "src/lib/cacheGet.ts".into(),
            dest_exists: false,
            ambiguous_target: false,
            candidates: vec![],
            def_span: Some(Span {
                start_line: 19,
                start_col: 17,
                end_line: 27,
                end_col: 2,
            }),
            blast_radius: BlastRadius {
                edit_count: 2,
                file_count: 2,
                affected_node_count: 4,
                affected_node_ids: vec![],
            },
            import_updates: vec![edit(
                "src/lib/blogService.ts",
                "update import of `getCached` to `src/lib/cacheGet.ts`".into(),
            )],
            references: vec![EditSite {
                file: "src/lib/cache.ts".into(),
                span: Some(Span {
                    start_line: 72,
                    start_col: 18,
                    end_line: 72,
                    end_col: 27,
                }),
                line: Some(72),
                old: "getCached".into(),
                new: "getCached".into(),
                confidence: Confidence::Extracted,
                reason: "call site".into(),
                needs_review: false,
                repo: None,
            }],
            collision: Collision {
                exists: false,
                severity: "none".into(),
                locations: vec![],
            },
        }
    }

    #[test]
    fn relocate_markdown_omits_the_rename_arrow() {
        let md = render_relocate_md(&sample_relocate_plan(), Path::new("out/plan.json"));
        // A move/extract leaves the name unchanged, so it must not read "X -> X".
        assert!(
            !md.contains("rename `getCached` -> `getCached`"),
            "relocate output must not render a no-op rename arrow:\n{md}"
        );
        // The real action is carried by the reason text.
        assert!(md.contains("update import of `getCached` to `src/lib/cacheGet.ts`"));
        // The reference line still appears (for context), just without the arrow.
        assert!(md.contains("`src/lib/cache.ts:72:18` -- call site (EXTRACTED)"));
        assert!(md.contains("codegraph refactor verify --plan out/plan.json --relocate"));
    }
}
