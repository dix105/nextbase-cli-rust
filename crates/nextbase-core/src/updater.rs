//! Update checking against GitHub Releases.
//!
//! The TypeScript build compared the `master` commit SHA and re-ran an installer
//! that did `npm install` plus `tsc` on the user's machine — so every commit to
//! master shipped to users within the check interval, and a bad commit could brick
//! a listener. Releases are tagged instead, and an update is a binary swap.

use anyhow::{bail, Context, Result};

const RELEASES_URL: &str = "https://api.github.com/repos/Nextbasedev/nextbase-rs/releases/latest";

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    UpToDate { version: String },
    Available { current: String, latest: String },
    NoReleases,
}

/// Compare dotted numeric versions. Non-numeric segments sort as 0, which is fine
/// for the `MAJOR.MINOR.PATCH` tags this project publishes.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |value: &str| -> Vec<u64> {
        value
            .trim_start_matches('v')
            .split(['.', '-', '+'])
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect()
    };

    let candidate = parse(candidate);
    let current = parse(current);
    let width = candidate.len().max(current.len());

    for index in 0..width {
        let a = candidate.get(index).copied().unwrap_or(0);
        let b = current.get(index).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    false
}

pub async fn check() -> Result<UpdateStatus> {
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?
        .get(RELEASES_URL)
        .header("accept", "application/vnd.github+json")
        // GitHub rejects requests without one.
        .header("user-agent", "nextbase-wisper-updater")
        .send()
        .await
        .context("Could not reach GitHub to check for updates")?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(UpdateStatus::NoReleases);
    }
    if !response.status().is_success() {
        bail!("Update check failed: HTTP {}", response.status().as_u16());
    }

    let body: serde_json::Value = response.json().await?;
    let Some(tag) = body.get("tag_name").and_then(|v| v.as_str()) else {
        return Ok(UpdateStatus::NoReleases);
    };

    Ok(if is_newer(tag, CURRENT_VERSION) {
        UpdateStatus::Available {
            current: CURRENT_VERSION.to_string(),
            latest: tag.to_string(),
        }
    } else {
        UpdateStatus::UpToDate {
            version: CURRENT_VERSION.to_string(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions_are_detected() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.1.10", "0.1.9"));
    }

    #[test]
    fn same_or_older_is_not_an_update() {
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.9.9", "1.0.0"));
    }

    #[test]
    fn missing_segments_count_as_zero() {
        assert!(is_newer("0.2", "0.1.9"));
        assert!(!is_newer("0.1", "0.1.0"));
    }
}
