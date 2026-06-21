//! Map the running platform to the release asset built by
//! `.github/workflows/release.yml`.

/// The release archive file name for a target triple.
/// Windows targets ship a `.zip`; everything else ships a `.tar.gz`.
pub fn archive_name(triple: &str) -> String {
    if triple.contains("windows") {
        format!("synaptic-{triple}.zip")
    } else {
        format!("synaptic-{triple}.tar.gz")
    }
}

/// The checksum sidecar name for a target triple.
pub fn sha256_name(triple: &str) -> String {
    format!("{}.sha256", archive_name(triple))
}

/// The bare binary file name inside the archive (`synaptic`/`synaptic.exe`).
pub fn binary_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// The target triple for the running platform, or `None` when no prebuilt
/// release asset is published for it. The four arms match the release matrix.
pub fn current_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_triple_uses_tar_gz() {
        assert_eq!(
            archive_name("x86_64-unknown-linux-gnu"),
            "synaptic-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            sha256_name("x86_64-unknown-linux-gnu"),
            "synaptic-x86_64-unknown-linux-gnu.tar.gz.sha256"
        );
    }

    #[test]
    fn windows_triple_uses_zip() {
        assert_eq!(
            archive_name("x86_64-pc-windows-msvc"),
            "synaptic-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn current_target_is_some_on_supported_hosts() {
        // The CI matrix only runs on the four supported targets.
        assert!(current_target().is_some());
    }
}
