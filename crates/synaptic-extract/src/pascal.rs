//! Pascal / Delphi extractor — **regex-based** (no maintained tree-sitter
//! grammar). Covers `.pas/.pp/.dpr/.dpk/.lpr/.inc`. Synaptic-original, mirroring
//! the regex-fallback approach used for Classic ASP.
//!
//! `uses A, B, C;` → `imports_from` to each unit (external nodes); `procedure`/
//! `function Name` (incl. qualified `TFoo.Bar`) → `name()` nodes; `Name = class|
//! record|interface` → type nodes; all `contains` from the file node. Pascal's
//! free-form syntax and case-insensitivity make call resolution unreliable over
//! a regex parse, so calls are not emitted.

#[cfg(feature = "lang-pascal")]
use std::sync::LazyLock;

#[cfg(feature = "lang-pascal")]
use regex::Regex;
#[cfg(feature = "lang-pascal")]
use synaptic_core::{make_id, NodeId};

#[cfg(feature = "lang-pascal")]
use crate::common::Builder;
#[cfg(feature = "lang-pascal")]
use crate::paths::{file_node_id, file_stem};
#[cfg(feature = "lang-pascal")]
use crate::result::ExtractionResult;

#[cfg(feature = "lang-pascal")]
static USES_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)\buses\b\s+(.*?);").expect("valid uses regex"));
#[cfg(feature = "lang-pascal")]
static ROUTINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:procedure|function)\s+(\w+(?:\.\w+)?)").expect("valid routine regex")
});
#[cfg(feature = "lang-pascal")]
static TYPE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(\w+)\s*=\s*(?:packed\s+)?(class|record|interface|object)\b")
        .expect("valid pascal type regex")
});

/// Blank out Pascal comments (`{ … }`, `(* … *)`, `// …`) and the *contents* of
/// `'…'` string literals while preserving every newline (so line numbers stay
/// accurate), so the structural regexes never match a keyword that only appears
/// in a comment or string. Strings are skipped (not blanked) outside themselves
/// — e.g. a `'http://x'` literal must not look like a `//` comment.
#[cfg(feature = "lang-pascal")]
fn strip_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let n = bytes.len();
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let blank = |b: u8| if b == b'\n' { b'\n' } else { b' ' };
    let mut i = 0;
    while i < n {
        match bytes[i] {
            b'\'' => {
                // Copy the string literal verbatim (incl. quotes).
                out.push(b'\'');
                i += 1;
                while i < n {
                    out.push(bytes[i]);
                    let c = bytes[i];
                    i += 1;
                    if c == b'\'' {
                        break;
                    }
                }
            }
            b'/' if i + 1 < n && bytes[i + 1] == b'/' => {
                while i < n && bytes[i] != b'\n' {
                    out.push(b' ');
                    i += 1;
                }
            }
            b'{' => {
                while i < n && bytes[i] != b'}' {
                    out.push(blank(bytes[i]));
                    i += 1;
                }
                if i < n {
                    out.push(b' '); // the closing `}`
                    i += 1;
                }
            }
            b'(' if i + 1 < n && bytes[i + 1] == b'*' => {
                out.push(b' ');
                out.push(b' ');
                i += 2;
                while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b')') {
                    out.push(blank(bytes[i]));
                    i += 1;
                }
                if i + 1 < n {
                    out.push(b' ');
                    out.push(b' ');
                    i += 2;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Extract a Pascal/Delphi source file already in memory.
#[cfg(feature = "lang-pascal")]
pub fn extract_pascal_source(path: &str, source: &[u8]) -> ExtractionResult {
    let text = strip_comments(&String::from_utf8_lossy(source));
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

    // `uses A, B in 'b.pas', C;` clauses (may appear in interface + implementation).
    for cap in USES_RE.captures_iter(&text) {
        let line = line_at(cap.get(0).expect("regex group 0 is the full match").start());
        for raw in cap[1].split(',') {
            // Drop a Delphi `Unit in 'path'` qualifier; keep the unit name.
            let name = raw
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '.');
            if name.is_empty() {
                continue;
            }
            let tail = name.rsplit('.').next().unwrap_or(name);
            let tgt = NodeId(make_id(&["pascal", "unit", &tail.to_lowercase()]));
            b.add_external_node(tgt.clone(), tail.to_string());
            b.add_edge(file_nid.clone(), tgt, "imports_from", line, Some("uses"));
        }
    }

    // Types declared as `Name = class|record|interface|object`.
    for cap in TYPE_RE.captures_iter(&text) {
        let name = &cap[1];
        let line = line_at(cap.get(0).expect("regex group 0 is the full match").start());
        let id = NodeId(make_id(&["pascal", &stem, &name.to_lowercase()]));
        b.add_node(id.clone(), name.to_string(), line);
        b.add_edge(file_nid.clone(), id, "contains", line, Some(&cap[2]));
    }

    // Procedures / functions (incl. qualified method impls `TFoo.Bar`).
    for cap in ROUTINE_RE.captures_iter(&text) {
        let name = &cap[1];
        let line = line_at(cap.get(0).expect("regex group 0 is the full match").start());
        let id = NodeId(make_id(&["pascal", &stem, &name.to_lowercase()]));
        b.add_node(id.clone(), format!("{name}()"), line);
        b.add_edge(file_nid.clone(), id, "contains", line, Some("routine"));
    }

    b.into_result()
}

/// Read and extract a Pascal/Delphi file from disk.
#[cfg(feature = "lang-pascal")]
pub fn extract_pascal_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_pascal_source(&path_str, &source))
}

#[cfg(all(test, feature = "lang-pascal"))]
mod tests {
    use super::*;

    fn labels(r: &ExtractionResult) -> Vec<String> {
        r.nodes.iter().map(|n| n.label.clone()).collect()
    }

    fn targets(r: &ExtractionResult, relation: &str) -> Vec<String> {
        r.edges
            .iter()
            .filter(|e| e.relation == relation)
            .map(|e| {
                r.nodes
                    .iter()
                    .find(|n| n.id == e.target)
                    .map(|n| n.label.clone())
                    .unwrap_or_else(|| e.target.0.clone())
            })
            .collect()
    }

    const SAMPLE: &[u8] = b"unit MyUnit;\ninterface\nuses SysUtils, Classes;\ntype\n  TAccount = class\n    procedure Deposit(amount: Double);\n  end;\nimplementation\nprocedure TAccount.Deposit(amount: Double);\nbegin\nend;\nfunction Total: Double;\nbegin\nend;\nend.\n";

    #[test]
    fn uses_clause_becomes_imports() {
        let imps = targets(
            &extract_pascal_source("src/MyUnit.pas", SAMPLE),
            "imports_from",
        );
        assert!(imps.contains(&"SysUtils".to_string()), "{imps:?}");
        assert!(imps.contains(&"Classes".to_string()), "{imps:?}");
    }

    #[test]
    fn types_and_routines_are_nodes() {
        let ls = labels(&extract_pascal_source("src/MyUnit.pas", SAMPLE));
        assert!(ls.contains(&"TAccount".to_string()), "{ls:?}");
        assert!(ls.contains(&"Total()".to_string()), "{ls:?}");
        // qualified method impl keeps its qualifier.
        assert!(ls.contains(&"TAccount.Deposit()".to_string()), "{ls:?}");
    }

    #[test]
    fn keywords_in_comments_are_ignored() {
        // `procedure`/`uses` inside `//`, `{ }`, and `(* *)` comments must not
        // produce nodes/imports; real declarations still do.
        let src = b"unit U;\n// procedure FakeLineComment;\n{ uses BogusBrace; }\n(* function FakeBlock; *)\nimplementation\nprocedure RealProc;\nbegin\nend;\nend.\n";
        let r = extract_pascal_source("src/U.pas", src);
        let ls = labels(&r);
        assert!(ls.contains(&"RealProc()".to_string()), "{ls:?}");
        assert!(!ls.iter().any(|l| l.contains("Fake")), "{ls:?}");
        let imps = targets(&r, "imports_from");
        assert!(!imps.iter().any(|t| t == "BogusBrace"), "{imps:?}");
    }

    #[test]
    fn delphi_uses_in_clause_keeps_unit_name() {
        let r = extract_pascal_source(
            "p.dpr",
            b"program P;\nuses Unit1 in 'Unit1.pas', Unit2;\nbegin\nend.\n",
        );
        let imps = targets(&r, "imports_from");
        assert!(imps.contains(&"Unit1".to_string()), "{imps:?}");
        assert!(imps.contains(&"Unit2".to_string()), "{imps:?}");
    }
}
