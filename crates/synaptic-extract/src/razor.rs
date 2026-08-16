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

/// A leading Razor directive: `@inherits X`, `@implements I`, `@inject T name`,
/// `@using N`. Anchored to the start of a line so `@inherits` inside prose or an
/// email address in the markup is not mistaken for one.
#[cfg(feature = "lang-razor")]
static DIRECTIVE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^[ \t]*@(inherits|implements|inject|using)[ \t]+([^\r\n]+)")
        .expect("valid razor directive regex")
});

/// The type a directive names: the first whitespace-delimited token, with any
/// generic argument list kept (`MudComponentBase<T>` stays whole) but a trailing
/// `;` or variable name dropped. `@inject IDialogService Dialog` names the
/// service, not the field.
#[cfg(feature = "lang-razor")]
fn directive_type(rest: &str) -> Option<&str> {
    let t = rest.split_whitespace().next()?.trim_end_matches(';');
    (!t.is_empty()
        && t.chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_'))
    .then_some(t)
}

/// The node for a type a directive names, creating an external stub when the
/// type is out of corpus (the usual case for framework base classes and
/// injected services) so the edge survives build's dangling-edge drop. Keyed on
/// the bare name, matching `Builder::ensure_named_node`'s global convention.
#[cfg(feature = "lang-razor")]
fn ensure_type_node(result: &mut ExtractionResult, name: &str) -> synaptic_core::NodeId {
    let id = synaptic_core::NodeId(synaptic_core::make_id(&[name]));
    if !result.nodes.iter().any(|n| n.id == id) {
        result.nodes.push(synaptic_core::Node {
            id: id.clone(),
            label: name.to_string(),
            file_type: synaptic_core::FileType::Code,
            source_file: String::new().into(),
            source_location: None,
            origin: Some("ast".into()),
            ..Default::default()
        });
    }
    id
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

    // Every Razor file declares a component -- the Razor compiler emits a class
    // whether or not the file has a `@code` block, and whether or not that block
    // is anything the C# grammar can read. Two real cases reach here with no
    // class node: a markup-only component (36.8% of a 1,999-file library), and a
    // `@code` block holding inline Razor templates (`@<div>...</div>` returning a
    // RenderFragment), which is valid Razor but not valid C#. Both were invisible.
    if !result.nodes.iter().any(|n| n.label == name) {
        let part = extract_csharp_source(path, format!("class {name}{{}}").as_bytes());
        result.nodes.extend(part.nodes);
        result.edges.extend(part.edges);
    }

    // The component is named by its file, never by any text inside it, so its
    // declaration site is the file itself. The synthesized `class` sits on the
    // `@code` line, which is an arbitrary place in the markup.
    // Every occurrence, not just the first: one delegation runs per `@code`
    // block, so a file with two blocks pushes the class node twice and
    // re-anchoring only the first left the surviving copy on its block's line.
    // Both the typed `span` and `source_location` have to move. Consumers read
    // the span first when it is present, so rewriting only `source_location`
    // left every component still reported at its `@code` line.
    let mut component_id = None;
    for n in result.nodes.iter_mut().filter(|n| n.label == name) {
        n.source_location = Some("L1".to_string());
        n.set_span(synaptic_core::span::Span {
            start_line: 1,
            start_col: 0,
            end_line: 1,
            end_col: 0,
        });
        component_id = Some(n.id.clone());
    }

    // Directives live in the markup, so delegating only `@code` never saw them.
    if let Some(component_id) = component_id {
        for caps in DIRECTIVE_RE.captures_iter(&text) {
            let line = text[..caps.get(0).map_or(0, |m| m.start())]
                .matches('\n')
                .count()
                + 1;
            let Some(ty) = directive_type(&caps[2]) else {
                continue;
            };
            match &caps[1] {
                "using" => result.imports.push(synaptic_core::ImportRecord {
                    local_name: ty.rsplit('.').next().unwrap_or(ty).to_string(),
                    imported_name: ty.to_string(),
                    module_stem: ty.rsplit('.').next().unwrap_or(ty).to_string(),
                    source_file: path.to_string(),
                    source_location: Some(format!("L{line}")),
                }),
                kind => {
                    let relation = match kind {
                        "inherits" => "inherits",
                        "implements" => "implements",
                        _ => "uses",
                    };
                    let target = ensure_type_node(&mut result, ty);
                    result.edges.push(synaptic_core::Edge {
                        source: component_id.clone(),
                        target,
                        relation: relation.to_string().into(),
                        confidence: synaptic_core::Confidence::Extracted,
                        source_file: path.to_string().into(),
                        source_location: Some(format!("L{line}")),
                        confidence_score: None,
                        weight: 1.0,
                        context: Some("razor_directive".into()),
                        cross_repo: false,
                        extra: Default::default(),
                    });
                }
            }
        }
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

    /// A Blazor component with no `@code` block is still a component -- the
    /// Razor compiler emits a class for it either way. Skipping delegation when
    /// there was no code block left 36.8% of a 1,999-file corpus (MudBlazor)
    /// with no node at all: not even a file node, so the file was invisible to
    /// every query.
    #[test]
    fn a_markup_only_component_is_still_declared() {
        let r = extract_razor_source("Pages/Plain.razor", b"<h1>Static</h1>\n<MudButton />\n");
        let ls = labels(&r);
        assert!(ls.contains(&"Plain".to_string()), "component: {ls:?}");
        assert!(ls.contains(&"Plain.razor".to_string()), "file node: {ls:?}");
    }

    /// A component is named by its file, so its declaration site is the file.
    /// Anchoring it at the `@code` block put every component in the corpus on an
    /// arbitrary line (0 of 1,261 were at line 1) and sent "go to definition"
    /// into the middle of the markup.
    #[test]
    fn the_component_is_anchored_at_the_file_not_the_code_block() {
        let src = b"@namespace MudBlazor\n<div>\n  <span>markup</span>\n</div>\n\n@code {\n    int count;\n}\n";
        let r = extract_razor_source("Components/MudAppBar.razor", src);
        let c = r
            .nodes
            .iter()
            .find(|n| n.label == "MudAppBar")
            .unwrap_or_else(|| panic!("component node; got {:?}", labels(&r)));
        assert_eq!(c.source_location.as_deref(), Some("L1"));
    }

    /// A `@code` block may hold an inline Razor template (`@<div>…</div>`
    /// returning a `RenderFragment`) -- valid Razor, not valid C#. The component
    /// must still be declared: its existence is a property of the file, not of
    /// whether its body parsed. MudChart, MudColorPicker and MudTimePicker all
    /// vanished from the graph this way.
    #[test]
    fn a_component_survives_a_code_block_csharp_cannot_read() {
        let src = br#"<div>markup</div>

@code {
    private RenderFragment ChartContainer => @<div @ref="_containerRef"
                                                   class="@Classname">
        @ChartContent
    </div>;
}
"#;
        let r = extract_razor_source("Components/MudChart.razor", src);
        let ls = labels(&r);
        assert!(ls.contains(&"MudChart".to_string()), "component: {ls:?}");
        let c = r.nodes.iter().find(|n| n.label == "MudChart").unwrap();
        assert_eq!(c.source_location.as_deref(), Some("L1"));
        assert_eq!(c.span().map(|s| s.start_line), Some(1));
        // The typed span must move too: consumers read it in preference to
        // `source_location`, so rewriting only the string left every component
        // still reported at its `@code` line.
        assert_eq!(c.span().map(|s| s.start_line), Some(1));
    }

    /// One delegation runs per `@code` block, so a file with two blocks emits
    /// the class node twice. Both must be re-anchored, or the copy that survives
    /// graph dedup keeps its block's line -- which is how four `App.razor`
    /// template files stayed mis-anchored.
    #[test]
    fn a_component_with_two_code_blocks_is_anchored_once_at_line_one() {
        let src = b"<div>x</div>\n\n@code {\n    int a;\n}\n\n<p>y</p>\n\n@code {\n    int b;\n}\n";
        let r = extract_razor_source("Components/App.razor", src);
        let comps: Vec<_> = r.nodes.iter().filter(|n| n.label == "App").collect();
        assert!(!comps.is_empty(), "{:?}", labels(&r));
        for c in comps {
            assert_eq!(c.source_location.as_deref(), Some("L1"));
            assert_eq!(c.span().map(|s| s.start_line), Some(1));
        }
    }

    /// Members inside `@code` keep their real lines; only the component itself
    /// moves to the file's line.
    #[test]
    fn code_members_keep_their_own_lines() {
        let src = b"<h1>x</h1>\n\n@code {\n    void Increment() { }\n}\n";
        let r = extract_razor_source("Pages/Counter.razor", src);
        let m = r
            .nodes
            .iter()
            .find(|n| n.label.contains("Increment"))
            .unwrap_or_else(|| panic!("method; got {:?}", labels(&r)));
        assert_eq!(m.source_location.as_deref(), Some("L4"));
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

    fn rels(r: &ExtractionResult, relation: &str) -> Vec<(String, String)> {
        let label = |id: &synaptic_core::NodeId| {
            r.nodes
                .iter()
                .find(|n| &n.id == id)
                .map(|n| n.label.clone())
                .unwrap_or_else(|| id.0.clone())
        };
        r.edges
            .iter()
            .filter(|e| e.relation == relation)
            .map(|e| (label(&e.source), label(&e.target)))
            .collect()
    }

    /// `@inherits` is how a Blazor component states its base class, and it
    /// appears in 12.4% of a 4,023-file corpus. It lives in the markup, so
    /// delegating only `@code` never saw it.
    #[test]
    fn inherits_directive_becomes_an_inherits_edge() {
        let src = b"@inherits MudComponentBase\n<div>x</div>\n";
        let r = extract_razor_source("Components/MudAlert.razor", src);
        assert!(
            rels(&r, "inherits").contains(&("MudAlert".into(), "MudComponentBase".into())),
            "{:?}",
            rels(&r, "inherits")
        );
    }

    #[test]
    fn implements_directive_becomes_an_implements_edge() {
        let src = b"@implements IDisposable\n@implements IAsyncDisposable\n<div>x</div>\n";
        let r = extract_razor_source("Components/Thing.razor", src);
        let imp = rels(&r, "implements");
        assert!(
            imp.contains(&("Thing".into(), "IDisposable".into())),
            "{imp:?}"
        );
        assert!(
            imp.contains(&("Thing".into(), "IAsyncDisposable".into())),
            "{imp:?}"
        );
    }

    /// `@inject` is Blazor's dependency injection, in 9.7% of files. The
    /// injected service is a real dependency of the component.
    #[test]
    fn inject_directive_becomes_a_uses_edge() {
        let src =
            b"@inject IDialogService DialogService\n@inject NavigationManager Nav\n<div>x</div>\n";
        let r = extract_razor_source("Pages/Index.razor", src);
        let uses = rels(&r, "uses");
        assert!(
            uses.contains(&("Index".into(), "IDialogService".into())),
            "{uses:?}"
        );
        assert!(
            uses.contains(&("Index".into(), "NavigationManager".into())),
            "{uses:?}"
        );
    }

    /// `@using` is an import and feeds cross-file resolution; 18.1% of files.
    #[test]
    fn using_directive_is_recorded_as_an_import() {
        let src = b"@using MudBlazor.Utilities\n@using System.Linq\n<div>x</div>\n";
        let r = extract_razor_source("Components/X.razor", src);
        let stems: Vec<&str> = r.imports.iter().map(|i| i.module_stem.as_str()).collect();
        assert!(stems.contains(&"Utilities"), "{stems:?}");
        assert!(stems.contains(&"Linq"), "{stems:?}");
    }

    /// A directive-looking line inside markup text or an email address must not
    /// be read as a directive.
    #[test]
    fn only_leading_directives_are_read() {
        let src = b"<p>write to @inherits.example.com</p>\n<div>@inject me</div>\n";
        let r = extract_razor_source("Components/Plain.razor", src);
        assert!(
            rels(&r, "inherits").is_empty(),
            "{:?}",
            rels(&r, "inherits")
        );
        assert!(rels(&r, "uses").is_empty(), "{:?}", rels(&r, "uses"));
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
