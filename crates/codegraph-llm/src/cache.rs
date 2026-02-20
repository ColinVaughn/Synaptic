//! Semantic-pass cache: stores a parsed extraction fragment per source file,
//! keyed by `(content, relative_path)` so an unchanged doc skips the LLM call on
//! a rebuild. The key uses blake3 (an internal detail, not a contract); the
//! `.md`-frontmatter-only refinement is deferred.

use std::path::PathBuf;

use serde_json::Value;

/// On-disk semantic cache rooted at a `cache/semantic` directory.
pub struct SemanticCache {
    dir: PathBuf,
}

impl SemanticCache {
    /// `dir` is the cache directory (e.g. `codegraph-out/cache/semantic`).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        SemanticCache { dir: dir.into() }
    }

    fn key(rel: &str, content: &str) -> String {
        let mut h = blake3::Hasher::new();
        h.update(content.as_bytes());
        h.update(&[0]);
        // Normalize separators to '/' before lowercasing so a Windows backslash
        // spelling and a posix spelling of the same path share a cache key.
        h.update(rel.replace('\\', "/").to_lowercase().as_bytes());
        h.finalize().to_hex().to_string()
    }

    fn entry(&self, rel: &str, content: &str) -> PathBuf {
        self.dir.join(format!("{}.json", Self::key(rel, content)))
    }

    /// Cached fragment for `(rel, content)`, or `None` on miss / unreadable entry.
    pub fn get(&self, rel: &str, content: &str) -> Option<Value> {
        let bytes = std::fs::read(self.entry(rel, content)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Store `value` for `(rel, content)`. Best-effort: parent dirs are created;
    /// errors propagate so the caller can decide whether to care.
    pub fn put(&self, rel: &str, content: &str, value: &Value) -> std::io::Result<()> {
        let path = self.entry(rel, content);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec(value)?)
    }

    /// Path the entry for `(rel, content)` would live at (mainly for tests).
    pub fn entry_path(&self, rel: &str, content: &str) -> PathBuf {
        self.entry(rel, content)
    }
}

/// True when two `(rel, content)` pairs map to distinct cache keys.
pub fn keys_differ(a: (&str, &str), b: (&str, &str)) -> bool {
    SemanticCache::key(a.0, a.1) != SemanticCache::key(b.0, b.1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn put_then_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SemanticCache::new(dir.path());
        let frag = json!({"nodes": [{"id": "concept_auth"}], "edges": []});
        assert!(cache.get("doc.md", "hello world").is_none(), "cold miss");
        cache.put("doc.md", "hello world", &frag).unwrap();
        assert_eq!(cache.get("doc.md", "hello world"), Some(frag));
    }

    #[test]
    fn distinct_path_or_content_is_a_distinct_entry() {
        assert!(keys_differ(("a.md", "x"), ("b.md", "x")));
        assert!(keys_differ(("a.md", "x"), ("a.md", "y")));
    }

    #[test]
    fn path_separator_spelling_does_not_change_key() {
        // Backslash and posix spellings of the same rel path share a key.
        assert!(!keys_differ(("sub\\doc.md", "x"), ("sub/doc.md", "x")));
    }

    #[test]
    fn changed_content_misses() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SemanticCache::new(dir.path());
        cache.put("doc.md", "v1", &json!({"nodes": []})).unwrap();
        assert!(cache.get("doc.md", "v2").is_none());
    }

    #[test]
    fn corrupt_entry_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SemanticCache::new(dir.path());
        let p = cache.entry_path("doc.md", "x");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"{ not json").unwrap();
        assert!(cache.get("doc.md", "x").is_none());
    }
}
