//! Razor / Blazor components (`.razor`, `.cshtml`) — feature `lang-razor`.
//! A Razor file is HTML markup with embedded C# in `@code { … }` / `@functions
//! { … }` blocks (which the Razor compiler emits as the component's partial
//! class). We extract those blocks and delegate to the C# extractor, wrapping
//! each in `class <ComponentName> { … }` so the members parse as class members —
//! the same "extract the script, delegate to the real language" approach as the
//! Vue/Svelte/Astro web-framework extractor.
//!
//! Block bodies are newline-padded to their original offset so node line numbers
//! line up with the `.razor`/`.cshtml` file.

#[cfg(feature = "lang-razor")]
use std::sync::LazyLock;

#[cfg(feature = "lang-razor")]
use regex::Regex;

#[cfg(feature = "lang-razor")]
use crate::csharp::extract_csharp_source;
#[cfg(feature = "lang-razor")]
use crate::result::ExtractionResult;

#[cfg(feature = "lang-razor")]
static CODE_KW_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)@(?:code|functions)\b").expect("valid razor @code regex"));

/// Index of the `}` matching the `{` at `open`, skipping braces inside string/
/// char literals and `//` / `/* */` comments so a `"}"` or `// }` in the C#
/// body doesn't close the block early. (C# verbatim/interpolated strings aren't
/// modeled — a rare edge for `@code`.) `None` if unbalanced.
#[cfg(feature = "lang-razor")]
fn match_brace(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let n = bytes.len();
    let mut depth = 0usize;
    let mut i = open;
    while i < n {
        match bytes[i] {
            b'"' | b'\'' => {
                i = skip_literal(bytes, i);
                continue;
            }
            b'/' if i + 1 < n && bytes[i + 1] == b'/' => {
                while i < n && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if i + 1 < n && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Index just past a `"…"`/`'…'` literal that opens at `start` (handles `\`
/// escapes). Byte-level scanning is safe: the delimiters/escape are ASCII.
#[cfg(feature = "lang-razor")]
fn skip_literal(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let n = bytes.len();
    let mut i = start + 1;
    while i < n {
        match bytes[i] {
            b'\\' => i += 2,
            b if b == quote => return i + 1,
            _ => i += 1,
        }
    }
    n
}

/// `(body_start_byte, body)` for each `@code`/`@functions { … }` block.
#[cfg(feature = "lang-razor")]
fn code_blocks(source: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    for m in CODE_KW_RE.find_iter(source) {
        let Some(open_rel) = source[m.end()..].find('{') else {
            continue;
        };
        let open = m.end() + open_rel;
        if let Some(close) = match_brace(source, open) {
            out.push((open + 1, &source[open + 1..close]));
        }
    }
    out
}

/// A valid C# identifier from the file stem (component class name).
#[cfg(feature = "lang-razor")]
fn component_name(path: &str) -> String {
    let stem = std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Component".to_string());
    let mut name: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if !name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        name.insert(0, '_');
    }
    name
}

/// Extract a Razor/Blazor component already in memory.
#[cfg(feature = "lang-razor")]
pub fn extract_razor_source(path: &str, source: &[u8]) -> ExtractionResult {
    let text = String::from_utf8_lossy(source);
    let name = component_name(path);
    let mut result = ExtractionResult::default();
    for (body_start, body) in code_blocks(&text) {
        // Pad to the body's start line, then wrap in the component class on that
        // same line so the body's own line numbers are preserved.
        let pad = text[..body_start].matches('\n').count();
        let mut synth = "\n".repeat(pad);
        synth.push_str(&format!("class {name}{{"));
        synth.push_str(body);
        synth.push('}');
        let part = extract_csharp_source(path, synth.as_bytes());
        result.nodes.extend(part.nodes);
        result.edges.extend(part.edges);
        result.raw_calls.extend(part.raw_calls);
        result.imports.extend(part.imports);
    }
    result
}

/// Read and extract a Razor/Blazor file from disk.
#[cfg(feature = "lang-razor")]
pub fn extract_razor_file(path: &std::path::Path) -> std::io::Result<ExtractionResult> {
    let source = std::fs::read(path)?;
    let path_str = path.to_string_lossy();
    Ok(extract_razor_source(&path_str, &source))
}

#[cfg(all(test, feature = "lang-razor"))]
mod tests {
    use super::*;

    fn labels(r: &ExtractionResult) -> Vec<String> {
        r.nodes.iter().map(|n| n.label.clone()).collect()
    }

    #[test]
    fn code_block_methods_delegate_to_csharp() {
        let src = b"<h1>Counter</h1>\n<p>@count</p>\n@code {\n    private int count = 0;\n    void Increment() { count++; }\n}\n";
        let r = extract_razor_source("Pages/Counter.razor", src);
        let ls = labels(&r);
        assert!(
            ls.contains(&"Counter".to_string()),
            "component class: {ls:?}"
        );
        assert!(
            ls.iter().any(|l| l == ".Increment()" || l == "Increment()"),
            "method: {ls:?}"
        );
    }

    #[test]
    fn no_code_block_is_empty() {
        let r = extract_razor_source("Pages/Plain.razor", b"<h1>Static</h1>\n");
        assert!(r.nodes.is_empty(), "{:?}", labels(&r));
    }

    #[test]
    fn brace_in_string_or_comment_does_not_close_block_early() {
        // The `}` in the string and the `// }` comment must not end @code; the
        // method after them must still be extracted.
        let src = b"@code {\n    string s = \"}\";\n    // closing } here\n    void After() { return; }\n}\n";
        let r = extract_razor_source("Pages/Tricky.razor", src);
        assert!(
            labels(&r).iter().any(|l| l.contains("After")),
            "method after a brace-in-string was dropped: {:?}",
            labels(&r)
        );
    }

    #[test]
    fn functions_block_also_extracted() {
        // Classic .cshtml uses @functions.
        let src = b"@functions {\n    public string Greet() { return \"hi\"; }\n}\n";
        let r = extract_razor_source("Views/Home.cshtml", src);
        assert!(
            labels(&r).iter().any(|l| l.contains("Greet")),
            "{:?}",
            labels(&r)
        );
    }
}
