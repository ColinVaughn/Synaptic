//! Markdown structure extractor (feature `lang-markdown`).
//!
//! Builds the heading hierarchy as `document` nodes connected by `contains`
//! edges (each heading is contained by the nearest enclosing heading of a
//! shallower level, or the file node at the top level). Fenced code blocks are
//! skipped so a `#` inside ```` ``` ```` is never mistaken for a heading.
//!
//! We implement headings + `contains` only and do not scan inline
//! `` `backticks` `` for `heading --references--> <name>` edges: doing so would
//! re-introduce the orphan-node bug the fence-skip was added to fix.

#[cfg(feature = "lang-markdown")]
use std::sync::LazyLock;

#[cfg(feature = "lang-markdown")]
use regex::Regex;
#[cfg(feature = "lang-markdown")]
use synaptic_core::{make_id, FileType, NodeId};

#[cfg(feature = "lang-markdown")]
use crate::common::Builder;
#[cfg(feature = "lang-markdown")]
use crate::paths::{file_node_id, file_stem};
#[cfg(feature = "lang-markdown")]
use crate::result::ExtractionResult;

#[cfg(feature = "lang-markdown")]
static HEADING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(#{1,6})\s+(.+)$").expect("valid heading regex"));

/// Extract Markdown heading structure from in-memory source.
#[cfg(feature = "lang-markdown")]
pub fn extract_markdown_source(path: &str, source: &[u8]) -> ExtractionResult {
    let mut b = Builder::new(path);
    let file_nid = file_node_id(path);
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    b.add_node_typed(file_nid.clone(), filename, FileType::Document, 1);

    let text = String::from_utf8_lossy(source);
    let stem = file_stem(path);
    // Stack of (level, node-id) for the enclosing-heading hierarchy.
    let mut stack: Vec<(usize, NodeId)> = Vec::new();
    let mut in_code = false;

    for (i, line) in text.lines().enumerate() {
        let line_no = i + 1;
        // Toggle on a fence line (``` after optional leading whitespace); skip
        // everything inside, so a `#` in a code block is never a heading.
        if line.trim().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        let Some(cap) = HEADING_RE.captures(line) else {
            continue;
        };
        let level = cap[1].len();
        let title = cap[2].trim().to_string();
        if title.is_empty() {
            continue;
        }
        // Unique id per heading: append the line number on a title collision so
        // two `## Setup` sections stay distinct nodes.
        let mut nid = NodeId(make_id(&[&stem, &title]));
        if b.seen.contains(&nid) {
            nid = NodeId(make_id(&[&stem, &title, &line_no.to_string()]));
        }
        b.add_node_typed(nid.clone(), title, FileType::Document, line_no);

        // Parent = nearest shallower heading still on the stack, else the file.
        while let Some(&(lvl, _)) = stack.last() {
            if lvl >= level {
                stack.pop();
            } else {
                break;
            }
        }
        let parent = stack
            .last()
            .map(|(_, id)| id.clone())
            .unwrap_or_else(|| file_nid.clone());
        b.add_edge(parent, nid.clone(), "contains", line_no, None);
        stack.push((level, nid));
    }
    b.into_result()
}

/// Read and extract a Markdown file from disk.
#[cfg(feature = "lang-markdown")]
pub fn extract_markdown_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_markdown_source(&path_str, &source))
}

#[cfg(all(test, feature = "lang-markdown"))]
mod tests {
    use super::*;

    fn extract(src: &str) -> ExtractionResult {
        extract_markdown_source("docs/guide.md", src.as_bytes())
    }

    fn label_of(r: &ExtractionResult, id: &NodeId) -> String {
        r.nodes
            .iter()
            .find(|n| &n.id == id)
            .map(|n| n.label.clone())
            .unwrap_or_else(|| id.0.clone())
    }

    fn contains(r: &ExtractionResult) -> Vec<(String, String)> {
        r.edges
            .iter()
            .filter(|e| e.relation == "contains")
            .map(|e| (label_of(r, &e.source), label_of(r, &e.target)))
            .collect()
    }

    #[test]
    fn headings_become_document_nodes() {
        let r = extract("# Title\n\nsome text\n\n## Section\n");
        assert!(r.nodes.iter().any(|n| n.label == "Title"));
        assert!(r.nodes.iter().any(|n| n.label == "Section"));
        assert!(
            r.nodes.iter().all(|n| n.file_type == FileType::Document),
            "all markdown nodes are documents"
        );
    }

    #[test]
    fn hierarchy_nests_by_level() {
        let r = extract("# Top\n\n## Child\n\n### Grandchild\n\n## Sibling\n");
        let c = contains(&r);
        // file -> Top, Top -> Child, Child -> Grandchild, Top -> Sibling.
        assert!(
            c.contains(&("guide.md".to_string(), "Top".to_string())),
            "{c:?}"
        );
        assert!(
            c.contains(&("Top".to_string(), "Child".to_string())),
            "{c:?}"
        );
        assert!(
            c.contains(&("Child".to_string(), "Grandchild".to_string())),
            "{c:?}"
        );
        assert!(
            c.contains(&("Top".to_string(), "Sibling".to_string())),
            "{c:?}"
        );
    }

    #[test]
    fn fenced_code_hash_is_not_a_heading() {
        let r = extract("# Real\n\n```sh\n# not a heading\nls\n```\n\n## After\n");
        let labels: Vec<_> = r.nodes.iter().map(|n| n.label.clone()).collect();
        assert!(labels.contains(&"Real".to_string()), "{labels:?}");
        assert!(labels.contains(&"After".to_string()), "{labels:?}");
        assert!(
            !labels.iter().any(|l| l.contains("not a heading")),
            "code-block # leaked as heading: {labels:?}"
        );
    }

    #[test]
    fn duplicate_titles_stay_distinct_nodes() {
        let r = extract("## Setup\n\ntext\n\n## Setup\n");
        let setup = r.nodes.iter().filter(|n| n.label == "Setup").count();
        assert_eq!(setup, 2, "duplicate-title headings must be distinct nodes");
    }

    #[test]
    fn no_backtick_reference_edges() {
        // We deliberately do not emit `references` edges from inline backticks
        // (only `contains`).
        let r = extract("# Uses `parse_config`\n");
        assert!(
            r.edges.iter().all(|e| e.relation == "contains"),
            "unexpected non-contains edge: {:?}",
            r.edges
        );
    }
}
