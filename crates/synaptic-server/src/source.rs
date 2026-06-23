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

/// Why a `source_file` could not be served from under the jail. Splitting these
/// out lets the caller tell "the file isn't there" from "it's outside the
/// trusted root", which matters in federated workspaces where a node's path can
/// point at a sibling repo outside a single `--source-root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// Resolved to a real file inside `root`.
    Found(PathBuf),
    /// `root` or `root/rel` does not exist on disk (nothing to read).
    Missing,
    /// The path resolved outside `root` (jail escape).
    OutsideRoot,
}

/// Resolve `rel` (a repo-relative `source_file`) under `root`, distinguishing a
/// missing file from a path that escapes the jail. Canonicalizing both sides
/// collapses `..` so traversal is caught by the `starts_with` check.
pub fn resolve_in_root_detailed(root: &Path, rel: &str) -> ResolveOutcome {
    let Ok(root) = root.canonicalize() else {
        return ResolveOutcome::Missing;
    };
    match root.join(rel).canonicalize() {
        Ok(canon) if canon.starts_with(&root) => ResolveOutcome::Found(canon),
        Ok(_) => ResolveOutcome::OutsideRoot,
        Err(_) => ResolveOutcome::Missing,
    }
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

        assert!(matches!(
            resolve_in_root_detailed(root, "src/a.py"),
            ResolveOutcome::Found(_)
        ));
        // Escape attempt: rejected, never Found. The exact reason is platform-
        // dependent -- a path that escapes the root resolves to OutsideRoot when
        // the target exists (e.g. /etc/passwd on Unix) and Missing when it does
        // not (e.g. on Windows); both refuse the read. The Missing-vs-OutsideRoot
        // distinction is pinned separately in detailed_outcome_separates_missing_from_escape.
        assert!(matches!(
            resolve_in_root_detailed(root, "../../etc/passwd"),
            ResolveOutcome::Missing | ResolveOutcome::OutsideRoot
        ));
        // Missing file -> Missing (not a panic).
        assert_eq!(
            resolve_in_root_detailed(root, "src/missing.py"),
            ResolveOutcome::Missing
        );
    }

    #[test]
    fn detailed_outcome_separates_missing_from_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.py"), "x = 1\n").unwrap();

        assert!(matches!(
            resolve_in_root_detailed(root, "src/a.py"),
            ResolveOutcome::Found(_)
        ));
        assert_eq!(
            resolve_in_root_detailed(root, "src/missing.py"),
            ResolveOutcome::Missing
        );
        // A path that exists but resolves outside the root is an escape, not a
        // missing file: create a real sibling so canonicalize succeeds.
        let sibling = dir.path().parent().unwrap().join("outside.py");
        std::fs::write(&sibling, "y = 2\n").unwrap();
        assert_eq!(
            resolve_in_root_detailed(root, "../outside.py"),
            ResolveOutcome::OutsideRoot
        );
        let _ = std::fs::remove_file(&sibling);
    }
}
