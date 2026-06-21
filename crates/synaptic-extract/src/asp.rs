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
use synaptic_core::{make_id, NodeId};

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
    Regex::new(r"(?im)^\s*(?:public\s+|private\s+|default\s+)*(function|sub|class)\s+(\w+)")
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

    // `<!--#include file="..."-->` / `virtual="..."` imports the base name.
    let include_re = &*INCLUDE_RE;
    for cap in include_re.captures_iter(&text) {
        let inc = &cap[1];
        let last = inc.rsplit(['/', '\\']).next().unwrap_or(inc);
        let base = last
            .strip_suffix(".asp")
            .or_else(|| last.strip_suffix(".inc"))
            .unwrap_or(last);
        if !base.is_empty() {
            let tgt = NodeId(make_id(&["asp", "inc", base]));
            b.add_external_node(tgt.clone(), base.to_string());
            b.add_edge(file_nid.clone(), tgt, "imports_from", 1, Some("include"));
        }
    }

    // Definitions: Function / Sub / Class.
    let def_re = &*DEF_RE;
    let mut funcs: HashMap<String, NodeId> = HashMap::new(); // lower-name -> id (Function/Sub only)
    for cap in def_re.captures_iter(&text) {
        let kind = cap[1].to_lowercase();
        let name = cap[2].to_string();
        let id = NodeId(make_id(&["asp", &stem, &name.to_lowercase()]));
        let label = if kind == "class" {
            name.clone()
        } else {
            format!("{name}()")
        };
        b.add_node(id.clone(), label, 1);
        b.add_edge(file_nid.clone(), id.clone(), "contains", 1, Some(&kind));
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
        let body = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        for c in call_re.captures_iter(body) {
            let callee = c
                .get(1)
                .or_else(|| c.get(2))
                .map(|m| m.as_str().to_lowercase());
            let Some(callee) = callee else { continue };
            if callee == caller_name {
                continue;
            }
            if let Some(tgt) = funcs.get(&callee) {
                let key = (caller.0.clone(), tgt.0.clone());
                if emitted.insert(key) {
                    b.add_edge(caller.clone(), tgt.clone(), "calls", 1, Some("call"));
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
        assert!(rels(&extract(), "imports_from")
            .iter()
            .any(|(_, t)| t == "util"));
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
    fn data_html_without_asp_is_minimal() {
        // A plain HTML file with no VBScript yields just the file node.
        let r = extract_asp_source("x.asp", b"<html><body>hi</body></html>");
        assert_eq!(r.nodes.len(), 1);
    }
}
