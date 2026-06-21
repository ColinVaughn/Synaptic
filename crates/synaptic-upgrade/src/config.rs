//! The persisted opt-in config at `~/.synaptic/update.toml`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Seconds between background update checks (24h).
const CHECK_INTERVAL_SECS: f64 = 86_400.0;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct UpdateConfig {
    /// Whether the background update check runs.
    #[serde(default)]
    pub enabled: bool,
    /// Epoch seconds of the last completed background check (f64 for parity with
    /// the manifest `mtime` convention used elsewhere in the project).
    #[serde(default)]
    pub last_check: Option<f64>,
}

impl UpdateConfig {
    /// Load the config, returning the default (disabled) when the file is absent.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Persist the config, creating the parent directory if needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serializing update config")?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }

    /// Whether a background check is due as of `now` (epoch seconds).
    pub fn due(&self, now: f64) -> bool {
        match self.last_check {
            None => true,
            Some(t) => now - t >= CHECK_INTERVAL_SECS,
        }
    }
}

/// The default config path: `~/.synaptic/update.toml`
/// (`%USERPROFILE%\.synaptic\update.toml` on Windows). Falls back to
/// `.synaptic/update.toml` in the CWD when no home directory is set. Mirrors the
/// home resolution used by the global graph store.
pub fn config_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let base = match home {
        Some(h) => h.join(".synaptic"),
        None => PathBuf::from(".synaptic"),
    };
    base.join("update.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_disabled_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = UpdateConfig::load(&dir.path().join("update.toml")).unwrap();
        assert!(!cfg.enabled);
        assert!(cfg.last_check.is_none());
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.toml");
        let cfg = UpdateConfig {
            enabled: true,
            last_check: Some(1_700_000_000.0),
        };
        cfg.save(&path).unwrap();
        let back = UpdateConfig::load(&path).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn due_respects_24h_window() {
        let never = UpdateConfig {
            enabled: true,
            last_check: None,
        };
        assert!(never.due(1_000.0));
        let fresh = UpdateConfig {
            enabled: true,
            last_check: Some(1_000.0),
        };
        assert!(!fresh.due(1_000.0 + 3_600.0)); // 1h later: not due
        assert!(fresh.due(1_000.0 + 86_400.0)); // exactly 24h later: due
    }
}
