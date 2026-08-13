//! Opt-in self-update for the Synaptic CLI.
//!
//! Nothing here touches the network or the binary unless the user has opted in
//! (`self-update --enable` for the background check) or explicitly run the
//! update command. See the [`check`] module for the throttled background notice
//! and the [`updater`] module for the actual download/verify/replace pipeline.

pub mod check;
pub mod config;
pub mod github;
pub mod target;
pub mod updater;
pub mod version;

pub use config::{UpdateConfig, config_path};
pub use github::{Asset, Release, latest_release};
pub use version::version_is_newer;

/// The repository self-update queries for releases.
pub const REPO_OWNER: &str = "ColinVaughn";
pub const REPO_NAME: &str = "Synaptic";

/// Human-facing releases page, shown when no prebuilt asset fits the platform.
pub fn releases_url() -> String {
    format!("https://github.com/{REPO_OWNER}/{REPO_NAME}/releases")
}
