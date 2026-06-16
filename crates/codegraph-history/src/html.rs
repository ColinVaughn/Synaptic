//! Render a [`DiffReport`] as a self-contained HTML page (inline CSS, no external
//! assets, plain ASCII). Mirrors the Markdown report's sections.

use std::fmt::Write as _;

use crate::report::DiffReport;

/// Escape text for safe inclusion in HTML element bodies and attributes.
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

const STYLE: &str = "body{font-family:system-ui,Arial,sans-serif;margin:2rem;color:#1a1a1a;\
background:#fff}h1{font-size:1.5rem}h2{font-size:1.1rem;margin-top:1.6rem;border-bottom:1px solid \
#ddd;padding-bottom:.2rem}table{border-collapse:collapse;margin:.4rem 0}td,th{border:1px solid \
#ccc;padding:.25rem .6rem;text-align:left;font-size:.9rem}th{background:#f3f3f3}code{background:\
#f3f3f3;padding:0 .2rem;border-radius:3px}.muted{color:#666}.up{color:#b00}.down{color:#070}\
@media(prefers-color-scheme:dark){body{background:#161616;color:#e6e6e6}th{background:#222}code,\
.k{background:#222}td,th{border-color:#333}h2{border-color:#333}}";

/// Render the full report as an HTML document string.
pub fn to_html(report: &DiffReport) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "<!DOCTYPE html>");
    let _ = writeln!(o, "<html lang=\"en\"><head><meta charset=\"utf-8\">");
    let _ = writeln!(
        o,
        "<title>CodeGraph diff {} -&gt; {}</title><style>{STYLE}</style></head><body>",
        esc(&report.rev1[..report.rev1.len().min(12)]),
        esc(&report.rev2[..report.rev2.len().min(12)])
    );
    let _ = writeln!(
        o,
        "<h1>Code graph diff: <code>{}</code> -&gt; <code>{}</code></h1>",
        esc(&report.rev1[..report.rev1.len().min(12)]),
        esc(&report.rev2[..report.rev2.len().min(12)])
    );
    let _ = writeln!(o, "<p class=\"muted\">{}</p>", esc(&report.summary));

    // Dependencies.
    section_deps(
        &mut o,
        "Added dependencies",
        &report.added_dependencies,
        "up",
    );
    section_deps(
        &mut o,
        "Removed dependencies",
        &report.removed_dependencies,
        "down",
    );

    // Removed APIs.
    let _ = writeln!(o, "<h2>Removed APIs</h2>");
    if report.removed_apis.is_empty() {
        let _ = writeln!(o, "<p class=\"muted\">none</p>");
    } else {
        let _ = writeln!(
            o,
            "<table><tr><th>symbol</th><th>file</th><th>referenced by</th></tr>"
        );
        for a in &report.removed_apis {
            let _ = writeln!(
                o,
                "<tr><td><code>{}</code></td><td>{}</td><td>{}</td></tr>",
                esc(&a.label),
                esc(&a.source_file),
                a.referenced_by
            );
        }
        let _ = writeln!(o, "</table>");
    }

    // Drift.
    let d = &report.drift;
    let _ = writeln!(o, "<h2>Architectural drift</h2>");
    let _ = writeln!(
        o,
        "<p>Coupling {:.3} -&gt; {:.3}; communities {} -&gt; {}.</p>",
        d.coupling_before, d.coupling_after, d.communities_before, d.communities_after
    );
    if !d.modules.is_empty() {
        let _ = writeln!(
            o,
            "<table><tr><th>module</th><th>before</th><th>after</th><th>delta</th></tr>"
        );
        for m in &d.modules {
            let cls = if m.delta >= 0.0 { "up" } else { "down" };
            let _ = writeln!(
                o,
                "<tr><td>{}</td><td>{:.3}</td><td>{:.3}</td><td class=\"{cls}\">{:+.3}</td></tr>",
                esc(&m.module),
                m.coupling_before,
                m.coupling_after,
                m.delta
            );
        }
        let _ = writeln!(o, "</table>");
    }

    // New cycles.
    let _ = writeln!(o, "<h2>New cycles</h2>");
    if report.new_cycles.is_empty() {
        let _ = writeln!(o, "<p class=\"muted\">none</p>");
    } else {
        let _ = writeln!(o, "<ul>");
        for cyc in &report.new_cycles {
            let path: Vec<String> = cyc.iter().map(|s| esc(s)).collect();
            let _ = writeln!(o, "<li><code>{}</code></li>", path.join(" -&gt; "));
        }
        let _ = writeln!(o, "</ul>");
    }

    // Hotspots.
    let _ = writeln!(o, "<h2>Hotspots of change</h2>");
    if report.hotspots.is_empty() {
        let _ = writeln!(o, "<p class=\"muted\">none</p>");
    } else {
        let _ = writeln!(
            o,
            "<table><tr><th>file</th><th>+lines</th><th>-lines</th><th>+nodes</th><th>-nodes</th><th>score</th></tr>"
        );
        for h in &report.hotspots {
            let _ = writeln!(
                o,
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{:.1}</td></tr>",
                esc(&h.file),
                h.lines_added,
                h.lines_removed,
                h.nodes_added,
                h.nodes_removed,
                h.score
            );
        }
        let _ = writeln!(o, "</table>");
    }

    let _ = writeln!(o, "</body></html>");
    o
}

fn section_deps(o: &mut String, title: &str, deps: &[crate::report::ModuleDep], cls: &str) {
    let _ = writeln!(o, "<h2>{}</h2>", esc(title));
    if deps.is_empty() {
        let _ = writeln!(o, "<p class=\"muted\">none</p>");
        return;
    }
    let _ = writeln!(o, "<ul class=\"{cls}\">");
    for d in deps {
        let _ = writeln!(
            o,
            "<li><code>{}</code> -&gt; <code>{}</code></li>",
            esc(&d.from),
            esc(&d.to)
        );
    }
    let _ = writeln!(o, "</ul>");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{DriftReport, ModuleDep, RemovedApi};

    fn sample() -> DiffReport {
        DiffReport {
            rev1: "aaaaaaaaaaaa0000".into(),
            rev2: "bbbbbbbbbbbb1111".into(),
            summary: "1 added, 0 removed".into(),
            added_dependencies: vec![ModuleDep {
                from: "crates/a".into(),
                to: "crates/<script>".into(), // XSS probe
            }],
            removed_dependencies: vec![],
            removed_apis: vec![RemovedApi {
                id: "x".into(),
                label: "OldApi".into(),
                source_file: "src/x.rs".into(),
                referenced_by: 3,
            }],
            drift: DriftReport {
                communities_before: 2,
                communities_after: 3,
                coupling_before: 0.1,
                coupling_after: 0.2,
                modules: vec![],
            },
            new_cycles: vec![vec!["a".into(), "b".into(), "a".into()]],
            hotspots: vec![],
        }
    }

    #[test]
    fn renders_document_with_sections() {
        let html = to_html(&sample());
        assert!(html.starts_with("<!DOCTYPE html>"));
        for s in [
            "Added dependencies",
            "Removed APIs",
            "Architectural drift",
            "New cycles",
            "Hotspots of change",
        ] {
            assert!(html.contains(s), "missing section {s}");
        }
        // The script-y module name is escaped (no raw <script>).
        assert!(!html.contains("<script>"), "unescaped HTML leaked");
        assert!(html.contains("&lt;script&gt;"));
    }
}
