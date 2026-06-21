//! Fetch the latest GitHub release metadata for the Synaptic repo.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::{REPO_NAME, REPO_OWNER};

/// One downloadable release asset.
#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

/// The subset of a GitHub release we use.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    /// The tag name, e.g. "v0.3.1".
    pub version: String,
    /// The release notes body (may be empty).
    pub notes: String,
    pub assets: Vec<Asset>,
}

// Wire shapes matching the GitHub REST response.
#[derive(Deserialize)]
struct WireRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<WireAsset>,
}

#[derive(Deserialize)]
struct WireAsset {
    name: String,
    browser_download_url: String,
}

/// Parse a `/releases/latest` JSON body into a [`Release`].
pub fn parse_latest(json: &str) -> Result<Release> {
    let w: WireRelease = serde_json::from_str(json).context("parsing release JSON")?;
    Ok(Release {
        version: w.tag_name,
        notes: w.body.unwrap_or_default(),
        assets: w
            .assets
            .into_iter()
            .map(|a| Asset {
                name: a.name,
                url: a.browser_download_url,
            })
            .collect(),
    })
}

/// Fetch the latest release from GitHub. Short timeout; honors `GITHUB_TOKEN`
/// (optional, raises the anonymous rate limit). Network failures are surfaced as
/// errors for the caller to handle (the background check swallows them).
pub fn latest_release() -> Result<Release> {
    let url = format!("https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/releases/latest");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(concat!("synaptic-update/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;
    let mut req = client
        .get(&url)
        .header("Accept", "application/vnd.github+json");
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.is_empty() {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
    }
    let resp = req.send().context("requesting latest release")?;
    let resp = resp
        .error_for_status()
        .context("GitHub returned an error")?;
    let body = resp.text().context("reading release body")?;
    parse_latest(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "tag_name": "v0.3.1",
        "body": "Changes - fix things",
        "assets": [
            {"name": "synaptic-x86_64-unknown-linux-gnu.tar.gz",
             "browser_download_url": "https://example.com/a.tar.gz"},
            {"name": "synaptic-x86_64-unknown-linux-gnu.tar.gz.sha256",
             "browser_download_url": "https://example.com/a.sha256"}
        ]
    }"#;

    #[test]
    fn parses_release_metadata() {
        let r = parse_latest(SAMPLE).unwrap();
        assert_eq!(r.version, "v0.3.1");
        assert_eq!(r.notes, "Changes - fix things");
        assert_eq!(r.assets.len(), 2);
        let a = r
            .assets
            .iter()
            .find(|a| a.name.ends_with(".tar.gz"))
            .unwrap();
        assert_eq!(a.url, "https://example.com/a.tar.gz");
    }
}
