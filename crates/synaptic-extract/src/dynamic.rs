//! Reflection / dynamic-dispatch SITE detectors. Unlike crosslang's event-bus
//! detectors these emit no edges -- they record `DynamicSite`s on the enclosing
//! node so the query layer can warn that "0 dependents" may be incomplete. The
//! `text` passed in is already comment/docstring-masked by `crosslang::augment`.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use synaptic_core::{DynamicKind, DynamicSite};

use crate::crosslang::{attach_dynamic_site, line_of};
use crate::result::ExtractionResult;

/// Entry point called by `crosslang::augment`. Dispatches by extension.
pub fn scan(path: &str, text: &str, result: &mut ExtractionResult) {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => scan_js(path, text, result),
        "cs" => scan_dotnet(path, text, result),
        "py" => scan_python(path, text, result),
        "java" | "kt" | "kts" => scan_jvm(path, text, result),
        _ => {}
    }
}

/// Record one site at the byte offset of `m`, attributed to the enclosing node.
fn record(
    result: &mut ExtractionResult,
    path: &str,
    text: &str,
    m: &regex::Match,
    kind: DynamicKind,
    key: Option<String>,
) {
    let line = line_of(text, m.start());
    let snippet: String = m.as_str().trim().chars().take(120).collect();
    attach_dynamic_site(
        result,
        path,
        line,
        DynamicSite {
            kind,
            line,
            key,
            snippet,
        },
    );
}

fn scan_js(path: &str, text: &str, result: &mut ExtractionResult) {
    // computed-member call: obj['lit'](  -> keyed;  obj[expr](  -> opaque
    for caps in js_computed_call_re().captures_iter(text) {
        let m = caps.get(0).expect("group 0");
        let key = caps.get(1).map(|k| k.as_str().to_string());
        record(result, path, text, &m, DynamicKind::Reflection, key);
    }
    for m in reflect_re().find_iter(text) {
        record(result, path, text, &m, DynamicKind::Reflection, None);
    }
    for m in eval_re().find_iter(text) {
        record(result, path, text, &m, DynamicKind::Eval, None);
    }
    for caps in dynamic_import_re().captures_iter(text) {
        // a string-literal import('./x') is resolvable; flag only non-literal ones
        if caps.get(1).is_some() {
            continue;
        }
        let m = caps.get(0).expect("group 0");
        record(result, path, text, &m, DynamicKind::DynamicImport, None);
    }
}

fn scan_dotnet(path: &str, text: &str, result: &mut ExtractionResult) {
    for caps in dotnet_getmethod_re().captures_iter(text) {
        let m = caps.get(0).expect("group 0");
        let key = caps.get(1).map(|k| k.as_str().to_string());
        record(result, path, text, &m, DynamicKind::Reflection, key);
    }
    for m in dotnet_activator_re().find_iter(text) {
        record(result, path, text, &m, DynamicKind::Reflection, None);
    }
}

fn scan_python(path: &str, text: &str, result: &mut ExtractionResult) {
    for caps in py_getattr_re().captures_iter(text) {
        let m = caps.get(0).expect("group 0");
        let key = caps.get(1).map(|k| k.as_str().to_string());
        record(result, path, text, &m, DynamicKind::Reflection, key);
    }
    for m in py_import_re().find_iter(text) {
        record(result, path, text, &m, DynamicKind::DynamicImport, None);
    }
}

fn scan_jvm(path: &str, text: &str, result: &mut ExtractionResult) {
    for caps in jvm_reflect_re().captures_iter(text) {
        let m = caps.get(0).expect("group 0");
        let key = caps
            .get(1)
            .or_else(|| caps.get(2))
            .map(|k| k.as_str().to_string());
        record(result, path, text, &m, DynamicKind::Reflection, key);
    }
}

fn dotnet_getmethod_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"\.\s*(?:GetMethod|GetDeclaredMethod|GetProperty|GetField)\s*\(\s*"([A-Za-z0-9_]+)""#,
        )
        .expect("valid regex")
    })
}

fn dotnet_activator_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\bActivator\s*\.\s*CreateInstance\b|\bAssembly\s*\.\s*CreateInstance\b"#)
            .expect("valid regex")
    })
}

fn py_getattr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"\b(?:getattr|setattr|hasattr)\s*\(\s*[^,()]+,\s*(?:["']([A-Za-z0-9_]+)["'])?"#,
        )
        .expect("valid regex")
    })
}

fn py_import_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\bimportlib\s*\.\s*import_module\b|\b__import__\s*\("#).expect("valid regex")
    })
}

fn jvm_reflect_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"\bClass\s*\.\s*forName\s*\(\s*"([A-Za-z0-9_.$]+)"|\.\s*(?:getMethod|getDeclaredMethod|getField|getDeclaredField)\s*\(\s*"([A-Za-z0-9_]+)""#,
        )
        .expect("valid regex")
    })
}

fn js_computed_call_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"[A-Za-z_$][\w$]*\s*\[\s*(?:["'`]([A-Za-z0-9_$]+)["'`]|[^\]]+)\s*\]\s*\("#)
            .expect("valid regex")
    })
}

fn reflect_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\bReflect\s*\.\s*(?:get|set|apply|construct|has|deleteProperty)\b"#)
            .expect("valid regex")
    })
}

fn eval_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\beval\s*\(|\bnew\s+Function\s*\("#).expect("valid regex"))
}

fn dynamic_import_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\bimport\s*\(\s*(["'`][^"'`]+["'`])?"#).expect("valid regex"))
}
