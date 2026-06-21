use std::path::Path;

use synaptic_core::make_id;

/// Normalize an id for endpoint reconciliation: NFKC + `[^\w]+`→`_` + collapse +
/// strip + casefold; `core::make_id(&[s])` performs exactly this for a single
/// string.
pub fn normalize_id(id: &str) -> String {
    make_id(&[id])
}

/// Normalize a `source_file`: backslashes → forward slashes, and (when `root`
/// is given and the path is absolute) relativize to `root` as a posix path.
pub fn norm_source_file(p: &str, root: Option<&str>) -> String {
    let slashed = p.replace('\\', "/");
    if let Some(root) = root {
        let path = Path::new(&slashed);
        if path.is_absolute() {
            let root_slashed = root.replace('\\', "/");
            if let Ok(rel) = path.strip_prefix(&root_slashed) {
                return rel.to_string_lossy().replace('\\', "/");
            }
        }
    }
    slashed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_id_matches_make_id_semantics() {
        assert_eq!(
            normalize_id("Session_ValidateToken"),
            "session_validatetoken"
        );
        assert_eq!(normalize_id("a.b.c"), "a_b_c");
        assert_eq!(normalize_id("Auth_Login"), "auth_login");
    }

    #[test]
    fn norm_source_file_converts_backslashes() {
        assert_eq!(
            norm_source_file("src\\middleware\\auth.py", None),
            "src/middleware/auth.py"
        );
    }

    #[test]
    fn norm_source_file_leaves_relative_unchanged() {
        assert_eq!(norm_source_file("src/foo.py", Some("/proj")), "src/foo.py");
    }

    #[test]
    fn norm_source_file_relativizes_absolute_under_root() {
        // CARGO_MANIFEST_DIR is absolute on every platform (drive-qualified on
        // Windows), so the relativization branch is exercised cross-platform.
        let root = env!("CARGO_MANIFEST_DIR");
        let abs = format!("{root}/docs/overview.md");
        assert_eq!(norm_source_file(&abs, Some(root)), "docs/overview.md");
    }
}
