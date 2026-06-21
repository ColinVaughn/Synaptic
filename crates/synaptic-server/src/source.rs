//! Source-file access for the code-retrieval tools: parse a node's
//! `source_location` line marker and resolve a repo-relative `source_file`
//! against a trusted root, refusing anything that escapes it.

use std::path::{Path, PathBuf};

/// Parse a `source_location` marker into a 1-based start line. The extractor
/// writes `"L<n>"`; tolerate a range (`"L42-L60"` -> 42) and a bare number.
pub fn parse_line_marker(s: &str) -> Option<usize> {
    let head = s.split('-').next().unwrap_or(s).trim();
    let digits = head.trim_start_matches(|c: char| !c.is_ascii_digit());
    digits.parse().ok()
}

/// Resolve `rel` (a repo-relative `source_file`) under `root`, returning the
/// canonical path only if it stays inside `root`. `None` means: no readable
/// file, or the path escaped the jail. Canonicalizing both sides collapses
/// `..` so traversal is caught by the `starts_with` check.
pub fn resolve_in_root(root: &Path, rel: &str) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let canon = root.join(rel).canonicalize().ok()?;
    canon.starts_with(&root).then_some(canon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_marker_variants() {
        assert_eq!(parse_line_marker("L42"), Some(42));
        assert_eq!(parse_line_marker("L42-L60"), Some(42));
        assert_eq!(parse_line_marker("7"), Some(7));
        assert_eq!(parse_line_marker("Lxyz"), None);
    }

    #[test]
    fn jail_allows_inside_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.py"), "x = 1\n").unwrap();

        assert!(resolve_in_root(root, "src/a.py").is_some());
        // Escape attempt: canonicalizes outside root -> rejected.
        assert!(resolve_in_root(root, "../../etc/passwd").is_none());
        // Missing file -> None (not a panic).
        assert!(resolve_in_root(root, "src/missing.py").is_none());
    }
}
