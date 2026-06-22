//! Install registry (`~/.synaptic/skills.toml`) + version-aware refresh.
//!
//! `synaptic install` records each `(repo, host)` it writes, with the version
//! that produced it and a content hash of each artifact (the per-platform
//! `SKILL.md` and the always-on marker block). A later `synaptic self-update`
//! (or `synaptic install --refresh`) calls [`refresh_all`], which re-renders each
//! recorded skill with the new binary and:
//!
//! - rewrites artifacts that are byte-identical to what we last wrote (so a
//!   version/content bump lands automatically);
//! - leaves hand-edited artifacts untouched and reports them (the on-disk content
//!   no longer matches the recorded hash);
//! - drops entries whose files are all gone (uninstalled / repo deleted).
//!
//! Only the markdown skill artifacts are refreshed. Codex MCP config / hooks and
//! Claude `settings.json` hooks are left to an explicit `synaptic install`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    extract_block, replace_or_append_section, skill_version, stamped_always_on, stamped_skill,
    Platform,
};

/// The registry file: `~/.synaptic/skills.toml` (`%USERPROFILE%\.synaptic\…` on
/// Windows), falling back to `.synaptic/skills.toml` in the CWD when no home
/// directory is set. Mirrors the resolution used by `update.toml`.
pub fn registry_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let base = match home {
        Some(h) => h.join(".synaptic"),
        None => PathBuf::from(".synaptic"),
    };
    base.join("skills.toml")
}

/// A short content hash (first 8 bytes of SHA-256, hex) — enough to detect a
/// hand-edit; not a security boundary.
fn content_hash(s: &str) -> String {
    let digest = Sha256::digest(s.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Canonical, stable string key for a repo root (falls back to the lossy path).
fn repo_key(repo_root: &Path) -> String {
    repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// One recorded install: a `(repo, host)` and the hashes of what we wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Absolute repo root the skill was installed into.
    pub repo: String,
    /// Platform key (`claude`, `agents`, ...).
    pub host: String,
    /// Synaptic version that produced the currently-installed artifacts.
    pub version: String,
    /// Hash of the `SKILL.md` we wrote (hosts with a dedicated skill file only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_hash: Option<String>,
    /// Hash of the always-on marker block we wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_hash: Option<String>,
}

/// The persisted registry: a list of [`Entry`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default, rename = "skill")]
    pub entries: Vec<Entry>,
}

impl Registry {
    /// Load the registry, returning an empty one when the file is absent or
    /// unparseable (a corrupt registry must never block install/update).
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Persist the registry, creating the parent directory if needed.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }

    fn upsert(&mut self, entry: Entry) {
        match self
            .entries
            .iter_mut()
            .find(|e| e.repo == entry.repo && e.host == entry.host)
        {
            Some(slot) => *slot = entry,
            None => self.entries.push(entry),
        }
    }

    fn remove(&mut self, repo: &str, host: &str) {
        self.entries.retain(|e| !(e.repo == repo && e.host == host));
    }
}

/// Record (or update) the registry entry for a freshly-installed `(repo, host)`.
/// Best-effort: a registry write failure never fails the install.
pub fn record_install(registry: &Path, platform: Platform, repo_root: &Path) {
    let entry = Entry {
        repo: repo_key(repo_root),
        host: platform.key().to_string(),
        version: skill_version().to_string(),
        skill_hash: platform
            .skill_dest()
            .map(|_| content_hash(&stamped_skill(platform))),
        block_hash: Some(content_hash(&stamped_always_on())),
    };
    let mut reg = Registry::load(registry);
    reg.upsert(entry);
    let _ = reg.save(registry);
}

/// Drop the registry entry for an uninstalled `(repo, host)`. Best-effort.
pub fn record_uninstall(registry: &Path, platform: Platform, repo_root: &Path) {
    let mut reg = Registry::load(registry);
    reg.remove(&repo_key(repo_root), platform.key());
    let _ = reg.save(registry);
}

/// What [`refresh_all`] did, for the CLI summary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshSummary {
    /// `(repo, host)` whose artifacts were rewritten to the current version.
    pub refreshed: Vec<(String, String)>,
    /// `(repo, host, file)` left untouched because the file was hand-edited.
    pub skipped_edited: Vec<(String, String, String)>,
    /// Entries already current (no rewrite needed).
    pub up_to_date: usize,
    /// `(repo, host)` dropped because their files are gone.
    pub dropped: Vec<(String, String)>,
}

impl RefreshSummary {
    /// A one-line human summary, or `None` when nothing was registered.
    pub fn line(&self) -> Option<String> {
        if self.refreshed.is_empty()
            && self.skipped_edited.is_empty()
            && self.dropped.is_empty()
            && self.up_to_date == 0
        {
            return None;
        }
        Some(format!(
            "skills: {} refreshed, {} up-to-date, {} modified (left as-is), {} removed",
            self.refreshed.len(),
            self.up_to_date,
            self.skipped_edited.len(),
            self.dropped.len(),
        ))
    }
}

/// Per-entry reconciliation outcome.
enum EntryStatus {
    /// No artifacts found on disk → drop the entry.
    Gone,
    /// Artifacts present; `refreshed` if any were rewritten, plus any files left
    /// untouched because they were hand-edited.
    Present {
        refreshed: bool,
        edited: Vec<String>,
    },
}

/// Re-render the skill for one entry and rewrite the artifacts that are unchanged
/// since install, updating the entry's hashes/version in place.
fn refresh_entry(entry: &mut Entry) -> EntryStatus {
    let repo_root = PathBuf::from(&entry.repo);
    let Some(platform) = Platform::parse(&entry.host) else {
        return EntryStatus::Gone;
    };
    if !repo_root.exists() {
        return EntryStatus::Gone;
    }

    let mut present = false;
    let mut refreshed = false;
    let mut edited = Vec::new();

    // Dedicated SKILL.md (hosts that have one).
    if let Some(dest) = platform.skill_dest() {
        let path = repo_root.join(dest);
        if let Ok(current) = std::fs::read_to_string(&path) {
            present = true;
            let rendered = stamped_skill(platform);
            if current != rendered {
                if entry.skill_hash.as_deref() == Some(content_hash(&current).as_str()) {
                    if std::fs::write(&path, &rendered).is_ok() {
                        entry.skill_hash = Some(content_hash(&rendered));
                        refreshed = true;
                    }
                } else {
                    edited.push(dest.to_string());
                }
            }
        }
    }

    // Always-on marker block (every host).
    let ao_path = repo_root.join(platform.always_on_file());
    if let Ok(file) = std::fs::read_to_string(&ao_path) {
        if let Some(current_block) = extract_block(&file) {
            present = true;
            let rendered_block = stamped_always_on();
            if current_block != rendered_block {
                if entry.block_hash.as_deref() == Some(content_hash(&current_block).as_str()) {
                    let updated = replace_or_append_section(&file, &rendered_block);
                    if std::fs::write(&ao_path, updated).is_ok() {
                        entry.block_hash = Some(content_hash(&rendered_block));
                        refreshed = true;
                    }
                } else {
                    edited.push(platform.always_on_file().to_string());
                }
            }
        }
    }

    if !present {
        return EntryStatus::Gone;
    }
    if refreshed {
        entry.version = skill_version().to_string();
    }
    EntryStatus::Present { refreshed, edited }
}

/// Re-render every registered skill with the current binary, refreshing the ones
/// unchanged since install and leaving hand-edited ones alone. Persists the
/// updated registry (dropping vanished entries) and returns a [`RefreshSummary`].
pub fn refresh_all(registry: &Path) -> RefreshSummary {
    let mut reg = Registry::load(registry);
    let mut summary = RefreshSummary::default();
    let mut kept: Vec<Entry> = Vec::new();

    for mut entry in std::mem::take(&mut reg.entries) {
        match refresh_entry(&mut entry) {
            EntryStatus::Gone => summary
                .dropped
                .push((entry.repo.clone(), entry.host.clone())),
            EntryStatus::Present { refreshed, edited } => {
                if refreshed {
                    summary
                        .refreshed
                        .push((entry.repo.clone(), entry.host.clone()));
                } else if edited.is_empty() {
                    summary.up_to_date += 1;
                }
                for file in edited {
                    summary
                        .skipped_edited
                        .push((entry.repo.clone(), entry.host.clone(), file));
                }
                kept.push(entry);
            }
        }
    }

    reg.entries = kept;
    let _ = reg.save(registry);
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_to(repo: &Path, platform: Platform) {
        crate::install(platform, repo).unwrap();
    }

    #[test]
    fn registry_round_trips_and_upserts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skills.toml");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        install_to(&repo, Platform::Claude);
        record_install(&path, Platform::Claude, &repo);

        let reg = Registry::load(&path);
        assert_eq!(reg.entries.len(), 1);
        assert_eq!(reg.entries[0].host, "claude");
        assert!(reg.entries[0].skill_hash.is_some());
        assert!(reg.entries[0].block_hash.is_some());

        // Re-recording the same (repo, host) updates in place, not duplicates.
        record_install(&path, Platform::Claude, &repo);
        assert_eq!(Registry::load(&path).entries.len(), 1);

        record_uninstall(&path, Platform::Claude, &repo);
        assert!(Registry::load(&path).entries.is_empty());
    }

    #[test]
    fn refresh_rewrites_a_stale_unedited_skill() {
        // Install, then simulate an "old" install by corrupting the recorded
        // hashes and writing stale on-disk content the registry still "owns".
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skills.toml");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        install_to(&repo, Platform::Agents);

        // Stale block on disk + a registry entry recording that exact stale text.
        let ao = repo.join("AGENTS.md");
        let stale_block = "<!-- synaptic:start -->\n## Synaptic\nOLD\n<!-- synaptic:end -->";
        std::fs::write(&ao, format!("# Project\n\n{stale_block}\n")).unwrap();
        let mut reg = Registry::default();
        reg.entries.push(Entry {
            repo: repo_key(&repo),
            host: "agents".into(),
            version: "0.0.1".into(),
            skill_hash: None,
            block_hash: Some(content_hash(stale_block)),
        });
        reg.save(&path).unwrap();

        let summary = refresh_all(&path);
        assert_eq!(summary.refreshed.len(), 1, "{summary:?}");
        assert!(summary.skipped_edited.is_empty());
        // The block was rewritten to the current render, foreign prose preserved.
        let after = std::fs::read_to_string(&ao).unwrap();
        assert!(after.contains("# Project"), "foreign content kept: {after}");
        assert!(after.contains(&stamped_always_on()) || after.contains("synaptic-skill v"));
        // Registry hash updated to the new block.
        let reg2 = Registry::load(&path);
        assert_eq!(
            reg2.entries[0].block_hash,
            Some(content_hash(&stamped_always_on()))
        );
    }

    #[test]
    fn refresh_skips_a_hand_edited_skill() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skills.toml");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        install_to(&repo, Platform::Agents);

        // The user edited the block; the registry records a DIFFERENT (old) hash,
        // and the on-disk block matches neither the record nor the current render.
        let ao = repo.join("AGENTS.md");
        let edited_block = "<!-- synaptic:start -->\n## Synaptic\nMY EDIT\n<!-- synaptic:end -->";
        std::fs::write(&ao, format!("{edited_block}\n")).unwrap();
        let mut reg = Registry::default();
        reg.entries.push(Entry {
            repo: repo_key(&repo),
            host: "agents".into(),
            version: "0.0.1".into(),
            skill_hash: None,
            block_hash: Some("deadbeefdeadbeef".into()),
        });
        reg.save(&path).unwrap();

        let summary = refresh_all(&path);
        assert!(summary.refreshed.is_empty(), "{summary:?}");
        assert_eq!(summary.skipped_edited.len(), 1, "{summary:?}");
        // The user's edit is preserved.
        assert!(std::fs::read_to_string(&ao).unwrap().contains("MY EDIT"));
    }

    #[test]
    fn refresh_drops_a_vanished_install() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skills.toml");
        let mut reg = Registry::default();
        reg.entries.push(Entry {
            repo: dir.path().join("gone").to_string_lossy().into_owned(),
            host: "claude".into(),
            version: "0.3.0".into(),
            skill_hash: Some("aaaa".into()),
            block_hash: Some("bbbb".into()),
        });
        reg.save(&path).unwrap();

        let summary = refresh_all(&path);
        assert_eq!(summary.dropped.len(), 1, "{summary:?}");
        assert!(Registry::load(&path).entries.is_empty(), "entry pruned");
    }

    #[test]
    fn refresh_reports_up_to_date_after_fresh_install() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skills.toml");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        install_to(&repo, Platform::Claude);
        record_install(&path, Platform::Claude, &repo);

        // Nothing changed since install -> all units current, no rewrite.
        let summary = refresh_all(&path);
        assert!(summary.refreshed.is_empty(), "{summary:?}");
        assert!(summary.skipped_edited.is_empty(), "{summary:?}");
        assert_eq!(summary.up_to_date, 1);
    }
}
