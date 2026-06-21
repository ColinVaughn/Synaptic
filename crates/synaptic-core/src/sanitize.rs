//! Label and metadata sanitization shared by the server and ingest layers.
//! A security boundary on LLM/corpus/external-index-derived text before it's
//! embedded in tool output or `graph.json`.

use serde_json::{Map, Value};

const MAX_LABEL_LEN: usize = 256;
const METADATA_MAX_VALUE_LEN: usize = 512;
const METADATA_MAX_LIST_ITEMS: usize = 50;

/// True for the control characters we strip: `\x00-\x1f` and `\x7f`.
/// (Tab/newline are control chars → stripped.)
fn is_strippable_control(c: char) -> bool {
    let n = c as u32;
    (c.is_control() && n <= 0x1f) || n == 0x7f
}

/// Strip control characters (`\x00-\x1f` and `\x7f`) and cap length to 256
/// chars. Safe for embedding in plain text / JSON; for direct HTML injection,
/// additionally escape the result.
pub fn sanitize_label(text: &str) -> String {
    text.chars()
        .filter(|c| !is_strippable_control(*c))
        .take(MAX_LABEL_LEN)
        .collect()
}

/// HTML-escape like Python's `html.escape(s, quote=True)`: `&`, `<`, `>`, `"`,
/// `'`. `&` is replaced first so the entities it introduces aren't re-escaped.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

/// Strip control chars, HTML-escape, and cap to 512 chars — the per-value rule
/// for metadata strings.
fn sanitize_metadata_string(value: &str) -> String {
    let stripped: String = value
        .chars()
        .filter(|c| !is_strippable_control(*c))
        .collect();
    html_escape(&stripped)
        .chars()
        .take(METADATA_MAX_VALUE_LEN)
        .collect()
}

/// Sanitize one metadata value, preserving JSON-compatible types. Strings are
/// stripped + escaped + capped; objects recurse; arrays are capped to 50 items
/// and each item recurses; numbers/bools/null pass through unchanged.
pub fn sanitize_metadata_value(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(sanitize_metadata_string(s)),
        Value::Object(m) => Value::Object(sanitize_metadata(m)),
        Value::Array(a) => Value::Array(
            a.iter()
                .take(METADATA_MAX_LIST_ITEMS)
                .map(sanitize_metadata_value)
                .collect(),
        ),
        // Bool / Number / Null pass through.
        other => other.clone(),
    }
}

/// Sanitize a metadata map before graph export: keys are stripped + escaped +
/// capped (entries with an empty key are dropped), values go through
/// [`sanitize_metadata_value`]. Metadata is less constrained than labels — it
/// can hold nested dicts, lists, source snippets, external-index symbols, and
/// docstring text — so this keeps it JSON-compatible and HTML-safe.
pub fn sanitize_metadata(metadata: &Map<String, Value>) -> Map<String, Value> {
    let mut result = Map::new();
    for (key, value) in metadata {
        let clean_key = sanitize_metadata_string(key);
        if clean_key.is_empty() {
            continue;
        }
        result.insert(clean_key, sanitize_metadata_value(value));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_control_chars() {
        assert_eq!(sanitize_label("a\u{0}b\u{1f}c\u{7f}d"), "abcd");
        assert_eq!(sanitize_label("normal label"), "normal label");
        // Tab/newline are control chars, so stripped.
        assert_eq!(sanitize_label("a\tb\nc"), "abc");
    }

    #[test]
    fn caps_length_at_256() {
        let long = "x".repeat(300);
        assert_eq!(sanitize_label(&long).chars().count(), 256);
    }

    #[test]
    fn keeps_unicode_letters() {
        assert_eq!(sanitize_label("café 中"), "café 中");
    }

    #[test]
    fn metadata_string_strips_escapes_and_caps() {
        // Control chars stripped, HTML-sensitive chars escaped (quote=True).
        let v = sanitize_metadata_value(&serde_json::json!("a<b>\"c\"'d'&e\u{0}f"));
        assert_eq!(
            v,
            serde_json::json!("a&lt;b&gt;&quot;c&quot;&#x27;d&#x27;&amp;ef")
        );
        // 512-char cap (after escaping).
        let long = "x".repeat(600);
        let capped = sanitize_metadata_value(&serde_json::json!(long));
        assert_eq!(capped.as_str().unwrap().chars().count(), 512);
    }

    #[test]
    fn metadata_recurses_and_drops_empty_keys() {
        let mut m = serde_json::Map::new();
        m.insert("scip_symbol".into(), serde_json::json!("Foo#bar()."));
        m.insert("nested".into(), serde_json::json!({ "x": "<i>", "y": 3 }));
        m.insert("list".into(), serde_json::json!(["<a>", true, 1]));
        m.insert("\u{0}".into(), serde_json::json!("dropped")); // key empties out
        let out = sanitize_metadata(&m);
        assert!(!out.contains_key("")); // empty-after-sanitize key dropped
        assert_eq!(
            out["nested"],
            serde_json::json!({ "x": "&lt;i&gt;", "y": 3 })
        );
        assert_eq!(out["list"], serde_json::json!(["&lt;a&gt;", true, 1]));
        // Numbers / bools survive untouched.
        assert_eq!(out["nested"]["y"], serde_json::json!(3));
    }

    #[test]
    fn metadata_list_capped_at_50() {
        let big: Vec<i64> = (0..80).collect();
        let out = sanitize_metadata_value(&serde_json::json!(big));
        assert_eq!(out.as_array().unwrap().len(), 50);
    }
}
