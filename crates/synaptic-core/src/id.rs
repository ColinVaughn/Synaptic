use std::sync::LazyLock;

use caseless::default_case_fold_str;
use regex::Regex;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

static NON_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^\w]+").expect("valid non-word regex"));
static MULTI_UNDERSCORE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"_+").expect("valid underscore-collapse regex"));

/// Stable, namespaced node identifier. Serializes transparently as a string so
/// `graph.json` node/edge endpoints stay plain strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub String);

impl NodeId {
    /// Build a `NodeId` from name parts using [`make_id`].
    pub fn new(parts: &[&str]) -> NodeId {
        NodeId(make_id(parts))
    }

    /// Borrow the inner string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Build a stable node ID from one or more name parts.
///
/// Join `strip("_.")`'d non-empty parts with `_`, NFKC-normalize, replace non-word
/// runs with `_`, collapse `_+`, strip `_`, then Unicode case-fold. Case folding
/// (not lowercasing) is required so `straße` -> `strasse`.
pub fn make_id(parts: &[&str]) -> String {
    let combined = parts
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| p.trim_matches(|c: char| c == '_' || c == '.'))
        .collect::<Vec<_>>()
        .join("_");

    let normalized: String = combined.nfkc().collect();
    let cleaned = NON_WORD.replace_all(&normalized, "_");
    let cleaned = MULTI_UNDERSCORE.replace_all(&cleaned, "_");
    default_case_fold_str(cleaned.trim_matches('_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_dots_and_underscores() {
        assert_eq!(make_id(&["_auth"]), "auth");
        assert_eq!(make_id(&[".httpx._client"]), "httpx_client");
    }

    #[test]
    fn is_deterministic() {
        assert_eq!(make_id(&["foo", "Bar"]), make_id(&["foo", "Bar"]));
        assert_eq!(make_id(&["foo", "Bar"]), "foo_bar");
    }

    #[test]
    fn no_leading_trailing_underscores() {
        let r = make_id(&["__init__"]);
        assert!(!r.starts_with('_'));
        assert!(!r.ends_with('_'));
        assert_eq!(r, "init");
    }

    #[test]
    fn hyphen_becomes_underscore() {
        assert_eq!(make_id(&["my-script", "main"]), "my_script_main");
    }

    #[test]
    fn preserves_unicode_word_chars() {
        // é is a Unicode word char and must survive (casefold of é is é).
        assert_eq!(make_id(&["café", "run"]), "café_run");
    }

    #[test]
    fn casefold_not_lowercase() {
        // German sharp s: casefold maps ß -> ss; lowercase would keep ß. ID
        // construction requires casefold so these collapse identically.
        assert_eq!(make_id(&["straße"]), "strasse");
    }

    #[test]
    fn cjk_preserved_and_not_collapsed() {
        // #811: non-ASCII identifiers must not collapse to underscores.
        let s = "中".repeat(300);
        let r = make_id(&[s.as_str()]);
        assert_eq!(r, s); // 中 is unchanged by NFKC + casefold
        assert!(!r.contains('_'));
    }

    #[test]
    fn skips_empty_parts() {
        assert_eq!(make_id(&["", "auth", ""]), "auth");
    }

    #[test]
    fn nodeid_roundtrips_as_plain_string() {
        let id = NodeId::new(&["foo", "Bar"]);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"foo_bar\"");
        let back: NodeId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }
}
