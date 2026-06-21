use std::collections::BTreeMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

/// One file's change-detection record. `mtime` is seconds since the Unix epoch;
/// `hash` is the blake3 hex of the file contents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub mtime: f64,
    pub hash: String,
}

/// Portable change-detection manifest keyed by forward-slash paths relative to
/// the scan root (so it round-trips across machines/checkouts). Hashes with
/// blake3 — internal bookkeeping, not a stable on-disk contract.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest(pub BTreeMap<String, FileEntry>);

/// Files that changed since a prior manifest.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ManifestDiff {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    pub unchanged: Vec<String>,
}

/// blake3 hex of a file's contents, or `None` on I/O error.
pub fn hash_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(blake3::hash(&bytes).to_hex().to_string())
}

fn relative_key(path: &Path, root: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

fn mtime_secs(path: &Path) -> f64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl Manifest {
    /// Build a manifest for `paths`, keyed relative to `root`. Files that fail
    /// to hash are skipped.
    pub fn build<'a>(paths: impl IntoIterator<Item = &'a Path>, root: &Path) -> Manifest {
        let mut map = BTreeMap::new();
        for p in paths {
            if let Some(hash) = hash_file(p) {
                map.insert(
                    relative_key(p, root),
                    FileEntry {
                        mtime: mtime_secs(p),
                        hash,
                    },
                );
            }
        }
        Manifest(map)
    }

    /// Like [`build`](Self::build) but reuses `prior`'s hash for any path whose
    /// mtime is unchanged — the stat-index fastpath that lets unchanged files
    /// skip the read + re-hash on a rebuild. A path missing from `prior`, whose
    /// mtime differs, or whose mtime can't be read (0.0) is hashed fresh.
    pub fn build_incremental<'a>(
        paths: impl IntoIterator<Item = &'a Path>,
        root: &Path,
        prior: &Manifest,
    ) -> Manifest {
        let mut map = BTreeMap::new();
        for p in paths {
            let key = relative_key(p, root);
            let mtime = mtime_secs(p);
            // Fastpath: a readable, unchanged mtime that matches a prior entry
            // means trust the prior hash and skip the read + hash entirely.
            if mtime != 0.0 {
                if let Some(prev) = prior.0.get(&key) {
                    if prev.mtime == mtime {
                        map.insert(key, prev.clone());
                        continue;
                    }
                }
            }
            if let Some(hash) = hash_file(p) {
                map.insert(key, FileEntry { mtime, hash });
            }
        }
        Manifest(map)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Manifest {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Compare `self` (prior) against a freshly built `current` manifest.
    pub fn diff(&self, current: &Manifest) -> ManifestDiff {
        let mut d = ManifestDiff::default();
        for (key, entry) in &current.0 {
            match self.0.get(key) {
                None => d.added.push(key.clone()),
                Some(prev) if prev.hash != entry.hash => d.changed.push(key.clone()),
                Some(_) => d.unchanged.push(key.clone()),
            }
        }
        for key in self.0.keys() {
            if !current.0.contains_key(key) {
                d.removed.push(key.clone());
            }
        }
        d.added.sort();
        d.changed.sort();
        d.removed.sort();
        d.unchanged.sort();
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn build_uses_relative_posix_keys() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/foo.py"), "x = 1\n").unwrap();
        fs::write(dir.path().join("doc.md"), "hi\n").unwrap();
        let paths = [dir.path().join("src/foo.py"), dir.path().join("doc.md")];
        let m = Manifest::build(paths.iter().map(|p| p.as_path()), dir.path());
        let keys: std::collections::BTreeSet<_> = m.0.keys().cloned().collect();
        assert_eq!(
            keys,
            ["doc.md".to_string(), "src/foo.py".to_string()]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        let m = Manifest::build([dir.path().join("a.py").as_path()], dir.path());
        let mpath = dir.path().join("synaptic-out/manifest.json");
        m.save(&mpath).unwrap();
        assert_eq!(Manifest::load(&mpath), m);
    }

    #[test]
    fn mtime_floats_round_trip_exactly() {
        // A high-precision f64 mtime must survive serialize/deserialize bit-for-bit
        // (requires serde_json's `float_roundtrip` feature; its default parser can
        // be off by ~1 ULP, which intermittently broke `save_load_round_trip`).
        // Built from bit patterns (exponent field 0x41D -> ~1.07e9 to 2.1e9, i.e.
        // realistic Unix seconds) with fully-populated 52-bit mantissas: the
        // regime where the fast parser is most likely to land 1 ULP off.
        let dir = tempfile::tempdir().unwrap();
        for mtime in [
            f64::from_bits(0x41D5_F3FF_1234_5678),
            f64::from_bits(0x41DA_BCDE_F987_6543),
            f64::from_bits(0x41D2_468A_CE13_5790),
        ] {
            let mut map = BTreeMap::new();
            map.insert(
                "a.py".to_string(),
                FileEntry {
                    mtime,
                    hash: "abc".to_string(),
                },
            );
            let m = Manifest(map);
            let p = dir.path().join("m.json");
            m.save(&p).unwrap();
            let back = Manifest::load(&p);
            assert_eq!(back.0["a.py"].mtime.to_bits(), mtime.to_bits());
            assert_eq!(back, m);
        }
    }

    #[test]
    fn diff_detects_added_changed_removed_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.py");
        let b = dir.path().join("b.py");
        fs::write(&a, "x = 1\n").unwrap();
        fs::write(&b, "y = 1\n").unwrap();
        let prior = Manifest::build([a.as_path(), b.as_path()], dir.path());

        // a unchanged, b changed, c added.
        fs::write(&b, "y = 2\n").unwrap();
        let c = dir.path().join("c.py");
        fs::write(&c, "z = 1\n").unwrap();
        let current = Manifest::build([a.as_path(), b.as_path(), c.as_path()], dir.path());

        let d = prior.diff(&current);
        assert_eq!(d.added, vec!["c.py".to_string()]);
        assert_eq!(d.changed, vec!["b.py".to_string()]);
        assert_eq!(d.unchanged, vec!["a.py".to_string()]);
        assert!(d.removed.is_empty());

        // Now drop c and rebuild: c is removed relative to prior? No, prior had
        // no c. Drop a instead to exercise removal.
        let current2 = Manifest::build([b.as_path(), c.as_path()], dir.path());
        let d2 = prior.diff(&current2);
        assert_eq!(d2.removed, vec!["a.py".to_string()]);
    }

    #[test]
    fn manifest_is_portable_across_roots() {
        // Two roots with identical content produce identical manifests, so a
        // diff across them reports nothing changed (the cross-machine case).
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for root in [a.path(), b.path()] {
            fs::create_dir_all(root.join("src")).unwrap();
            fs::write(root.join("src/foo.py"), "pass\n").unwrap();
            fs::write(root.join("doc.md"), "hello\n").unwrap();
        }
        let ma = Manifest::build(
            [a.path().join("src/foo.py"), a.path().join("doc.md")]
                .iter()
                .map(|p| p.as_path()),
            a.path(),
        );
        let mb = Manifest::build(
            [b.path().join("src/foo.py"), b.path().join("doc.md")]
                .iter()
                .map(|p| p.as_path()),
            b.path(),
        );
        // Portability invariant: identical keys + content hashes across roots
        // (mtime is machine/time-specific and intentionally NOT part of it;
        // the diff compares hashes, so a cross-root diff reports no changes).
        assert_eq!(
            ma.0.keys().collect::<Vec<_>>(),
            mb.0.keys().collect::<Vec<_>>()
        );
        for (key, entry_a) in &ma.0 {
            assert_eq!(entry_a.hash, mb.0[key].hash);
        }
        let d = ma.diff(&mb);
        assert!(d.added.is_empty() && d.changed.is_empty() && d.removed.is_empty());
        assert_eq!(d.unchanged.len(), 2);
    }

    #[test]
    fn build_incremental_skips_rehash_when_mtime_unchanged() {
        // A prior entry whose mtime equals the file's CURRENT mtime but carries a
        // sentinel hash: the fastpath must reuse the sentinel (proving it never
        // read/hashed the real content). A plain build() would return the real hash.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.py");
        fs::write(&a, "x = 1\n").unwrap();
        let real_mtime = super::mtime_secs(&a);
        let mut map = BTreeMap::new();
        map.insert(
            "a.py".to_string(),
            FileEntry {
                mtime: real_mtime,
                hash: "SENTINEL".to_string(),
            },
        );
        let prior = Manifest(map);
        let inc = Manifest::build_incremental([a.as_path()], dir.path(), &prior);
        assert_eq!(
            inc.0["a.py"].hash, "SENTINEL",
            "unchanged mtime ⇒ reuse prior hash (skip re-hash)"
        );
    }

    #[test]
    fn build_incremental_rehashes_when_mtime_differs() {
        // A prior entry with a stale (different) mtime must trigger a fresh hash.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.py");
        fs::write(&a, "x = 1\n").unwrap();
        let real_mtime = super::mtime_secs(&a);
        let mut map = BTreeMap::new();
        map.insert(
            "a.py".to_string(),
            FileEntry {
                mtime: real_mtime - 1000.0, // stale
                hash: "STALE".to_string(),
            },
        );
        let prior = Manifest(map);
        let inc = Manifest::build_incremental([a.as_path()], dir.path(), &prior);
        assert_ne!(inc.0["a.py"].hash, "STALE", "stale mtime ⇒ re-hash");
        assert_eq!(inc.0["a.py"].hash, hash_file(&a).unwrap());
    }

    #[test]
    fn build_incremental_hashes_new_files() {
        // A path absent from `prior` is hashed fresh.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.py");
        fs::write(&a, "x = 1\n").unwrap();
        let inc = Manifest::build_incremental([a.as_path()], dir.path(), &Manifest::default());
        assert_eq!(inc.0["a.py"].hash, hash_file(&a).unwrap());
    }
}
