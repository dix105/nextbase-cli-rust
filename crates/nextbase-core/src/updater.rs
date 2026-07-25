//! Update checking against GitHub Releases.
//!
//! The TypeScript build compared the `master` commit SHA and re-ran an installer
//! that did `npm install` plus `tsc` on the user's machine — so every commit to
//! master shipped to users within the check interval, and a bad commit could brick
//! a listener. Releases are tagged instead, and an update is a binary swap.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

const RELEASES_URL: &str = "https://api.github.com/repos/dix105/nextbase-cli-rust/releases/latest";

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Both binaries ship together and share the core, so a partial update would pair
/// a new `wisper` with an old `nextbase`.
const BINARIES: [&str; 2] = ["wisper", "nextbase"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    UpToDate { version: String },
    Available { current: String, latest: String },
    NoReleases,
}

/// A published release and the assets attached to it.
#[derive(Debug, Clone)]
pub struct Release {
    pub tag: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

impl Release {
    fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == name)
    }

    pub fn is_newer_than_current(&self) -> bool {
        is_newer(&self.tag, CURRENT_VERSION)
    }
}

/// The target triple this binary was built for, matching the release asset names.
pub fn build_target() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        (os, arch) => bail!("No release binaries are published for {os} on {arch} yet."),
    })
}

/// Release assets are the bare binaries, named `<binary>-<target>[.exe]`, so an
/// update is one download and a rename — no archive handling on the client.
fn asset_name(binary: &str, target: &str) -> String {
    format!("{binary}-{target}{}", std::env::consts::EXE_SUFFIX)
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

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        // GitHub rejects requests without a user agent.
        .user_agent("nextbase-wisper-updater")
        .build()?)
}

/// Fetch the latest published release. `None` means nothing is published yet.
///
/// Draft releases are excluded by this endpoint, so a half-finished release build
/// never reaches users.
pub async fn latest_release() -> Result<Option<Release>> {
    let response = client()?
        .get(RELEASES_URL)
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .context("Could not reach GitHub to check for updates")?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        bail!("Update check failed: HTTP {}", response.status().as_u16());
    }

    let body: serde_json::Value = response.json().await?;
    let Some(tag) = body.get("tag_name").and_then(|v| v.as_str()) else {
        return Ok(None);
    };

    let assets = body
        .get("assets")
        .and_then(|value| value.as_array())
        .map(|assets| {
            assets
                .iter()
                .filter_map(|asset| {
                    Some(Asset {
                        name: asset.get("name")?.as_str()?.to_string(),
                        url: asset.get("browser_download_url")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(Some(Release {
        tag: tag.to_string(),
        assets,
    }))
}

pub async fn check() -> Result<UpdateStatus> {
    let Some(release) = latest_release().await? else {
        return Ok(UpdateStatus::NoReleases);
    };

    Ok(if release.is_newer_than_current() {
        UpdateStatus::Available {
            current: CURRENT_VERSION.to_string(),
            latest: release.tag,
        }
    } else {
        UpdateStatus::UpToDate {
            version: CURRENT_VERSION.to_string(),
        }
    })
}

/// Which binaries an update replaced.
#[derive(Debug, Clone)]
pub struct Applied {
    pub from: String,
    pub to: String,
    pub replaced: Vec<PathBuf>,
}

/// The directory the running binary lives in. Both `wisper` and `nextbase` are
/// installed side by side, so this is where replacements go.
fn install_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("Could not determine the path of this binary")?;
    // Resolve symlinks: replacing a link would leave the real binary stale.
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    exe.parent()
        .map(Path::to_path_buf)
        .context("Could not determine the install directory")
}

fn staged_path(target: &Path) -> PathBuf {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    target.with_file_name(format!("{name}.new"))
}

fn retired_path(target: &Path) -> PathBuf {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    target.with_file_name(format!("{name}.old"))
}

/// Remove leftovers from an earlier update.
///
/// Windows cannot delete the running image, so the previous binary is moved aside
/// and only disappears on a later run — this is that later run.
pub fn clean_stale() {
    let Ok(dir) = install_dir() else { return };
    for binary in BINARIES {
        let target = dir.join(format!("{binary}{}", std::env::consts::EXE_SUFFIX));
        let _ = std::fs::remove_file(retired_path(&target));
        let _ = std::fs::remove_file(staged_path(&target));
    }
}

/// Write `bytes` over `target`.
///
/// Unix can unlink a running executable, so a rename over it is enough — the
/// running process keeps its own inode. Windows refuses to overwrite a running
/// image but does allow renaming it, so the old file is moved aside first and
/// deleted on a later run.
fn replace_binary(target: &Path, bytes: &[u8]) -> Result<()> {
    let staged = staged_path(target);
    std::fs::write(&staged, bytes)
        .with_context(|| format!("Could not write {}", staged.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("Could not make {} executable", staged.display()))?;
    }

    // A truncated download is indistinguishable from a good one until it runs, and
    // by then the old binary is gone. Check the replacement first.
    if let Err(error) = verify_runnable(&staged) {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }

    let retired = retired_path(target);
    let _ = std::fs::remove_file(&retired);
    let moved_aside = if target.exists() {
        std::fs::rename(target, &retired).with_context(|| {
            format!(
                "Could not move {} aside. Close anything running it and try again.",
                target.display()
            )
        })?;
        true
    } else {
        false
    };

    if let Err(error) = std::fs::rename(&staged, target) {
        // Put the working binary back rather than leaving nothing behind.
        if moved_aside {
            let _ = std::fs::rename(&retired, target);
        }
        let _ = std::fs::remove_file(&staged);
        return Err(anyhow::Error::new(error)
            .context(format!("Could not install the new {}", target.display())));
    }

    let _ = std::fs::remove_file(&retired);
    Ok(())
}

/// Run the downloaded binary's `--version` to prove it is a working executable.
fn verify_runnable(path: &Path) -> Result<()> {
    let output = std::process::Command::new(path)
        .arg("--version")
        .output()
        .with_context(|| format!("The downloaded binary would not run ({})", path.display()))?;

    if !output.status.success() {
        bail!(
            "The downloaded binary exited with an error, so it was not installed ({}).",
            path.display()
        );
    }
    Ok(())
}

async fn download(client: &reqwest::Client, asset: &Asset) -> Result<Vec<u8>> {
    let response = client
        .get(&asset.url)
        .send()
        .await
        .with_context(|| format!("Could not download {}", asset.name))?;

    if !response.status().is_success() {
        bail!(
            "Could not download {}: HTTP {}",
            asset.name,
            response.status().as_u16()
        );
    }

    let bytes = response.bytes().await?.to_vec();
    // Anything this small is an error page or a truncated transfer, not a binary.
    if bytes.len() < 500_000 {
        bail!(
            "{} downloaded as only {} bytes, which is not a complete binary.",
            asset.name,
            bytes.len()
        );
    }
    Ok(bytes)
}

/// Download `release` and replace the installed binaries.
///
/// The caller is responsible for stopping the listener first: it runs from the
/// same executable, which Windows will not let anything overwrite while it is
/// running, and which would keep running the old code everywhere else.
pub async fn apply(release: &Release) -> Result<Applied> {
    let target = build_target()?;
    let dir = install_dir()?;
    let client = client()?;

    // Download everything before touching the install, so a network failure
    // halfway through cannot leave a new wisper next to an old nextbase.
    let mut pending = Vec::new();
    for binary in BINARIES {
        let path = dir.join(format!("{binary}{}", std::env::consts::EXE_SUFFIX));
        // `nextbase` may not be installed; only replace what is actually there.
        if !path.exists() {
            continue;
        }
        let name = asset_name(binary, target);
        let Some(asset) = release.asset(&name) else {
            bail!(
                "Release {} has no asset named {name}. Update by re-running the installer instead.",
                release.tag
            );
        };
        pending.push((path, download(&client, asset).await?));
    }

    if pending.is_empty() {
        bail!(
            "Found no installed binary to replace in {}. Re-run the installer instead.",
            dir.display()
        );
    }

    let mut replaced = Vec::new();
    for (path, bytes) in &pending {
        replace_binary(path, bytes)?;
        replaced.push(path.clone());
    }

    Ok(Applied {
        from: CURRENT_VERSION.to_string(),
        to: release.tag.clone(),
        replaced,
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

    #[test]
    fn asset_names_match_the_release_workflow() {
        let name = asset_name("wisper", "x86_64-pc-windows-msvc");
        if cfg!(windows) {
            assert_eq!(name, "wisper-x86_64-pc-windows-msvc.exe");
        } else {
            assert_eq!(name, "wisper-x86_64-pc-windows-msvc");
        }
    }

    #[test]
    fn this_build_has_a_known_release_target() {
        // A platform with no published binaries must fail the update with a clear
        // message rather than downloading someone else's architecture.
        assert!(build_target().is_ok());
    }

    #[test]
    fn side_files_sit_next_to_the_binary() {
        let target = Path::new("/opt/bin/wisper.exe");
        assert_eq!(staged_path(target), Path::new("/opt/bin/wisper.exe.new"));
        assert_eq!(retired_path(target), Path::new("/opt/bin/wisper.exe.old"));
    }

    #[cfg(unix)]
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wisper-update-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A payload that runs and exits 0, standing in for a real binary. Copying a
    /// system binary does not work: macOS kills the copy for failing code-signing.
    #[cfg(unix)]
    const RUNNABLE: &[u8] = b"#!/bin/sh\nexit 0\n";

    #[cfg(unix)]
    #[test]
    fn a_replaced_binary_leaves_no_side_files_behind() {
        let dir = scratch("swap");
        let target = dir.join("wisper");
        std::fs::write(&target, b"the old build").unwrap();

        replace_binary(&target, RUNNABLE).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), RUNNABLE);
        assert!(!staged_path(&target).exists());
        assert!(!retired_path(&target).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_payload_that_cannot_run_leaves_the_old_binary_working() {
        let dir = scratch("unrunnable");
        let target = dir.join("wisper");
        std::fs::write(&target, b"the old build").unwrap();

        // A proxy or a rate-limit page can hand back something that is not a
        // binary. Installing it would leave the user with no working CLI at all.
        let error = replace_binary(&target, b"#!/nonexistent/interpreter\n").unwrap_err();

        assert!(error.to_string().contains("would not run"));
        assert_eq!(std::fs::read(&target).unwrap(), b"the old build");
        assert!(!staged_path(&target).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_payload_that_runs_but_fails_leaves_the_old_binary_working() {
        let dir = scratch("failing");
        let target = dir.join("wisper");
        std::fs::write(&target, b"the old build").unwrap();

        let error = replace_binary(&target, b"#!/bin/sh\nexit 3\n").unwrap_err();

        assert!(error.to_string().contains("exited with an error"));
        assert_eq!(std::fs::read(&target).unwrap(), b"the old build");
        assert!(!staged_path(&target).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn assets_are_looked_up_by_exact_name() {
        let release = Release {
            tag: "v0.2.0".into(),
            assets: vec![
                Asset {
                    name: "wisper-aarch64-apple-darwin".into(),
                    url: "https://example.invalid/a".into(),
                },
                Asset {
                    name: "nextbase-wisper-aarch64-apple-darwin.tar.gz".into(),
                    url: "https://example.invalid/b".into(),
                },
            ],
        };

        assert_eq!(
            release.asset("wisper-aarch64-apple-darwin").map(|a| &a.url),
            Some(&"https://example.invalid/a".to_string())
        );
        // The archive must not satisfy a request for the bare binary.
        assert!(release.asset("wisper-x86_64-apple-darwin").is_none());
        assert!(release.is_newer_than_current());
    }
}
