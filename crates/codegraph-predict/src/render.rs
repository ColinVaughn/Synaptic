//! Render a `ChangeForecast` as agent-readable Markdown.

use crate::forecast::ChangeForecast;

/// Render the forecast as Markdown (the `forecast.md` an agent reads). Sections
/// with no content are omitted so the document stays scannable; the header,
/// summary, and base line are always present.
pub fn render_markdown(f: &ChangeForecast) -> String {
    let mut s = String::new();
    s.push_str("# Change forecast\n\n");
    s.push_str(&f.summary);
    s.push_str("\n\n");
    let base = f.base.as_deref().unwrap_or("working tree");
    s.push_str(&format!("Base: `{base}`\n"));

    if let Some(r) = &f.risk {
        s.push_str(&format!(
            "\n## Change risk: {} ({}/100)\n",
            r.level, r.score
        ));
        if !r.factors.is_empty() {
            s.push_str("\nDrivers:\n");
            for factor in &r.factors {
                s.push_str(&format!("- {factor}\n"));
            }
        }
    }

    if !f.changed_files.is_empty() {
        s.push_str(&format!(
            "\n## Changed files ({})\n\n",
            f.changed_files.len()
        ));
        for file in &f.changed_files {
            s.push_str(&format!("- `{file}`\n"));
        }
    }

    if !f.changed_nodes.is_empty() {
        s.push_str(&format!(
            "\n## Changed nodes ({})\n\n",
            f.changed_nodes.len()
        ));
        for n in &f.changed_nodes {
            s.push_str(&format!("- `{}`{} - `{}`\n", n.label, meta(n), n.file));
        }
    }

    if !f.public_api_breaks.is_empty() {
        s.push_str(&format!(
            "\n## Public API at risk ({})\n\nEditing these public symbols can break callers outside their file or module.\n\n",
            f.public_api_breaks.len()
        ));
        for n in &f.public_api_breaks {
            s.push_str(&format!("- `{}` - `{}`\n", n.label, n.file));
        }
    }

    if !f.at_risk_tests.is_empty() {
        s.push_str(&format!(
            "\n## Tests at risk ({})\n\nThese tests exercise the changed code; run them before and after the change.\n\n",
            f.at_risk_tests.len()
        ));
        for h in &f.at_risk_tests {
            s.push_str(&format!(
                "- [{}h via {}] `{}` - `{}`\n",
                h.depth, h.via_relation, h.label, h.file
            ));
        }
    }

    let shown = f.blast_radius.len();
    if f.blast_radius_total > shown {
        s.push_str(&format!(
            "\n## Blast radius ({shown} of {} dependents shown)\n\n",
            f.blast_radius_total
        ));
    } else {
        s.push_str(&format!(
            "\n## Blast radius ({shown} at-risk dependent(s))\n\n"
        ));
    }
    if f.blast_radius.is_empty() {
        s.push_str("No downstream dependents in the graph.\n");
    } else {
        for h in &f.blast_radius {
            s.push_str(&format!(
                "- [{}h via {}] `{}` - `{}`\n",
                h.depth, h.via_relation, h.label, h.file
            ));
        }
    }

    if !f.new_cycles.is_empty() {
        s.push_str(&format!(
            "\n## New import cycles ({})\n\n",
            f.new_cycles.len()
        ));
        for cycle in &f.new_cycles {
            s.push_str(&format!("- {}\n", cycle.join(" -> ")));
        }
    }

    if !f.removed_apis.is_empty() {
        s.push_str(&format!(
            "\n## Removed public APIs ({})\n\n",
            f.removed_apis.len()
        ));
        for api in &f.removed_apis {
            s.push_str(&format!("- {api}\n"));
        }
    }

    let delta = &f.dependency_delta;
    if !delta.added.is_empty() || !delta.removed.is_empty() {
        s.push_str("\n## Dependency delta\n\n");
        for d in &delta.added {
            s.push_str(&format!("- + `{}` -> `{}`\n", d.from, d.to));
        }
        for d in &delta.removed {
            s.push_str(&format!("- - `{}` -> `{}`\n", d.from, d.to));
        }
    }

    if !f.co_change_suggestions.is_empty() {
        s.push_str(&format!(
            "\n## Co-change suggestions ({})\n\nFiles that historically change together with the changed files; consider whether they need updating too.\n\n",
            f.co_change_suggestions.len()
        ));
        for c in &f.co_change_suggestions {
            s.push_str(&format!(
                "- `{}` ({}% confidence, {} commits)\n",
                c.file, c.confidence_pct, c.support
            ));
        }
    }

    if !f.verify_checklist.is_empty() {
        s.push_str("\n## Verify checklist\n\n");
        for step in &f.verify_checklist {
            s.push_str(&format!(
                "- [ ] {}\n      `{}`\n",
                step.description, step.command
            ));
        }
    }

    s
}

/// Render an analytic `EditForecast` (the `--edit` mode of `predict`) as Markdown.
pub fn render_edit_markdown(f: &crate::EditForecast) -> String {
    let mut s = String::new();
    s.push_str("# Edit forecast\n\n");
    s.push_str(&f.summary);
    s.push_str("\n\n");
    s.push_str(&format!("- symbol: `{}`\n", f.symbol));
    s.push_str(&format!("- edit: {}\n", f.kind));
    s.push_str(&format!("- defined in: `{}`\n", f.target_file));
    s.push_str(&format!(
        "- removes the node: {}\n",
        if f.removes_node { "yes" } else { "no" }
    ));
    if f.removes_node {
        s.push_str(&format!("- edges severed: {}\n", f.severed_edges));
    }
    if f.removed_public_api {
        s.push_str("- removes a public API from external view\n");
    }

    s.push_str(&format!("\n## Will break ({})\n\n", f.breaks.len()));
    if f.breaks.is_empty() {
        s.push_str("- none\n");
    } else {
        for d in &f.breaks {
            s.push_str(&format!(
                "- `{}` - `{}` ({}, {})\n",
                d.label, d.file, d.via_relation, d.reason
            ));
        }
    }

    s.push_str(&format!("\n## To review ({})\n\n", f.review.len()));
    if f.review.is_empty() {
        s.push_str("- none\n");
    } else {
        for d in &f.review {
            s.push_str(&format!(
                "- `{}` - `{}` ({}, {})\n",
                d.label, d.file, d.via_relation, d.reason
            ));
        }
    }
    s
}

/// A `(kind, visibility)` parenthetical for a changed-node line, omitted when
/// neither is known.
fn meta(n: &crate::forecast::NodeRef) -> String {
    match (n.kind.as_deref(), n.visibility.as_deref()) {
        (Some(k), Some(v)) => format!(" ({k}, {v})"),
        (Some(k), None) => format!(" ({k})"),
        (None, Some(v)) => format!(" ({v})"),
        (None, None) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forecast::{
        DepEdge, DependencyDelta, ImpactHit, NodeRef, VerifyStep, FORECAST_VERSION,
    };

    fn populated() -> ChangeForecast {
        ChangeForecast {
            version: FORECAST_VERSION,
            base: Some("HEAD".into()),
            changed_files: vec!["src/a.py".into()],
            changed_nodes: vec![NodeRef {
                id: "a".into(),
                label: "alpha".into(),
                file: "src/a.py".into(),
                kind: Some("function".into()),
                visibility: Some("public".into()),
            }],
            blast_radius: vec![ImpactHit {
                id: "b".into(),
                label: "beta".into(),
                file: "src/b.py".into(),
                depth: 1,
                via_relation: "calls".into(),
            }],
            blast_radius_total: 1,
            at_risk_tests: vec![ImpactHit {
                id: "t".into(),
                label: "test_alpha".into(),
                file: "tests/test_a.py".into(),
                depth: 2,
                via_relation: "calls".into(),
            }],
            public_api_breaks: vec![NodeRef {
                id: "a".into(),
                label: "alpha".into(),
                file: "src/a.py".into(),
                kind: Some("function".into()),
                visibility: Some("public".into()),
            }],
            new_cycles: vec![vec!["x".into(), "y".into(), "x".into()]],
            removed_apis: vec!["gone (src/c.py)".into()],
            dependency_delta: DependencyDelta {
                added: vec![DepEdge {
                    from: "m1".into(),
                    to: "m2".into(),
                }],
                removed: vec![],
            },
            co_change_suggestions: vec![crate::CoChange {
                file: "src/schema.py".into(),
                support: 7,
                confidence_pct: 88,
            }],
            risk: Some(crate::RiskScore {
                score: 80,
                level: "high".into(),
                factors: vec!["blast radius (150)".into()],
            }),
            verify_checklist: vec![VerifyStep {
                description: "review dependents".into(),
                command: "codegraph affected \"alpha\"".into(),
            }],
            summary: "1 changed file(s), 1 changed node(s), 1 at-risk dependent(s)".into(),
        }
    }

    #[test]
    fn renders_header_summary_and_every_populated_section() {
        let md = render_markdown(&populated());
        assert!(md.starts_with("# Change forecast"));
        assert!(
            md.contains("1 at-risk dependent(s)"),
            "summary line present"
        );
        assert!(md.contains("## Changed files"));
        assert!(md.contains("## Changed nodes"));
        assert!(md.contains("## Change risk: high (80/100)"));
        assert!(md.contains("blast radius (150)"), "risk driver listed");
        assert!(md.contains("## Public API at risk"));
        assert!(md.contains("## Tests at risk"));
        assert!(md.contains("test_alpha"), "at-risk test listed");
        assert!(md.contains("## Blast radius"));
        assert!(md.contains("[1h via calls]"), "impact hit formatted");
        assert!(md.contains("## New import cycles"));
        assert!(md.contains("x -> y -> x"));
        assert!(md.contains("## Removed public APIs"));
        assert!(md.contains("## Dependency delta"));
        assert!(md.contains("`m1` -> `m2`"));
        assert!(md.contains("## Co-change suggestions"));
        assert!(md.contains("src/schema.py"), "co-change file listed");
        assert!(md.contains("## Verify checklist"));
        assert!(md.contains("codegraph affected \"alpha\""));
    }

    #[test]
    fn omits_empty_sections_but_keeps_header() {
        let f = ChangeForecast {
            version: FORECAST_VERSION,
            base: None,
            changed_files: vec![],
            changed_nodes: vec![],
            blast_radius: vec![],
            blast_radius_total: 0,
            at_risk_tests: vec![],
            public_api_breaks: vec![],
            new_cycles: vec![],
            removed_apis: vec![],
            dependency_delta: DependencyDelta::default(),
            co_change_suggestions: vec![],
            risk: None,
            verify_checklist: vec![],
            summary: "0 changed file(s), 0 changed node(s), 0 at-risk dependent(s)".into(),
        };
        let md = render_markdown(&f);
        assert!(md.starts_with("# Change forecast"));
        assert!(
            !md.contains("## New import cycles"),
            "empty section omitted"
        );
        assert!(
            !md.contains("## Public API at risk"),
            "empty section omitted"
        );
    }

    #[test]
    fn renders_an_edit_forecast() {
        let f = crate::EditForecast {
            symbol: "Service".into(),
            kind: "delete".into(),
            target_file: "svc.py".into(),
            removes_node: true,
            severed_edges: 4,
            removed_public_api: true,
            breaks: vec![crate::EditDependent {
                label: "caller".into(),
                file: "a.py".into(),
                depth: 1,
                via_relation: "calls".into(),
                reason: "depends on a symbol that would no longer exist".into(),
            }],
            review: vec![],
            summary: "delete Service: removes the node and severs 4 edge(s); removes a public API; 1 dependent(s) break, 0 to review".into(),
        };
        let md = render_edit_markdown(&f);
        assert!(md.starts_with("# Edit forecast"), "{md}");
        assert!(md.contains("edges severed: 4"), "{md}");
        assert!(md.contains("removes a public API"), "{md}");
        assert!(md.contains("## Will break (1)"), "{md}");
        assert!(md.contains("caller"), "{md}");
        assert!(md.contains("## To review (0)"), "{md}");
    }
}
