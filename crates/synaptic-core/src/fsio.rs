//! Small shared filesystem helpers.

use std::io::{self, Write};
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

/// Atomic write driven by a streaming callback: the closure writes into a
/// buffered temp file which is then renamed over `path`. Same reader guarantee
/// as [`write_atomic`], but the payload never exists in memory as one contiguous
/// buffer — required for the multi-hundred-MB artifacts a large corpus produces
/// (a 379k-node graph writes an 836 MB `graph.json`).
pub fn write_atomic_with<F>(path: &Path, write: F) -> io::Result<()>
where
    F: FnOnce(&mut dyn io::Write) -> io::Result<()>,
{
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp{}", std::process::id()));
    let tmp = path.with_file_name(name);

    let staged = (|| {
        let file = std::fs::File::create(&tmp)?;
        let mut buf = io::BufWriter::new(file);
        write(&mut buf)?;
        buf.flush()?;
        buf.into_inner()
            .map_err(|e| io::Error::other(e.to_string()))?
            .sync_all()
    })();
    if let Err(e) = staged {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

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

    #[test]
    fn write_atomic_with_streams_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("streamed.json");
        write_atomic_with(&path, |w| {
            w.write_all(b"{\"a\":")?;
            w.write_all(b"1}")
        })
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"a\":1}");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "streamed.json")
            .collect();
        assert!(leftovers.is_empty(), "no temp residue: {leftovers:?}");
    }

    #[test]
    fn write_atomic_with_removes_temp_on_writer_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("failed.json");
        let err = write_atomic_with(&path, |_w| Err(io::Error::other("boom")));
        assert!(err.is_err(), "writer error propagates");
        assert!(!path.exists(), "target not created on failure");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "temp cleaned up: {leftovers:?}");
    }

    #[test]
    fn write_atomic_with_replaces_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.json");
        write_atomic_with(&path, |w| w.write_all(b"second-longer-content")).unwrap();
        write_atomic_with(&path, |w| w.write_all(b"short")).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"short");
    }
}
