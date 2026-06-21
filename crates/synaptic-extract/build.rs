//! Computes a fingerprint of the extractor's own source + enabled `lang-*` features
//! and exposes it as `SYNAPTIC_EXTRACT_BUILD_ID`. `cache.rs` folds this into the
//! on-disk AST cache namespace so a change to extraction *behavior* (a walker fix that
//! emits different nodes for the same bytes) invalidates stale cached results — which
//! `CARGO_PKG_VERSION` alone could not do within a dev cycle. Cargo re-runs this script
//! only when `src/` or `Cargo.toml` change, so identical rebuilds keep the cache warm.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

fn main() {
    // Re-run when extractor source or the manifest (features/deps) change.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");

    let mut hasher = DefaultHasher::new();

    // (1) Every .rs file under src/, hashed in a stable order (relative path + bytes).
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let src = Path::new(&manifest_dir).join("src");
    let mut files: Vec<PathBuf> = Vec::new();
    collect_rs(&src, &mut files);
    files.sort();
    for f in &files {
        if let Ok(rel) = f.strip_prefix(&src) {
            rel.to_string_lossy().hash(&mut hasher);
        }
        if let Ok(bytes) = std::fs::read(f) {
            bytes.hash(&mut hasher);
        }
    }

    // (2) The enabled feature set (which `lang-*` are on changes extractor output).
    let mut features: Vec<String> = std::env::vars()
        .filter_map(|(k, _)| k.strip_prefix("CARGO_FEATURE_").map(|s| s.to_string()))
        .collect();
    features.sort();
    for feat in &features {
        feat.hash(&mut hasher);
    }

    println!(
        "cargo:rustc-env=SYNAPTIC_EXTRACT_BUILD_ID={:016x}",
        hasher.finish()
    );
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}
