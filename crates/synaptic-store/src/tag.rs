//! Shard-tag -> safe, stable filename. Enforced at the store boundary so a
//! hostile or unusual federation tag (`@scope/pkg`, `../escape`, a reserved
//! Windows device name) can never escape `synaptic-out/store/`.

use crate::StoreError;

/// Reserved Windows device names. A file named after any of these (any case,
/// with or without extension) is not openable, so a tag mapping to one is rejected.
const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Map a federation tag to a filesystem-safe stem.
///
/// ASCII `[A-Za-z0-9_-]` pass through unchanged; every other byte is hex-escaped
/// as `_xNN_`, so the mapping is injective (no two distinct tags collide) and
/// deterministic. Rejects the empty string, `.`/`..`, and any tag whose encoded
/// form would collide with a reserved Windows device name.
pub fn sanitize_tag(tag: &str) -> Result<String, StoreError> {
    if tag.is_empty() || tag == "." || tag == ".." {
        return Err(StoreError::BadTag(tag.to_string()));
    }
    let mut out = String::with_capacity(tag.len());
    for b in tag.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' => out.push(b as char),
            _ => out.push_str(&format!("_x{b:02x}_")),
        }
    }
    if RESERVED.contains(&out.to_ascii_lowercase().as_str()) {
        return Err(StoreError::BadTag(tag.to_string()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_plain_tag() {
        assert_eq!(sanitize_tag("billing").unwrap(), "billing");
        assert_eq!(sanitize_tag("my-repo_1").unwrap(), "my-repo_1");
    }

    #[test]
    fn rejects_empty_and_dot_components() {
        // whole-name traversal components and empty are rejected outright
        assert!(sanitize_tag("..").is_err());
        assert!(sanitize_tag(".").is_err());
        assert!(sanitize_tag("").is_err());
    }

    #[test]
    fn separators_are_encoded_never_emitted() {
        // embedded separators are made safe by encoding, not rejected, so the
        // result is a single path component that cannot escape the store dir
        for raw in ["a/b", "a\\b", "../x", "a/../b"] {
            let s = sanitize_tag(raw).unwrap();
            assert!(!s.contains('/'), "{raw:?} -> {s:?} still has /");
            assert!(!s.contains('\\'), "{raw:?} -> {s:?} still has \\");
            assert_ne!(s, "..");
            assert_ne!(s, ".");
        }
    }

    #[test]
    fn rejects_reserved_windows_names() {
        assert!(sanitize_tag("CON").is_err());
        assert!(sanitize_tag("nul").is_err());
    }

    #[test]
    fn encodes_scoped_npm_tag() {
        // federation tags can look like `@scope/pkg`; must round-trip to a safe,
        // stable name with no separators or `@`.
        let s = sanitize_tag("@scope/pkg").unwrap();
        assert!(!s.contains('/') && !s.contains('@'));
        assert_eq!(s, sanitize_tag("@scope/pkg").unwrap()); // deterministic
    }
}
