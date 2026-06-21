//! Unified cross-repo alias resolution. Several JS/TS toolchains let one member
//! reference another by an **alias** that is decoupled from the target's
//! `package.json name`: import maps (`@PCMatic/Hub`), tsconfig `paths`
//! (`@app/*` → `../hub/src/*`), and webpack/Vite module-federation `remotes`
//! (`hub` imported as `hub/Button`). This module collects all of them into one
//! [`AliasMap`] of `alias → member tag`, consumed by cross-repo resolution.
//!
//! Import-map aliases are **exact** (the specifier equals the alias); tsconfig and
//! module-federation aliases are **prefix** (the specifier starts at the alias —
//! `@app/Button`, `hub/Widget`). [`AliasMap::resolve`] tries exact first, then the
//! longest matching prefix.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Files that may carry an alias declaration. `.ejs`/`.html` aren't classified as
/// code by `detect`, so this module scans by extension directly.
const CANDIDATE_EXTS: &[&str] = &["ejs", "html", "htm", "js", "mjs", "cjs", "ts", "json"];
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_DEPTH: usize = 8;

/// One alias declaration before it is resolved to a member. `targets` are strings
/// tested against member tags (an import-map value, a tsconfig path entry, a remote
/// URL, and/or — for module federation — the alias key itself); the first member
/// tag that is a path segment of any target wins.
pub(crate) struct RawAlias {
    pub alias: String,
    pub kind: AliasKind,
    pub targets: Vec<String>,
}

/// How an alias matches an import specifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AliasKind {
    /// The specifier must equal the alias (import maps).
    Exact,
    /// The specifier equals the alias or begins with `alias/…` (tsconfig `paths`,
    /// module-federation remotes).
    Prefix,
}

/// `alias → member tag`, split by match style. First insertion wins on duplicates
/// (callers iterate members/aliases in a deterministic order).
#[derive(Debug, Default, Clone)]
pub struct AliasMap {
    exact: BTreeMap<String, String>,
    prefix: BTreeMap<String, String>,
}

impl AliasMap {
    /// Record `alias → tag`. The first mapping for a given alias+kind wins.
    pub fn insert(&mut self, kind: AliasKind, alias: String, tag: String) {
        let map = match kind {
            AliasKind::Exact => &mut self.exact,
            AliasKind::Prefix => &mut self.prefix,
        };
        map.entry(alias).or_insert(tag);
    }

    /// Member tag for an import `specifier`: an exact alias match first, else the
    /// **longest** prefix alias whose boundary matches (`@app` matches `@app` and
    /// `@app/Button`, but never `@apple`).
    pub fn resolve(&self, specifier: &str) -> Option<&str> {
        if let Some(tag) = self.exact.get(specifier) {
            return Some(tag.as_str());
        }
        self.prefix
            .iter()
            .filter(|(alias, _)| {
                specifier == alias.as_str()
                    || specifier
                        .strip_prefix(alias.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|(alias, _)| alias.len())
            .map(|(_, tag)| tag.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.prefix.is_empty()
    }
}

/// Build the `alias → member tag` map from every alias source (import maps,
/// tsconfig `paths`, module-federation `remotes`) across the members' source trees.
/// `members` is `(tag, root)` pairs. One bounded walk per member dispatches each
/// candidate file to the parsers it matches; a self-alias (alias → its own member)
/// is dropped. Deterministic given the caller's member order (first mapping wins).
pub fn collect_aliases(members: &[(String, PathBuf)]) -> AliasMap {
    let tags: Vec<&str> = members.iter().map(|(t, _)| t.as_str()).collect();
    let mut map = AliasMap::default();
    for (owner, root) in members {
        let mut raws = Vec::new();
        scan_dir(root, MAX_DEPTH, &mut raws);
        for raw in raws {
            if let Some(tag) = match_member(&raw.targets, &tags) {
                if tag != owner.as_str() {
                    map.insert(raw.kind, raw.alias, tag.to_string());
                }
            }
        }
    }
    map
}

/// Member tag that appears as a path segment of any `target`. Splits on path,
/// scheme, and `name@url` separators so `hub@http://…/hub/…` yields a `hub` segment.
pub(crate) fn match_member<'a>(targets: &[String], tags: &[&'a str]) -> Option<&'a str> {
    for target in targets {
        let segs: Vec<&str> = target.split(['/', '\\', '@', ':', '?']).collect();
        if let Some(tag) = tags.iter().copied().find(|t| segs.contains(t)) {
            return Some(tag);
        }
    }
    None
}

/// Bounded, noise-pruned walk; reads each candidate file once and dispatches it to
/// the alias parser(s) whose name/content signature it matches.
fn scan_dir(dir: &Path, depth: usize, out: &mut Vec<RawAlias>) {
    if depth == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.filter_map(Result::ok) {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            let name = entry.file_name();
            if matches!(
                name.to_string_lossy().as_ref(),
                "node_modules" | ".git" | "synaptic-out"
            ) {
                continue;
            }
            scan_dir(&path, depth - 1, out);
        } else if ft.is_file() {
            let is_candidate = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| CANDIDATE_EXTS.contains(&e));
            if !is_candidate {
                continue;
            }
            if std::fs::metadata(&path)
                .map(|m| m.len())
                .unwrap_or(u64::MAX)
                > MAX_FILE_BYTES
            {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                dispatch(&path, &text, out);
            }
        }
    }
}

/// Pick the parser(s) for a file: `tsconfig*.json` → tsconfig (exclusive); otherwise
/// run the module-federation and import-map parsers, each a no-op if its key is
/// absent (a config can carry neither, either, or both).
fn dispatch(path: &Path, text: &str, out: &mut Vec<RawAlias>) {
    let fname = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    if ext == "json" && fname.starts_with("tsconfig") {
        crate::tsconfig::parse(text, out);
        return;
    }
    crate::module_federation::parse(text, out);
    crate::import_map::parse(text, out);
}

/// The balanced `{…}` object that follows `"<key>"` or `<key>` (first occurrence).
pub(crate) fn named_object(text: &str, key: &str) -> Option<String> {
    let at = text.find(key)?;
    let brace = text[at..].find('{')? + at;
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    for (off, &b) in bytes[brace..].iter().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[brace..brace + off + 1].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract `key: "value"` pairs from an object's text. Values must be string
/// literals (`"`, `'`, or template `` ` `` — interpolation is kept verbatim, which
/// is fine since we only match path segments). Keys may be quoted; bare identifier
/// keys (`hub`, `@scope/x`) are read only when `allow_unquoted_keys` (module
/// federation uses them; import maps always quote). Non-string values are skipped.
pub(crate) fn object_pairs(obj: &str, allow_unquoted_keys: bool) -> Vec<(String, String)> {
    let bytes = obj.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        let key = if c == '"' || c == '\'' {
            read_quoted(obj, i, c)
        } else if allow_unquoted_keys && is_key_start(c) {
            read_bare_key(obj, i)
        } else {
            None
        };
        let Some((key, after_key)) = key else {
            i += 1;
            continue;
        };
        let mut j = after_key;
        while j < bytes.len() && (bytes[j] as char).is_whitespace() {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b':' {
            j += 1;
            while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                j += 1;
            }
            if j < bytes.len() {
                let q = bytes[j] as char;
                if (q == '"' || q == '\'' || q == '`') && !key.is_empty() {
                    if let Some((val, vnext)) = read_quoted(obj, j, q) {
                        out.push((key, val));
                        i = vnext;
                        continue;
                    }
                }
            }
        }
        i = after_key;
    }
    out
}

fn is_key_start(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '@' || c == '_'
}

/// Read a bare (unquoted) object key — identifier plus the chars seen in scoped
/// package names. Returns the key and the index just past it.
fn read_bare_key(s: &str, start: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_ascii_alphanumeric() || matches!(c, '@' | '_' | '/' | '.' | '-') {
            i += 1;
        } else {
            break;
        }
    }
    (i > start).then(|| (s[start..i].to_string(), i))
}

/// Read a `q`-quoted string starting at `start` (the opening quote). Returns the
/// inner content and the index just past the closing quote. No escape handling
/// (alias targets don't need it).
pub(crate) fn read_quoted(s: &str, start: usize, q: char) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] as char == q {
            return Some((s[start + 1..i].to_string(), i + 1));
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_resolves_nothing() {
        let m = AliasMap::default();
        assert!(m.is_empty());
        assert_eq!(m.resolve("@app/x"), None);
    }

    #[test]
    fn exact_beats_prefix() {
        let mut m = AliasMap::default();
        m.insert(AliasKind::Prefix, "@app".into(), "wrong".into());
        m.insert(AliasKind::Exact, "@app/Button".into(), "right".into());
        assert_eq!(m.resolve("@app/Button"), Some("right"));
    }

    #[test]
    fn prefix_matches_subpath_and_self() {
        let mut m = AliasMap::default();
        m.insert(AliasKind::Prefix, "@app".into(), "hub".into());
        assert_eq!(m.resolve("@app"), Some("hub"));
        assert_eq!(m.resolve("@app/Button"), Some("hub"));
        assert_eq!(m.resolve("@app/nested/deep"), Some("hub"));
    }

    #[test]
    fn prefix_respects_segment_boundary() {
        let mut m = AliasMap::default();
        m.insert(AliasKind::Prefix, "@app".into(), "hub".into());
        // Not a path-segment boundary, no match.
        assert_eq!(m.resolve("@apple"), None);
        assert_eq!(m.resolve("@app-ui/x"), None);
    }

    #[test]
    fn longest_prefix_wins() {
        let mut m = AliasMap::default();
        m.insert(AliasKind::Prefix, "@app".into(), "hub".into());
        m.insert(AliasKind::Prefix, "@app/ui".into(), "design".into());
        assert_eq!(m.resolve("@app/ui/Button"), Some("design"));
        assert_eq!(m.resolve("@app/core"), Some("hub"));
    }

    #[test]
    fn first_insertion_wins() {
        let mut m = AliasMap::default();
        m.insert(AliasKind::Exact, "@x".into(), "first".into());
        m.insert(AliasKind::Exact, "@x".into(), "second".into());
        assert_eq!(m.resolve("@x"), Some("first"));
    }

    #[test]
    fn match_member_splits_name_at_url_and_path() {
        let tags = ["hub", "vpn"];
        assert_eq!(
            match_member(&["hub@http://x/remoteEntry.js".into()], &tags),
            Some("hub")
        );
        assert_eq!(
            match_member(&["http://h/vpn/assets/x.js".into()], &tags),
            Some("vpn")
        );
        assert_eq!(match_member(&["../other/src/x".into()], &tags), None);
    }

    fn touch(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn collect_aliases_merges_all_sources() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        // Member `app` declares three kinds of alias pointing at siblings.
        touch(
            r,
            "app/tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@app/*": ["../hub/src/*"] } } }"#,
        );
        touch(
            r,
            "app/webpack.config.js",
            r#"new ModuleFederationPlugin({ remotes: { vpn: 'vpn@http://x/remoteEntry.js' } })"#,
        );
        touch(
            r,
            "app/public/index.ejs",
            r#"<script>const m = { imports: { "@x/Lib": `/libs/lib/dist/index.js` } }</script>"#,
        );
        // A self-alias must be dropped (app -> app).
        touch(
            r,
            "app/self.importmap.json",
            r#"{ "imports": { "@self": "/app/dist/x.js" } }"#,
        );
        for m in ["app", "hub", "vpn", "lib"] {
            touch(r, &format!("{m}/package.json"), "{}");
        }
        let members = vec![
            ("app".to_string(), r.join("app")),
            ("hub".to_string(), r.join("hub")),
            ("vpn".to_string(), r.join("vpn")),
            ("lib".to_string(), r.join("lib")),
        ];
        let m = collect_aliases(&members);
        assert_eq!(m.resolve("@app/Button"), Some("hub"), "tsconfig prefix");
        assert_eq!(m.resolve("vpn/RemoteApp"), Some("vpn"), "module federation");
        assert_eq!(m.resolve("@x/Lib"), Some("lib"), "import map exact");
        assert_eq!(m.resolve("@self"), None, "self-alias dropped");
    }
}
