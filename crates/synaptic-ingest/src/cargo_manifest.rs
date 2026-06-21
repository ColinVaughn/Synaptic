//! Cargo workspace introspection.
//!
//! Emits a `crate:<name>` node per workspace member and a `crate_depends_on`
//! edge for each **workspace-internal** dependency (external registry deps are
//! dropped — they aren't nodes in the graph).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use toml::Value as Toml;

use crate::{make_edge, make_node, Ingested};

fn load_toml(path: &Path) -> Option<Toml> {
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

fn rel_posix(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Member `Cargo.toml` paths: the root package (if any) + each `workspace.members`
/// glob expansion.
fn member_manifest_paths(root: &Path, root_data: &Toml) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if root_data.get("package").and_then(Toml::as_table).is_some() {
        paths.push(root.join("Cargo.toml"));
    }
    let Some(members) = root_data
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(Toml::as_array)
    else {
        return paths;
    };
    for m in members {
        let Some(pat) = m.as_str() else { continue };
        let full = root.join(pat);
        let Ok(matches) = glob::glob(&full.to_string_lossy()) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = matches.filter_map(Result::ok).collect();
        dirs.sort();
        for dir in dirs {
            let manifest = dir.join("Cargo.toml");
            if manifest.is_file() && !paths.contains(&manifest) {
                paths.push(manifest);
            }
        }
    }
    paths
}

/// Introspect a Cargo workspace at `root` into crate nodes + internal-dependency
/// edges. Empty result if `root/Cargo.toml` is missing/unparseable.
pub fn introspect_cargo(root: &Path) -> Ingested {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let root_manifest = root.join("Cargo.toml");
    let Some(root_data) = load_toml(&root_manifest) else {
        return Ingested::default();
    };

    // crate name -> (node id, manifest path, parsed manifest), sorted by name.
    let mut crates: BTreeMap<String, (String, PathBuf, Toml)> = BTreeMap::new();
    for manifest in member_manifest_paths(&root, &root_data) {
        let data = if manifest == root_manifest {
            root_data.clone()
        } else {
            match load_toml(&manifest) {
                Some(d) => d,
                None => continue,
            }
        };
        if let Some(name) = data
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(Toml::as_str)
        {
            crates.insert(name.to_string(), (format!("crate:{name}"), manifest, data));
        }
    }

    let mut out = Ingested::default();
    for (name, (id, manifest, _)) in &crates {
        out.nodes.push(make_node(
            id.clone(),
            name,
            &rel_posix(&root, manifest),
            1,
            None,
        ));
    }
    for (src_id, manifest, data) in crates.values() {
        let sf = rel_posix(&root, manifest);
        let Some(deps) = data.get("dependencies").and_then(Toml::as_table) else {
            continue;
        };
        let mut dep_names: Vec<&String> = deps.keys().collect();
        dep_names.sort();
        for dep in dep_names {
            if let Some((tgt_id, _, _)) = crates.get(dep) {
                out.edges.push(make_edge(
                    src_id.clone(),
                    tgt_id.clone(),
                    "crate_depends_on",
                    &sf,
                    1,
                    Some("cargo_dependency"),
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_internal_deps_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("crates/a")).unwrap();
        std::fs::create_dir_all(root.join("crates/b")).unwrap();
        // a depends on b (internal) + serde (external).
        std::fs::write(
            root.join("crates/a/Cargo.toml"),
            "[package]\nname = \"a\"\n[dependencies]\nb = { path = \"../b\" }\nserde = \"1\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("crates/b/Cargo.toml"),
            "[package]\nname = \"b\"\n",
        )
        .unwrap();

        let g = introspect_cargo(root);
        let ids: Vec<&str> = g.nodes.iter().map(|n| n.id.0.as_str()).collect();
        assert!(
            ids.contains(&"crate:a") && ids.contains(&"crate:b"),
            "{ids:?}"
        );
        assert_eq!(g.edges.len(), 1, "only the internal a->b dep");
        assert_eq!(g.edges[0].source.0, "crate:a");
        assert_eq!(g.edges[0].target.0, "crate:b");
        assert_eq!(g.edges[0].relation, "crate_depends_on");
    }

    #[test]
    fn missing_manifest_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(introspect_cargo(dir.path()), Ingested::default());
    }
}
