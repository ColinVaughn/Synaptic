//! Download the matching release archive, verify its checksum, extract the
//! binaries, and atomically replace the running executable.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::github::{Asset, Release};
use crate::target::{archive_name, binary_name, sha256_name};

/// Find a release asset by exact file name.
pub fn find_asset<'a>(release: &'a Release, name: &str) -> Option<&'a Asset> {
    release.assets.iter().find(|a| a.name == name)
}

/// Verify `bytes` hashes to the hex digest at the start of `expected`
/// (sidecar files are often `"<hex>  <filename>"`). Case-insensitive.
pub fn verify_sha256(bytes: &[u8], expected: &str) -> bool {
    let want = match expected.split_whitespace().next() {
        Some(t) => t.to_ascii_lowercase(),
        None => return false,
    };
    let mut h = Sha256::new();
    h.update(bytes);
    let got = h.finalize();
    let got_hex: String = got.iter().map(|b| format!("{b:02x}")).collect();
    got_hex == want
}

/// Download `url` into memory (blocking).
fn download(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(concat!("synaptic-update/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client.get(url).send().context("downloading asset")?;
    let resp = resp.error_for_status().context("download failed")?;
    Ok(resp.bytes().context("reading download body")?.to_vec())
}

/// Extract the binary named `stem` (e.g. "synaptic") from a release archive into
/// `dest_dir`, returning the written path. Dispatches on the archive extension.
pub fn extract_binary(archive: &Path, stem: &str, dest_dir: &Path) -> Result<PathBuf> {
    let want = binary_name(stem);
    let name = archive.to_string_lossy();
    if name.ends_with(".zip") {
        extract_from_zip(archive, &want, dest_dir)
    } else {
        extract_from_tar_gz(archive, &want, dest_dir)
    }
}

fn extract_from_tar_gz(archive: &Path, want: &str, dest_dir: &Path) -> Result<PathBuf> {
    let f = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(f));
    for entry in ar.entries().context("reading tar entries")? {
        let mut e = entry.context("reading tar entry")?;
        let path = e.path().context("entry path")?.into_owned();
        if path.file_name().and_then(|s| s.to_str()) == Some(want) {
            let out = dest_dir.join(want);
            e.unpack(&out)
                .with_context(|| format!("unpacking {want}"))?;
            return Ok(out);
        }
    }
    bail!("{want} not found in {}", archive.display())
}

fn extract_from_zip(archive: &Path, want: &str, dest_dir: &Path) -> Result<PathBuf> {
    let f = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(f).context("opening zip")?;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).context("reading zip entry")?;
        let name = file.enclosed_name();
        let matches = name
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            == Some(want);
        if matches {
            let out = dest_dir.join(want);
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).context("reading zip member")?;
            std::fs::write(&out, &buf).with_context(|| format!("writing {want}"))?;
            return Ok(out);
        }
    }
    bail!("{want} not found in {}", archive.display())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Run the full update: download + verify + extract + replace the running exe
/// (and best-effort the sibling alias). `triple` is the current platform target.
pub fn apply_update(release: &Release, triple: &str) -> Result<()> {
    let archive_file = archive_name(triple);
    let asset = find_asset(release, &archive_file)
        .ok_or_else(|| anyhow!("release {} has no asset {archive_file}", release.version))?;

    println!("Downloading {archive_file} ...");
    let bytes = download(&asset.url)?;

    // Verify against the sha256 sidecar when present; warn (don't fail) if the
    // release predates checksum publishing.
    match find_asset(release, &sha256_name(triple)) {
        Some(sidecar) => {
            let sum = download(&sidecar.url)?;
            let sum = String::from_utf8_lossy(&sum);
            if !verify_sha256(&bytes, &sum) {
                bail!("checksum mismatch for {archive_file} - aborting");
            }
            println!("Checksum verified.");
        }
        None => {
            eprintln!("warning: no checksum published for this release; skipping verification")
        }
    }

    let tmp = tempfile::tempdir().context("creating temp dir")?;
    let archive_path = tmp.path().join(&archive_file);
    std::fs::write(&archive_path, &bytes).context("writing archive to temp")?;

    // Replace the running executable (whatever its name) with the matching new
    // binary, then best-effort replace the sibling alias.
    let current = std::env::current_exe().context("resolving current exe")?;
    let cur_stem = if current
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s == "syn")
        .unwrap_or(false)
    {
        "syn"
    } else {
        "synaptic"
    };
    let sibling_stem = if cur_stem == "syn" { "synaptic" } else { "syn" };

    let new_self = extract_binary(&archive_path, cur_stem, tmp.path())?;
    make_executable(&new_self)?;
    self_replace::self_replace(&new_self).context("replacing running executable")?;

    if let Ok(new_sibling) = extract_binary(&archive_path, sibling_stem, tmp.path()) {
        let _ = make_executable(&new_sibling);
        let sibling_path = current.with_file_name(binary_name(sibling_stem));
        if let Err(e) = std::fs::copy(&new_sibling, &sibling_path) {
            eprintln!(
                "warning: updated {cur_stem} but could not update the {sibling_stem} alias at {}: {e}",
                sibling_path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{Asset, Release};

    #[test]
    fn verify_sha256_matches_and_rejects() {
        let bytes = b"hello world";
        // sha256("hello world")
        let good = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert!(verify_sha256(bytes, good));
        // sidecar files often look like "<hex>  filename"
        assert!(verify_sha256(bytes, &format!("{good}  synaptic.tar.gz")));
        assert!(!verify_sha256(bytes, "deadbeef"));
    }

    #[test]
    fn find_asset_by_name() {
        let r = Release {
            version: "v1".into(),
            notes: String::new(),
            assets: vec![
                Asset {
                    name: "a.zip".into(),
                    url: "u1".into(),
                },
                Asset {
                    name: "b.tar.gz".into(),
                    url: "u2".into(),
                },
            ],
        };
        assert_eq!(find_asset(&r, "b.tar.gz").unwrap().url, "u2");
        assert!(find_asset(&r, "missing").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn extracts_named_binary_from_tar_gz() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar.gz");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let enc = GzEncoder::new(f, Compression::default());
            let mut tar = tar::Builder::new(enc);
            let mut header = tar::Header::new_gnu();
            let data = b"#!/bin/true\n";
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, "synaptic-x/synaptic", &data[..])
                .unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let out = extract_binary(&archive, "synaptic", dir.path()).unwrap();
        assert!(out.exists());
        assert_eq!(std::fs::read(&out).unwrap(), b"#!/bin/true\n");
    }

    #[cfg(windows)]
    #[test]
    fn extracts_named_binary_from_zip() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.zip");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            zip.start_file("synaptic-x/synaptic.exe", opts).unwrap();
            zip.write_all(b"MZ binary").unwrap();
            zip.finish().unwrap();
        }
        let out = extract_binary(&archive, "synaptic", dir.path()).unwrap();
        assert!(out.exists());
        assert_eq!(std::fs::read(&out).unwrap(), b"MZ binary");
    }
}
