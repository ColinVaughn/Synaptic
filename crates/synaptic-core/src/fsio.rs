//! Small shared filesystem helpers.

use std::io;
use std::path::Path;

/// Write via a sibling temp file + rename, so a concurrent reader (a CLI
/// query, a second serve) never observes a truncated file. The temp name
/// carries the pid so two uncoordinated writers cannot clobber each other's
/// temp. One shared implementation: `graph.json` is written by the extract,
/// update, and serve paths, and they must all give readers the same guarantee.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp{}", std::process::id()));
    let tmp = path.with_file_name(name);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_replaces_content_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.json");
        write_atomic(&path, b"first").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        write_atomic(&path, b"second-longer-content").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second-longer-content");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "graph.json")
            .collect();
        assert!(leftovers.is_empty(), "no temp residue: {leftovers:?}");
    }
}
