//! Classic ASP (VBScript) extractor — **regex-based** (no tree-sitter grammar
//! exists for Classic ASP / VBScript). Synaptic-original.
//!
//! `Function`/`Sub` → `name()` nodes, `Class` → class nodes; `<!--#include-->`
//! → `imports_from` to the included file's base name; calls between defined
//! functions/subs (`Foo(...)` or `Call Foo`) → `calls` edges. VBScript is
//! dynamically typed (no type refs).

#[cfg(feature = "lang-asp")]
use std::collections::{HashMap, HashSet};
#[cfg(feature = "lang-asp")]
use std::sync::LazyLock;

#[cfg(feature = "lang-asp")]
use regex::Regex;
#[cfg(feature = "lang-asp")]
use synaptic_core::{NodeId, make_id};

#[cfg(feature = "lang-asp")]
use crate::common::Builder;
#[cfg(feature = "lang-asp")]
use crate::paths::{file_node_id, file_stem};
#[cfg(feature = "lang-asp")]
use crate::result::ExtractionResult;

// Patterns compiled once process-wide (not per `.asp` file). M1.
#[cfg(feature = "lang-asp")]
static INCLUDE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<!--\s*#include\s+(?:file|virtual)\s*=\s*"([^"]+)""#).expect("inc re")
});
#[cfg(feature = "lang-asp")]
static DEF_RE: LazyLock<Regex> = LazyLock::new(|| {
    // `[ \t]*` not `\s*`: `\s` matches a newline, which would start the match on
    // a preceding blank line and report the declaration one or more lines early.
    Regex::new(
        r"(?im)^[ \t]*(?:public[ \t]+|private[ \t]+|default[ \t]+)*(function|sub|class)[ \t]+(\w+)",
    )
    .expect("def re")
});
#[cfg(feature = "lang-asp")]
static BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)\b(?:function|sub)\s+(\w+)(.*?)\bend\s+(?:function|sub)").expect("block re")
});
#[cfg(feature = "lang-asp")]
static CALL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:\bcall\s+(\w+))|(\b\w+)\s*\(").expect("call re"));

/// Extract a Classic ASP source file already in memory.
#[cfg(feature = "lang-asp")]
pub fn extract_asp_source(path: &str, source: &[u8]) -> ExtractionResult {
    let text = String::from_utf8_lossy(source);
    let mut b = Builder::new(path);
    let file_nid = file_node_id(path);
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let stem = file_stem(path);
    b.add_node(file_nid.clone(), filename, 1);

    // Precompute newline offsets once, then a per-match line lookup is O(log n).
    let newlines: Vec<usize> = text.match_indices('\n').map(|(i, _)| i).collect();
    let line_at = |byte: usize| newlines.partition_point(|&nl| nl < byte) + 1;

    // `<!--#include file="..."-->` / `virtual="..."` imports the base name.
    let include_re = &*INCLUDE_RE;
    for cap in include_re.captures_iter(&text) {
        let line = line_at(cap.get(0).expect("regex group 0 is the full match").start());
        let inc = &cap[1];
        let last = inc.rsplit(['/', '\\']).next().unwrap_or(inc);
        let base = last
            .strip_suffix(".asp")
            .or_else(|| last.strip_suffix(".inc"))
            .unwrap_or(last);
        if !base.is_empty() {
            let tgt = NodeId(make_id(&["asp", "inc", base]));
            b.add_external_node(tgt.clone(), base.to_string());
            b.add_edge(file_nid.clone(), tgt, "imports_from", line, Some("include"));
        }
    }

    // Definitions: Function / Sub / Class.
    let def_re = &*DEF_RE;
    let mut funcs: HashMap<String, NodeId> = HashMap::new(); // lower-name -> id (Function/Sub only)
    for cap in def_re.captures_iter(&text) {
        let line = line_at(cap.get(0).expect("regex group 0 is the full match").start());
        let kind = cap[1].to_lowercase();
        let name = cap[2].to_string();
        // `make_id` trims leading/trailing `_`, so `UnShift` and `UnShift_` share an
        // id and the second is dropped. VBScript libraries pair `Sub Name` with
        // `Function Name_` as a convention, so the collision deletes half the API.
        let id = NodeId(make_id(&[
            "asp",
            &stem,
            &crate::common::symbol_key(&name.to_lowercase()),
        ]));
        let label = if kind == "class" {
            name.clone()
        } else {
            format!("{name}()")
        };
        b.add_node(id.clone(), label, line);
        b.add_edge(file_nid.clone(), id.clone(), "contains", line, Some(&kind));
        if kind != "class" {
            funcs.insert(name.to_lowercase(), id);
        }
    }

    // Calls: per Function/Sub block, link to other defined functions/subs.
    let block_re = &*BLOCK_RE;
    let call_re = &*CALL_RE;
    let mut emitted: HashSet<(String, String)> = HashSet::new();
    for cap in block_re.captures_iter(&text) {
        let caller_name = cap[1].to_lowercase();
        let Some(caller) = funcs.get(&caller_name).cloned() else {
            continue;
        };
        let Some(body_m) = cap.get(2) else { continue };
        let (body, body_start) = (body_m.as_str(), body_m.start());
        for c in call_re.captures_iter(body) {
            let hit = c.get(1).or_else(|| c.get(2));
            let Some(hit) = hit else { continue };
            let callee = hit.as_str().to_lowercase();
            if callee == caller_name {
                continue;
            }
            if let Some(tgt) = funcs.get(&callee) {
                let key = (caller.0.clone(), tgt.0.clone());
                if emitted.insert(key) {
                    let line = line_at(body_start + hit.start());
                    b.add_edge(caller.clone(), tgt.clone(), "calls", line, Some("call"));
                }
            }
        }
    }

    b.into_result()
}

/// Read and extract a Classic ASP file from disk.
#[cfg(feature = "lang-asp")]
pub fn extract_asp_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_asp_source(&path_str, &source))
}

#[cfg(all(test, feature = "lang-asp"))]
mod tests {
    use super::extract_asp_source;
    use crate::result::ExtractionResult;

    const SAMPLE: &[u8] = b"<!--#include file=\"lib/util.asp\"-->\n<html>\n<%\nClass Account\nEnd Class\n\nFunction Greet(name)\n  Greet = Sound(name)\nEnd Function\n\nSub Sound(x)\n  Response.Write x\nEnd Sub\n%>\n</html>\n";

    fn extract() -> ExtractionResult {
        extract_asp_source("web/default.asp", SAMPLE)
    }

    fn labels(r: &ExtractionResult) -> Vec<String> {
        r.nodes.iter().map(|n| n.label.clone()).collect()
    }

    fn rels(r: &ExtractionResult, relation: &str) -> Vec<(String, String)> {
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
            .map(|e| (lbl(&e.source), lbl(&e.target)))
            .collect()
    }

    #[test]
    fn function_sub_and_class_nodes() {
        let ls = labels(&extract());
        assert!(ls.contains(&"Greet()".to_string()), "{ls:?}");
        assert!(ls.contains(&"Sound()".to_string()));
        assert!(ls.contains(&"Account".to_string()));
    }

    #[test]
    fn include_becomes_import() {
        assert!(
            rels(&extract(), "imports_from")
                .iter()
                .any(|(_, t)| t == "util")
        );
    }

    #[test]
    fn call_between_functions_resolves() {
        // Greet() calls Sound(); Response.Write is not a defined function.
        let calls = rels(&extract(), "calls");
        assert!(
            calls.contains(&("Greet()".to_string(), "Sound()".to_string())),
            "{calls:?}"
        );
        assert!(!calls.iter().any(|(_, t)| t.starts_with("Response")));
    }

    #[test]
    fn definitions_carry_their_own_line() {
        // SAMPLE: Class Account on line 4, Function Greet on 7, Sub Sound on 11.
        let r = extract();
        let at = |label: &str| {
            r.nodes
                .iter()
                .find(|n| n.label == label)
                .unwrap_or_else(|| panic!("no node {label}"))
                .source_location
                .clone()
        };
        assert_eq!(at("Account"), Some("L4".to_string()));
        assert_eq!(at("Greet()"), Some("L7".to_string()));
        assert_eq!(at("Sound()"), Some("L11".to_string()));
    }

    #[test]
    fn a_definition_after_blank_lines_is_not_pulled_up() {
        // `^\s*` would let \s match the newlines and start the match on line 2.
        let r = extract_asp_source("x.asp", b"<%\n\n\nFunction Late(a)\nEnd Function\n%>\n");
        let n = r
            .nodes
            .iter()
            .find(|n| n.label == "Late()")
            .expect("Late() node");
        assert_eq!(n.source_location, Some("L4".to_string()));
    }

    /// `make_id` trims leading/trailing `_`, so `UnShift` and `UnShift_` collapsed
    /// onto one id and the second was dropped. easyasp runs a three-way convention
    /// throughout its list and string APIs -- `Search`, `Search_`, `Search__` -- so
    /// the tag has to distinguish one underscore from two, not merely flag that
    /// some are present.
    #[test]
    fn underscore_suffixed_names_are_distinct_symbols() {
        let src = b"<%\nPublic Sub Search(s)\nEnd Sub\n\nPublic Function Search_(s)\nEnd Function\n\nPrivate Sub Search__(s, keep)\nEnd Sub\n%>\n";
        let r = extract_asp_source("core/easp.list.asp", src);
        let ls = labels(&r);
        for want in ["Search()", "Search_()", "Search__()"] {
            assert!(ls.contains(&want.to_string()), "missing {want}: {ls:?}");
        }
        let ids: Vec<&str> = r.nodes.iter().map(|n| n.id.0.as_str()).collect();
        let uniq: std::collections::HashSet<&&str> = ids.iter().collect();
        assert_eq!(ids.len(), uniq.len(), "ids must be distinct: {ids:?}");
    }

    #[test]
    fn data_html_without_asp_is_minimal() {
        // A plain HTML file with no VBScript yields just the file node.
        let r = extract_asp_source("x.asp", b"<html><body>hi</body></html>");
        assert_eq!(r.nodes.len(), 1);
    }
}
