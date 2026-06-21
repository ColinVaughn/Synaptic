//! Salesforce Apex extractor — **regex-based**. `tree-sitter-apex` pins an
//! incompatible tree-sitter version, so this is a regex fallback over
//! `.cls`/`.trigger` files.
//!
//! `class`/`interface`/`enum` → name nodes (`contains` from the file); methods →
//! `name()` nodes (`method` from the nearest enclosing class, else `contains`
//! from the file); `trigger Name on SObject` → a trigger node plus a `triggers`
//! edge to the SObject (a `concept`). Apex is statically typed but call graphs
//! over a regex parse are noisy, so calls are intentionally not emitted.

#[cfg(feature = "lang-apex")]
use std::sync::LazyLock;

#[cfg(feature = "lang-apex")]
use regex::Regex;
#[cfg(feature = "lang-apex")]
use synaptic_core::{make_id, FileType, NodeId};

#[cfg(feature = "lang-apex")]
use crate::common::Builder;
#[cfg(feature = "lang-apex")]
use crate::paths::{file_node_id, file_stem};
#[cfg(feature = "lang-apex")]
use crate::result::ExtractionResult;

#[cfg(feature = "lang-apex")]
static TYPE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?im)^\s*(?:(?:global|public|private|protected|virtual|abstract|with\s+sharing|without\s+sharing|inherited\s+sharing|static|override|final)\s+)*(class|interface|enum)\s+(\w+)",
    )
    .expect("valid apex type regex")
});
#[cfg(feature = "lang-apex")]
static TRIGGER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*trigger\s+(\w+)\s+on\s+(\w+)").expect("valid trigger re")
});
#[cfg(feature = "lang-apex")]
static METHOD_RE: LazyLock<Regex> = LazyLock::new(|| {
    // A declaration is a line with ≥1 access/modifier keyword, an optional return
    // type, then `name(`. The leading-modifier requirement excludes control flow
    // (`if (`, `for (`) and plain calls.
    Regex::new(
        r"(?im)^\s*(?:(?:global|public|private|protected|static|virtual|override|abstract|final|testmethod|webservice|transient)\s+)+(?:[\w.<>\[\],]+(?:\s*<[^>]*>)?\s+)?(\w+)\s*\(",
    )
    .expect("valid apex method regex")
});

/// Extract an Apex source file already in memory.
#[cfg(feature = "lang-apex")]
pub fn extract_apex_source(path: &str, source: &[u8]) -> ExtractionResult {
    let text = String::from_utf8_lossy(source);
    let mut b = Builder::new(path);
    let file_nid = file_node_id(path);
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let stem = file_stem(path);
    b.add_node(file_nid.clone(), filename, 1);

    // Precompute newline offsets once, then a per-match line lookup is O(log n)
    // instead of re-scanning from the start every time (was O(matches × len)). M2.
    let newlines: Vec<usize> = text.match_indices('\n').map(|(i, _)| i).collect();
    let line_at = |byte: usize| newlines.partition_point(|&nl| nl < byte) + 1;

    // Types (class/interface/enum). Keep (byte-offset, id) so methods can attach
    // to the nearest enclosing class.
    let mut classes: Vec<(usize, NodeId)> = Vec::new();
    for cap in TYPE_RE.captures_iter(&text) {
        let name = &cap[2];
        let pos = cap.get(0).expect("regex group 0 is the full match").start();
        let line = line_at(pos);
        let id = NodeId(make_id(&["apex", &stem, &name.to_lowercase()]));
        b.add_node(id.clone(), name.to_string(), line);
        b.add_edge(
            file_nid.clone(),
            id.clone(),
            "contains",
            line,
            Some(&cap[1]),
        );
        classes.push((pos, id));
    }

    // Triggers: `trigger Name on SObject (events)`.
    for cap in TRIGGER_RE.captures_iter(&text) {
        let name = &cap[1];
        let object = &cap[2];
        let pos = cap.get(0).expect("regex group 0 is the full match").start();
        let line = line_at(pos);
        let tid = NodeId(make_id(&["apex", &stem, &name.to_lowercase()]));
        b.add_node(tid.clone(), name.to_string(), line);
        b.add_edge(
            file_nid.clone(),
            tid.clone(),
            "contains",
            line,
            Some("trigger"),
        );
        let oid = NodeId(make_id(&["sobject", object]));
        b.add_node_typed(oid.clone(), object.to_string(), FileType::Concept, line);
        b.add_edge(tid, oid, "triggers", line, Some("trigger_object"));
    }

    // Methods attach to the nearest enclosing class (by byte offset), else the file node.
    for cap in METHOD_RE.captures_iter(&text) {
        let name = &cap[1];
        // Skip keywords the modifier-prefixed pattern can still catch before a
        // control construct (defensive; `new` constructors etc.).
        if matches!(
            name.to_lowercase().as_str(),
            "if" | "for" | "while" | "catch" | "switch" | "return"
        ) {
            continue;
        }
        let pos = cap.get(0).expect("regex group 0 is the full match").start();
        let line = line_at(pos);
        let owner = classes
            .iter()
            .rfind(|(cpos, _)| *cpos < pos)
            .map(|(_, id)| id.clone());
        let mid = match &owner {
            Some(cid) => NodeId(make_id(&[cid.as_str(), &name.to_lowercase()])),
            None => NodeId(make_id(&["apex", &stem, &name.to_lowercase(), "fn"])),
        };
        b.add_node(mid.clone(), format!("{name}()"), line);
        match owner {
            Some(cid) => b.add_edge(cid, mid, "method", line, None),
            None => b.add_edge(file_nid.clone(), mid, "contains", line, Some("method")),
        }
    }

    b.into_result()
}

/// Read and extract an Apex file from disk.
#[cfg(feature = "lang-apex")]
pub fn extract_apex_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_apex_source(&path_str, &source))
}

#[cfg(all(test, feature = "lang-apex"))]
mod tests {
    use super::*;

    fn labels(r: &ExtractionResult) -> Vec<String> {
        r.nodes.iter().map(|n| n.label.clone()).collect()
    }

    fn rels(r: &ExtractionResult, relation: &str) -> Vec<(String, String)> {
        let lbl = |id: &NodeId| {
            r.nodes
                .iter()
                .find(|n| &n.id == id)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| id.0.clone())
        };
        r.edges
            .iter()
            .filter(|e| e.relation == relation)
            .map(|e| (lbl(&e.source), lbl(&e.target)))
            .collect()
    }

    #[test]
    fn class_and_methods() {
        let src = b"public with sharing class AccountService {\n  public Account fetch(Id id) {\n    return null;\n  }\n  private void log(String msg) {}\n}\n";
        let r = extract_apex_source("classes/AccountService.cls", src);
        let ls = labels(&r);
        assert!(ls.contains(&"AccountService".to_string()), "{ls:?}");
        assert!(ls.contains(&"fetch()".to_string()), "{ls:?}");
        assert!(ls.contains(&"log()".to_string()), "{ls:?}");
        // methods attach to the class.
        let methods = rels(&r, "method");
        assert!(
            methods.contains(&("AccountService".to_string(), "fetch()".to_string())),
            "{methods:?}"
        );
    }

    #[test]
    fn control_flow_is_not_a_method() {
        let src = b"public class C {\n  public void run() {\n    if (true) { return; }\n    for (Integer i = 0; i < 3; i++) {}\n  }\n}\n";
        let r = extract_apex_source("classes/C.cls", src);
        let ls = labels(&r);
        assert!(ls.contains(&"run()".to_string()), "{ls:?}");
        assert!(!ls.iter().any(|l| l == "if()" || l == "for()"), "{ls:?}");
    }

    #[test]
    fn trigger_fires_on_sobject() {
        let src = b"trigger AccountTrigger on Account (before insert, after update) {\n  System.debug('x');\n}\n";
        let r = extract_apex_source("triggers/AccountTrigger.trigger", src);
        assert!(labels(&r).contains(&"AccountTrigger".to_string()));
        let fires = rels(&r, "triggers");
        assert!(
            fires.contains(&("AccountTrigger".to_string(), "Account".to_string())),
            "{fires:?}"
        );
        assert_eq!(
            r.nodes
                .iter()
                .find(|n| n.label == "Account")
                .map(|n| n.file_type),
            Some(FileType::Concept)
        );
    }
}
