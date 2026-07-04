//! Per-file called-name sidecar (`synaptic-out/.callnames.json`): for every
//! extracted file, the bare symbol names its raw calls reference. An
//! incremental rebuild uses it to find which UNCHANGED files might resolve
//! against a name the current change (re)introduces -- their raw calls exist
//! only in their own extraction, so without this index a returning or newly
//! added definition never attracts their calls until they happen to change.
//!
//! Missing or stale sidecar degrades gracefully: no ripple candidates, i.e.
//! the pre-sidecar behavior. A full rebuild or `extract` re-seeds it whole.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use synaptic_core::RawCall;

/// file key (repo-relative, POSIX) -> bare names its raw calls reference.
pub type CallNames = BTreeMap<String, BTreeSet<String>>;

/// The bare called-name set of one file's raw calls -- a sidecar entry value.
/// One derivation shared by `extract` (seeding) and the incremental rebuild
/// (maintenance) so the two writers can never drift.
pub fn call_names<'a>(raw_calls: impl IntoIterator<Item = &'a RawCall>) -> BTreeSet<String> {
    raw_calls
        .into_iter()
        .map(|c| bare_name(&c.callee).to_string())
        .collect()
}

/// Group raw calls into a whole sidecar map, keyed by each call's source file
/// (normalized to the POSIX manifest-key form). Call-free files need no entry:
/// they can never be ripple candidates.
pub fn from_raw_calls<'a>(raw_calls: impl IntoIterator<Item = &'a RawCall>) -> CallNames {
    let mut map = CallNames::new();
    for rc in raw_calls {
        map.entry(rc.source_file.replace('\\', "/"))
            .or_default()
            .insert(bare_name(&rc.callee).to_string());
    }
    map
}

const FILE: &str = ".callnames.json";

/// Path of the sidecar under the output dir.
pub fn callnames_path(out_dir: &Path) -> PathBuf {
    out_dir.join(FILE)
}

/// Load the sidecar; missing or corrupt reads as empty (ripple disabled).
pub fn load_callnames(out_dir: &Path) -> CallNames {
    std::fs::read(callnames_path(out_dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Persist the sidecar (best-effort callers may ignore the error).
pub fn save_callnames(out_dir: &Path, names: &CallNames) -> std::io::Result<()> {
    std::fs::create_dir_all(out_dir)?;
    std::fs::write(callnames_path(out_dir), serde_json::to_vec(names)?)
}

/// The bare symbol name of a node label or callee: `announce()` -> `announce`,
/// `.update()` -> `update`. Raw-call callees are already bare; node labels
/// carry the display suffix/prefix.
pub fn bare_name(label: &str) -> &str {
    let l = label.strip_suffix("()").unwrap_or(label);
    l.strip_prefix('.').unwrap_or(l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_name_strips_display_decorations() {
        assert_eq!(bare_name("announce()"), "announce");
        assert_eq!(bare_name(".update()"), "update");
        assert_eq!(bare_name("Class"), "Class");
    }

    #[test]
    fn load_missing_or_corrupt_is_empty_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_callnames(dir.path()).is_empty(), "missing -> empty");
        std::fs::write(callnames_path(dir.path()), b"not json").unwrap();
        assert!(load_callnames(dir.path()).is_empty(), "corrupt -> empty");

        let mut names = CallNames::new();
        names
            .entry("main.py".into())
            .or_default()
            .insert("announce".into());
        save_callnames(dir.path(), &names).unwrap();
        assert_eq!(load_callnames(dir.path()), names);
    }
}
