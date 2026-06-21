//! `.gitmodules` parsing — submodule paths are separate codebases a workspace
//! fallback can surface as project roots.
use std::path::{Path, PathBuf};

/// Parse `root/.gitmodules` and return each submodule's `path = …` directory,
/// resolved under `root`. Empty when the file is absent or has no entries.
pub fn submodule_paths(root: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(root.join(".gitmodules")) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            l.strip_prefix("path").and_then(|rest| {
                rest.trim_start()
                    .strip_prefix('=')
                    .map(|v| root.join(v.trim()))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_submodule_paths() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(".gitmodules"),
            "[submodule \"libs/auth\"]\n\tpath = libs/auth\n\turl = https://x/auth\n[submodule \"vendor/x\"]\n\tpath = vendor/x\n\turl = https://x/x\n",
        )
        .unwrap();
        let paths: Vec<String> = submodule_paths(d.path())
            .iter()
            .map(|p| {
                p.strip_prefix(d.path())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(paths, vec!["libs/auth", "vendor/x"]);
    }

    #[test]
    fn absent_gitmodules_is_empty() {
        let d = tempfile::tempdir().unwrap();
        assert!(submodule_paths(d.path()).is_empty());
    }
}
