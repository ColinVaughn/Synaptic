//! The opt-in, throttled background update check.

use std::path::Path;

use crate::config::{config_path, UpdateConfig};
use crate::github::{latest_release, Release};
use crate::version::version_is_newer;

/// Current epoch seconds (lives here so deterministic code stays clock-free).
fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Background check called once per CLI invocation. Returns a one-line notice to
/// print to stderr when an update is available, or `None`. Never blocks for long
/// and never errors: any network/parse failure is swallowed (returns `None`).
///
/// Honors a hard override: `SYNAPTIC_UPDATE_CHECK=0` disables the check even when
/// the config is enabled (e.g. for CI).
pub fn maybe_notify(current_version: &str) -> Option<String> {
    if std::env::var("SYNAPTIC_UPDATE_CHECK").as_deref() == Ok("0") {
        return None;
    }
    maybe_notify_with(&config_path(), current_version, now_secs(), || {
        latest_release().ok()
    })
}

/// Testable core: explicit config path, clock, and release fetcher.
pub fn maybe_notify_with(
    path: &Path,
    current_version: &str,
    now: f64,
    fetch: impl FnOnce() -> Option<Release>,
) -> Option<String> {
    let mut cfg = UpdateConfig::load(path).unwrap_or_default();
    if !cfg.enabled || !cfg.due(now) {
        return None;
    }
    // Stamp before fetching so a flaky network does not retry every invocation.
    cfg.last_check = Some(now);
    let _ = cfg.save(path);

    let latest = fetch()?;
    if version_is_newer(current_version, &latest.version) {
        Some(format!(
            "(note) Synaptic {} is available - run `synaptic self-update`",
            latest.version.trim_start_matches('v')
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UpdateConfig;
    use crate::github::Release;

    fn rel(v: &str) -> Release {
        Release {
            version: v.into(),
            notes: String::new(),
            assets: vec![],
        }
    }

    #[test]
    fn disabled_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.toml");
        UpdateConfig {
            enabled: false,
            last_check: None,
        }
        .save(&path)
        .unwrap();
        let out = maybe_notify_with(&path, "0.3.0", 1000.0, || Some(rel("0.9.9")));
        assert!(out.is_none());
    }

    #[test]
    fn throttled_returns_none_and_does_not_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.toml");
        UpdateConfig {
            enabled: true,
            last_check: Some(1000.0),
        }
        .save(&path)
        .unwrap();
        let mut fetched = false;
        let out = maybe_notify_with(&path, "0.3.0", 1000.0 + 60.0, || {
            fetched = true;
            Some(rel("9.9.9"))
        });
        assert!(out.is_none());
        assert!(!fetched, "should not fetch within the throttle window");
    }

    #[test]
    fn enabled_and_due_with_newer_returns_notice_and_stamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.toml");
        UpdateConfig {
            enabled: true,
            last_check: None,
        }
        .save(&path)
        .unwrap();
        let out = maybe_notify_with(&path, "0.3.0", 5000.0, || Some(rel("v0.3.1")));
        assert!(out.unwrap().contains("0.3.1"));
        // last_check advanced so the next call is throttled.
        let cfg = UpdateConfig::load(&path).unwrap();
        assert_eq!(cfg.last_check, Some(5000.0));
    }

    #[test]
    fn up_to_date_returns_none_but_still_stamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.toml");
        UpdateConfig {
            enabled: true,
            last_check: None,
        }
        .save(&path)
        .unwrap();
        let out = maybe_notify_with(&path, "0.3.1", 5000.0, || Some(rel("0.3.1")));
        assert!(out.is_none());
        assert_eq!(UpdateConfig::load(&path).unwrap().last_check, Some(5000.0));
    }
}
