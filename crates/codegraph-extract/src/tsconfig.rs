//! tsconfig / jsconfig path-alias resolution for JS/TS imports.
//!
//! `import { api } from '@/lib/api'` is a TypeScript *path alias*: `tsc` rewrites
//! `@/...` via `compilerOptions.paths` (+ `baseUrl`) declared in a
//! `tsconfig.json` / `jsconfig.json`. Those files are not in the code corpus, so
//! the per-file extractor leaves the alias as a stub; this module parses the
//! configs into a pure [`AliasResolver`] that the cross-file
//! [`crate::resolve::resolve_imports`] pass uses to bind the alias to its real
//! file.
//!
//! Scope (per the design): nearest-ancestor config, `baseUrl` + `paths` globs,
//! and a single `extends` level. Resolution goes **only** through explicit
//! `paths` patterns — never a bare `baseUrl` fallback — so a bare package like
//! `react` is never turned into a phantom path. The [`AliasResolver`] is
//! constructed from in-memory entries (filesystem-free + unit-testable); only
//! [`load_alias_resolver`] touches disk.

use std::path::{Path, PathBuf};

/// One config's alias data, prepared for nearest-ancestor lookup.
#[derive(Debug, Clone, PartialEq)]
pub struct AliasEntry {
    /// Directory the `base_url`/`paths` resolve against, posix and relative to the
    /// scan root (`""` = root). This is the dir of whichever config actually
    /// *provided* the `paths` (the child, or — when the child only `extends` — the
    /// extended base config).
    pub config_dir: String,
    /// `compilerOptions.baseUrl`, posix, relative to `config_dir` (`.` if unset).
    pub base_url: String,
    /// `compilerOptions.paths`, sorted most-specific first.
    pub paths: Vec<(String, Vec<String>)>,
}

/// Pure alias resolver: a set of [`AliasEntry`]s sorted by `config_dir` depth
/// descending, so a linear scan finds the nearest-ancestor config first.
#[derive(Debug, Clone, Default)]
pub struct AliasResolver {
    entries: Vec<AliasEntry>,
}

impl AliasResolver {
    /// Build from in-memory entries (used directly by tests). Sorts for
    /// nearest-ancestor lookup and most-specific-pattern-first matching.
    pub fn from_entries(mut entries: Vec<AliasEntry>) -> Self {
        for e in &mut entries {
            // Most-specific pattern first (descending specificity).
            e.paths
                .sort_by_key(|(pattern, _)| std::cmp::Reverse(pattern_specificity(pattern)));
        }
        // Deepest config_dir first; dedupe identical entries (two children that
        // extend the same root base both yield the base entry).
        entries.sort_by(|a, b| {
            dir_depth(&b.config_dir)
                .cmp(&dir_depth(&a.config_dir))
                .then_with(|| a.config_dir.cmp(&b.config_dir))
        });
        entries.dedup();
        Self { entries }
    }

    /// True when no config contributed any alias (the common non-JS / no-alias
    /// case); resolution is then a no-op.
    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(|e| e.paths.is_empty())
    }

    /// Candidate resolved paths (posix, relative to root) for `spec` imported from
    /// `importer`, most-specific first. Empty when `spec` matches no `paths`
    /// pattern of the nearest-ancestor config (e.g. a bare package).
    pub fn resolve(&self, importer: &str, spec: &str) -> Vec<String> {
        let importer = importer.replace('\\', "/");
        let importer_dir = importer.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let Some(entry) = self
            .entries
            .iter()
            .find(|e| dir_is_ancestor(&e.config_dir, importer_dir))
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (pattern, replacements) in &entry.paths {
            if let Some(cands) = match_pattern(pattern, replacements, spec) {
                for rel in cands {
                    if let Some(p) = normalize_join(&[&entry.config_dir, &entry.base_url, &rel]) {
                        out.push(p);
                    }
                }
            }
        }
        out
    }
}

/// Sort key: exact (non-wildcard) patterns are most specific; otherwise the
/// length of the literal prefix before `*`.
fn pattern_specificity(pattern: &str) -> usize {
    match pattern.find('*') {
        None => usize::MAX,
        Some(i) => i,
    }
}

fn dir_depth(dir: &str) -> usize {
    if dir.is_empty() {
        0
    } else {
        dir.split('/').count()
    }
}

/// Does `config_dir` contain `importer_dir`? Root (`""`) contains everything.
fn dir_is_ancestor(config_dir: &str, importer_dir: &str) -> bool {
    config_dir.is_empty()
        || importer_dir == config_dir
        || importer_dir.starts_with(&format!("{config_dir}/"))
}

/// Match a single `paths` pattern against `spec`, yielding the substituted
/// replacements (the `*` capture spliced into each), or `None` if no match.
fn match_pattern(pattern: &str, replacements: &[String], spec: &str) -> Option<Vec<String>> {
    match pattern.find('*') {
        Some(star) => {
            let prefix = &pattern[..star];
            let suffix = &pattern[star + 1..];
            if spec.len() < prefix.len() + suffix.len()
                || !spec.starts_with(prefix)
                || !spec.ends_with(suffix)
            {
                return None;
            }
            let cap = &spec[prefix.len()..spec.len() - suffix.len()];
            Some(
                replacements
                    .iter()
                    .map(|r| {
                        if r.contains('*') {
                            r.replacen('*', cap, 1)
                        } else {
                            r.clone()
                        }
                    })
                    .collect(),
            )
        }
        None => (spec == pattern).then(|| replacements.to_vec()),
    }
}

/// Join path fragments and normalize `.`/`..`/empty components (posix). `None` if
/// it climbs above the root.
fn normalize_join(parts: &[&str]) -> Option<String> {
    let mut comps: Vec<&str> = Vec::new();
    for part in parts {
        for comp in part.split('/') {
            match comp {
                "" | "." => {}
                ".." => {
                    comps.pop()?;
                }
                other => comps.push(other),
            }
        }
    }
    Some(comps.join("/"))
}

// Filesystem loading (the only impure part).

/// Parse `config_paths` (discovered by `codegraph-detect`) into an
/// [`AliasResolver`]. `root` is the scan root; entry dirs are made relative to it
/// (posix). Unreadable / malformed configs are skipped with a note — never fatal.
pub fn load_alias_resolver(root: &Path, config_paths: &[PathBuf]) -> AliasResolver {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut entries = Vec::new();
    for cfg in config_paths {
        match load_entry(&root, cfg) {
            Ok(Some(entry)) => entries.push(entry),
            Ok(None) => {}
            Err(e) => eprintln!("note: skipping {} ({e})", cfg.display()),
        }
    }
    AliasResolver::from_entries(entries)
}

/// `compilerOptions` slice we care about, plus where it was declared.
struct Parsed {
    base_url: Option<String>,
    paths: Option<Vec<(String, Vec<String>)>>,
    extends: Option<String>,
}

/// Load one config file → its `baseUrl`/`paths`/`extends` (no extends following).
fn load_one(path: &Path) -> std::io::Result<Parsed> {
    let raw = std::fs::read_to_string(path)?;
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw); // drop UTF-8 BOM
    let stripped = strip_jsonc(raw);
    let v: serde_json::Value = serde_json::from_str(&stripped)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let extends = v
        .get("extends")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let co = v.get("compilerOptions");
    let base_url = co
        .and_then(|c| c.get("baseUrl"))
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let paths = co
        .and_then(|c| c.get("paths"))
        .and_then(|x| x.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, val)| {
                    let reps = val
                        .as_array()?
                        .iter()
                        .filter_map(|r| r.as_str().map(str::to_string))
                        .collect::<Vec<_>>();
                    Some((k.clone(), reps))
                })
                .collect::<Vec<_>>()
        });
    Ok(Parsed {
        base_url,
        paths,
        extends,
    })
}

/// Resolve the effective alias entry for one discovered config, following one
/// `extends` level. The `paths` come wholesale from whichever config defines them
/// (child wins); `base_url`/`config_dir` track the *defining* config's directory,
/// matching `tsc`.
fn load_entry(root: &Path, cfg_path: &Path) -> std::io::Result<Option<AliasEntry>> {
    let child = load_one(cfg_path)?;
    let child_dir = cfg_path.parent().unwrap_or(Path::new(""));

    // (paths, owner_dir, base_url): defining config wins.
    let (paths, owner_dir, base_url) = if let Some(p) = child.paths.clone() {
        (p, child_dir.to_path_buf(), child.base_url.clone())
    } else if let Some(ext) = &child.extends {
        // Follow one extends level; the base's paths resolve against the base dir.
        let base_path = child_dir.join(ext);
        match load_one(&base_path) {
            Ok(base) => match base.paths {
                Some(p) => (
                    p,
                    base_path.parent().unwrap_or(Path::new("")).to_path_buf(),
                    base.base_url,
                ),
                None => return Ok(None),
            },
            Err(_) => return Ok(None), // missing/unreadable extends: no aliases
        }
    } else {
        return Ok(None);
    };

    let config_dir = rel_posix(root, &owner_dir);
    Ok(Some(AliasEntry {
        config_dir,
        base_url: base_url.unwrap_or_else(|| ".".to_string()),
        paths,
    }))
}

/// `dir` relative to `root`, posix, `""` for the root itself.
fn rel_posix(root: &Path, dir: &Path) -> String {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    dir.strip_prefix(root)
        .unwrap_or(&dir)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Strip `//` line and `/* */` block comments and trailing commas from JSONC,
/// leaving string literals untouched. tsconfig files routinely contain both, and
/// `serde_json` rejects both.
fn strip_jsonc(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut in_str = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            out.push(b as char);
            if b == b'\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => {
                in_str = true;
                out.push('"');
                i += 1;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2; // consume the closing */
            }
            // Non-ASCII byte inside a non-string region (rare): copy via the
            // original str slice to stay UTF-8-correct.
            _ if b >= 0x80 => {
                let ch_len = utf8_len(b);
                out.push_str(&s[i..(i + ch_len).min(s.len())]);
                i += ch_len;
            }
            _ => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    strip_trailing_commas(&out)
}

fn utf8_len(first: u8) -> usize {
    match first {
        b if b >= 0xF0 => 4,
        b if b >= 0xE0 => 3,
        b if b >= 0xC0 => 2,
        _ => 1,
    }
}

/// Remove commas that are immediately followed (ignoring whitespace) by `}` or
/// `]`, outside string literals.
fn strip_trailing_commas(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            out.push(b as char);
            if b == b'\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_str = true;
            out.push('"');
            i += 1;
            continue;
        }
        if b == b',' {
            // Look ahead past whitespace for a closing bracket.
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'}' || bytes[j] == b']') {
                i += 1; // drop the comma
                continue;
            }
        }
        if b >= 0x80 {
            let ch_len = utf8_len(b);
            out.push_str(&s[i..(i + ch_len).min(s.len())]);
            i += ch_len;
            continue;
        }
        out.push(b as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(dir: &str, base: &str, paths: &[(&str, &[&str])]) -> AliasEntry {
        AliasEntry {
            config_dir: dir.to_string(),
            base_url: base.to_string(),
            paths: paths
                .iter()
                .map(|(p, r)| (p.to_string(), r.iter().map(|s| s.to_string()).collect()))
                .collect(),
        }
    }

    #[test]
    fn wildcard_paths_resolve_against_baseurl() {
        let r = AliasResolver::from_entries(vec![entry("", ".", &[("@/*", &["src/*"])])]);
        assert_eq!(
            r.resolve("src/app/Foo.tsx", "@/lib/api"),
            vec!["src/lib/api".to_string()]
        );
    }

    #[test]
    fn exact_pattern_resolves() {
        let r = AliasResolver::from_entries(vec![entry("", ".", &[("@app", &["src/app/index"])])]);
        assert_eq!(
            r.resolve("src/x.ts", "@app"),
            vec!["src/app/index".to_string()]
        );
    }

    #[test]
    fn bare_package_yields_no_candidates() {
        let r = AliasResolver::from_entries(vec![entry("", ".", &[("@/*", &["src/*"])])]);
        assert!(r.resolve("src/x.ts", "react").is_empty());
        assert!(!r.is_empty());
    }

    #[test]
    fn nearest_ancestor_config_wins() {
        let root = entry("", ".", &[("@/*", &["src/*"])]);
        let sub = entry("packages/app", ".", &[("@/*", &["app-src/*"])]);
        let r = AliasResolver::from_entries(vec![root, sub]);
        // Importer under packages/app uses the sub config.
        assert_eq!(
            r.resolve("packages/app/x.ts", "@/util"),
            vec!["packages/app/app-src/util".to_string()]
        );
        // Importer elsewhere falls back to the root config.
        assert_eq!(
            r.resolve("web/y.ts", "@/util"),
            vec!["src/util".to_string()]
        );
    }

    #[test]
    fn more_specific_pattern_is_tried_first() {
        let r = AliasResolver::from_entries(vec![entry(
            "",
            ".",
            &[("@/*", &["src/*"]), ("@/components/*", &["src/ui/*"])],
        )]);
        let cands = r.resolve("src/x.ts", "@/components/Button");
        assert_eq!(cands.first().map(String::as_str), Some("src/ui/Button"));
    }

    #[test]
    fn baseurl_is_applied() {
        let r = AliasResolver::from_entries(vec![entry("", "./src", &[("@/*", &["*"])])]);
        assert_eq!(r.resolve("src/x.ts", "@/lib/api"), vec!["src/lib/api"]);
    }

    #[test]
    fn strip_jsonc_removes_comments_and_trailing_commas() {
        let src = r#"{
            // line comment
            "compilerOptions": {
              "baseUrl": ".", /* block */
              "paths": { "@/*": ["src/*"], },
            },
        }"#;
        let cleaned = strip_jsonc(src);
        let v: serde_json::Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(v["compilerOptions"]["baseUrl"], ".");
    }

    #[test]
    fn strip_jsonc_keeps_slashes_inside_strings() {
        let src = r#"{ "url": "http://x/y", "glob": "a/*" }"#;
        let cleaned = strip_jsonc(src);
        let v: serde_json::Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(v["url"], "http://x/y");
        assert_eq!(v["glob"], "a/*");
    }

    #[test]
    fn load_resolver_from_disk_with_jsonc() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tsconfig.json"),
            "{\n  // comment\n  \"compilerOptions\": {\n    \"baseUrl\": \".\",\n    \"paths\": { \"@/*\": [\"src/*\"], }\n  }\n}\n",
        )
        .unwrap();
        let r = load_alias_resolver(dir.path(), &[dir.path().join("tsconfig.json")]);
        assert_eq!(r.resolve("src/app.ts", "@/lib/api"), vec!["src/lib/api"]);
    }

    #[test]
    fn load_resolver_follows_one_extends_level() {
        let dir = tempfile::tempdir().unwrap();
        // Base at root holds the paths; child in packages/app only extends it.
        std::fs::write(
            dir.path().join("tsconfig.base.json"),
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@/*": ["src/*"] } } }"#,
        )
        .unwrap();
        let child_dir = dir.path().join("packages/app");
        std::fs::create_dir_all(&child_dir).unwrap();
        std::fs::write(
            child_dir.join("tsconfig.json"),
            r#"{ "extends": "../../tsconfig.base.json" }"#,
        )
        .unwrap();
        let r = load_alias_resolver(dir.path(), &[child_dir.join("tsconfig.json")]);
        // Base's paths resolve against the base dir (root), per tsc.
        assert_eq!(
            r.resolve("packages/app/src/Foo.tsx", "@/lib/api"),
            vec!["src/lib/api"]
        );
    }
}
