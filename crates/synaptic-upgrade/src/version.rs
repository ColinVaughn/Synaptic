//! Semver comparison tolerant of a leading `v` (release tags are `vX.Y.Z`).

use semver::Version;

fn parse(s: &str) -> Option<Version> {
    Version::parse(s.trim().trim_start_matches('v')).ok()
}

/// Whether `latest` is a strictly newer semantic version than `current`.
/// Returns `false` if either string fails to parse (never offers a bad update).
pub fn version_is_newer(current: &str, latest: &str) -> bool {
    match (parse(current), parse(latest)) {
        (Some(c), Some(l)) => l > c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_patch_is_newer() {
        assert!(version_is_newer("0.3.0", "0.3.1"));
        assert!(version_is_newer("0.3.0", "v0.3.1")); // leading v tolerated
    }

    #[test]
    fn same_or_older_is_not_newer() {
        assert!(!version_is_newer("0.3.1", "0.3.1"));
        assert!(!version_is_newer("0.3.2", "0.3.1"));
    }

    #[test]
    fn unparseable_is_not_newer() {
        assert!(!version_is_newer("0.3.0", "not-a-version"));
    }
}
