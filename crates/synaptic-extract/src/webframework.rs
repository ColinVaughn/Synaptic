//! Vue / Svelte / Astro single-file components. These are HTML-ish templates
//! with an embedded JS/TS script block (Vue/Svelte: `<script>…</script>`; Astro:
//! `---`-fenced frontmatter). Rather than parse the whole template, we extract
//! the script and delegate to the existing TypeScript/JavaScript extractor —
//! the component's imports, functions, and classes flow straight into the graph.
//!
//! The script content is newline-padded to its original offset so node
//! `source_location` line numbers match the `.vue`/`.svelte`/`.astro` file.

#[cfg(any(feature = "lang-vue", feature = "lang-svelte", feature = "lang-astro"))]
use crate::ecmascript::{extract_js_source, extract_ts_source};
#[cfg(any(feature = "lang-vue", feature = "lang-svelte", feature = "lang-astro"))]
use crate::result::ExtractionResult;

/// Run the TS or JS extractor over `script`, padded with `pad` leading newlines
/// so line numbers line up with the original component file.
#[cfg(any(feature = "lang-vue", feature = "lang-svelte", feature = "lang-astro"))]
fn delegate(path: &str, script: &str, pad: usize, is_ts: bool) -> ExtractionResult {
    let mut padded = String::with_capacity(pad + script.len());
    for _ in 0..pad {
        padded.push('\n');
    }
    padded.push_str(script);
    if is_ts {
        extract_ts_source(path, padded.as_bytes())
    } else {
        extract_js_source(path, padded.as_bytes())
    }
}

/// The first `<script …>…</script>` block: `(content, leading_newlines, is_ts)`.
#[cfg(any(feature = "lang-vue", feature = "lang-svelte"))]
fn script_block(source: &str) -> Option<(&str, usize, bool)> {
    let lower = source.to_lowercase();
    let open = lower.find("<script")?;
    let tag_end = source[open..].find('>')? + open;
    let opening = source[open..=tag_end].to_lowercase();
    let is_ts = opening.contains("lang=\"ts\"")
        || opening.contains("lang='ts'")
        || opening.contains("typescript");
    let content_start = tag_end + 1;
    let close = lower[content_start..].find("</script>")? + content_start;
    let content = &source[content_start..close];
    let pad = source[..content_start].matches('\n').count();
    Some((content, pad, is_ts))
}

/// Astro frontmatter: the leading `---`-fenced block (always JS/TS).
#[cfg(feature = "lang-astro")]
fn frontmatter(source: &str) -> Option<(&str, usize)> {
    if !source.trim_start().starts_with("---") {
        return None;
    }
    let first = source.find("---")?;
    let after = first + 3;
    let close = source[after..].find("\n---")? + after;
    let content = &source[after..close];
    let pad = source[..after].matches('\n').count();
    Some((content, pad))
}

/// Extract a Vue single-file component (`<script>`/`<script setup>`).
#[cfg(feature = "lang-vue")]
pub fn extract_vue_source(path: &str, source: &[u8]) -> ExtractionResult {
    let text = String::from_utf8_lossy(source);
    match script_block(&text) {
        Some((script, pad, is_ts)) => delegate(path, script, pad, is_ts),
        None => ExtractionResult::default(),
    }
}

/// Extract a Svelte component (`<script>`).
#[cfg(feature = "lang-svelte")]
pub fn extract_svelte_source(path: &str, source: &[u8]) -> ExtractionResult {
    let text = String::from_utf8_lossy(source);
    match script_block(&text) {
        Some((script, pad, is_ts)) => delegate(path, script, pad, is_ts),
        None => ExtractionResult::default(),
    }
}

/// Extract an Astro component (`---` frontmatter, treated as TS).
#[cfg(feature = "lang-astro")]
pub fn extract_astro_source(path: &str, source: &[u8]) -> ExtractionResult {
    let text = String::from_utf8_lossy(source);
    match frontmatter(&text) {
        Some((script, pad)) => delegate(path, script, pad, true),
        None => ExtractionResult::default(),
    }
}

#[cfg(all(test, feature = "lang-vue", feature = "lang-astro"))]
mod tests {
    use super::{extract_astro_source, extract_svelte_source, extract_vue_source};
    use crate::result::ExtractionResult;

    fn rels(r: &ExtractionResult, relation: &str) -> Vec<String> {
        let lbl = |id: &synaptic_core::NodeId| {
            r.nodes
                .iter()
                .find(|n| &n.id == id)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| id.0.clone())
        };
        r.edges
            .iter()
            .filter(|e| e.relation == relation)
            .map(|e| lbl(&e.target))
            .collect()
    }

    fn labels(r: &ExtractionResult) -> Vec<String> {
        r.nodes.iter().map(|n| n.label.clone()).collect()
    }

    #[test]
    fn vue_script_delegates_to_ts() {
        let src = b"<template><div/></template>\n<script setup lang=\"ts\">\nimport { ref } from 'vue'\nfunction greet(): string { return 'hi' }\n</script>\n";
        let r = extract_vue_source("App.vue", src);
        assert!(
            labels(&r).contains(&"greet()".to_string()),
            "{:?}",
            labels(&r)
        );
        // import resolves through the TS extractor.
        assert!(r.edges.iter().any(|e| e.relation == "imports_from"));
        // line number reflects the original file (script starts at line 3).
        let greet = r.nodes.iter().find(|n| n.label == "greet()").unwrap();
        assert_eq!(greet.source_location.as_deref(), Some("L4"));
    }

    #[test]
    fn svelte_script_without_lang_is_js() {
        let src = b"<script>\nfunction handle() { return 1 }\n</script>\n<button>x</button>\n";
        let r = extract_svelte_source("Btn.svelte", src);
        assert!(
            labels(&r).contains(&"handle()".to_string()),
            "{:?}",
            labels(&r)
        );
    }

    #[test]
    fn astro_frontmatter_delegates() {
        let src = b"---\nimport Layout from './L.astro'\nfunction render() { return 1 }\n---\n<html></html>\n";
        let r = extract_astro_source("Page.astro", src);
        assert!(
            labels(&r).contains(&"render()".to_string()),
            "{:?}",
            labels(&r)
        );
        assert!(
            rels(&r, "imports_from").iter().any(|t| t.contains('L'))
                || !rels(&r, "imports_from").is_empty()
        );
    }

    #[test]
    fn non_component_returns_empty() {
        assert!(extract_vue_source("x.vue", b"<template>hi</template>")
            .nodes
            .is_empty());
    }
}
