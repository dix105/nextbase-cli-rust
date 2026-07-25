//! Bringing existing audio in: a local file, or a remote URL.
//!
//! Two rules shape this:
//!
//! 1. **The original bytes go to the provider.** Sarvam accepts mp3, m4a, flac, ogg,
//!    opus, webm and more natively, so re-encoding before upload would only add a
//!    conversion step to blame for a bad transcript. The file is submitted as it
//!    arrived.
//! 2. **A converted copy is only for sampling.** The quality gate has to cut a
//!    three-minute slice, and slicing is sample-exact WAV work. When the import is not
//!    WAV, `ffmpeg` makes a 16 kHz mono copy *just for that* — and when `ffmpeg` is
//!    absent, the gate is skipped and said so rather than silently transcoding.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Refuse absurd downloads rather than filling the disk. Two hours of 16 kHz mono is
/// ~115 MB; even a lossless multi-hour recording stays well inside this.
const MAX_DOWNLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Formats Sarvam documents as accepted, so they can be submitted untouched.
const SUPPORTED: [&str; 13] = [
    "wav", "mp3", "aac", "aiff", "ogg", "opus", "flac", "mp4", "m4a", "amr", "wma", "webm", "pcm",
];

/// An import ready for the pipeline.
#[derive(Debug, Clone)]
pub struct Imported {
    /// What goes to the provider — the original bytes.
    pub audio: PathBuf,
    /// A WAV the sample gate can slice, when one is available.
    pub sampleable: Option<PathBuf>,
    /// Why the gate cannot run, when it cannot.
    pub gate_blocked: Option<String>,
    pub downloaded: bool,
}

pub fn is_remote(source: &str) -> bool {
    let lower = source.trim().to_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn extension_of(path: &Path) -> Option<String> {
    path.extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
}

fn is_wav(path: &Path) -> bool {
    extension_of(path).as_deref() == Some("wav")
}

/// Whether a slice can be cut from this file directly.
fn sliceable(path: &Path) -> bool {
    is_wav(path) && crate::wav::info(path).is_ok()
}

pub fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Fetch or copy `source` into `directory` and work out how it can be handled.
pub async fn prepare(source: &str, directory: &Path) -> Result<Imported> {
    std::fs::create_dir_all(directory)?;

    let (audio, downloaded) = if is_remote(source) {
        (download(source, directory).await?, true)
    } else {
        (copy_local(source, directory)?, false)
    };

    if let Some(extension) = extension_of(&audio) {
        if !SUPPORTED.contains(&extension.as_str()) {
            // Not fatal — the provider may still cope — but worth saying plainly.
            crate::log::log(&format!(
                "Imported audio has an unrecognised extension (.{extension}); Sarvam may reject it."
            ));
        }
    }

    if sliceable(&audio) {
        return Ok(Imported {
            audio,
            sampleable: None,
            gate_blocked: None,
            downloaded,
        });
    }

    // Not sliceable: make a WAV copy purely so the gate has something to cut.
    if !ffmpeg_available() {
        return Ok(Imported {
            audio,
            sampleable: None,
            gate_blocked: Some(
                "The sample quality check needs a WAV to cut from, and ffmpeg is not installed to make one. The whole file will be transcribed in one pass instead.".to_string(),
            ),
            downloaded,
        });
    }

    let converted = directory.join("sample-source.wav");
    match convert_for_sampling(&audio, &converted) {
        Ok(()) => Ok(Imported {
            audio,
            sampleable: Some(converted),
            gate_blocked: None,
            downloaded,
        }),
        Err(error) => Ok(Imported {
            audio,
            sampleable: None,
            gate_blocked: Some(format!(
                "Could not build a WAV for the sample check ({error}). The whole file will be transcribed in one pass instead."
            )),
            downloaded,
        }),
    }
}

fn copy_local(source: &str, directory: &Path) -> Result<PathBuf> {
    let source = source.trim();
    let path = source
        .strip_prefix("file://")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(source));

    if !path.is_file() {
        bail!("Audio file not found: {}", path.display());
    }

    // Created here as well as in `prepare`: a helper that only works when the caller
    // happened to make the directory first is a trap for the next caller.
    std::fs::create_dir_all(directory)?;
    let extension = extension_of(&path).unwrap_or_else(|| "wav".to_string());
    let destination = directory.join(format!("audio.{extension}"));

    // Copied rather than referenced: a meeting's directory has to stay complete on its
    // own, and the original must not be touched by later cleanup.
    std::fs::copy(&path, &destination)
        .with_context(|| format!("Could not copy {}", path.display()))?;
    Ok(destination)
}

async fn download(url: &str, directory: &Path) -> Result<PathBuf> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30 * 60))
        .user_agent("nextbase-meeting")
        .build()?;

    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Could not download {url}"))?;
    if !response.status().is_success() {
        bail!(
            "Could not download {url}: HTTP {}",
            response.status().as_u16()
        );
    }

    if let Some(length) = response.content_length() {
        if length > MAX_DOWNLOAD_BYTES {
            bail!(
                "That file is {:.1} GB, which is larger than this will download.",
                length as f64 / 1024.0 / 1024.0 / 1024.0
            );
        }
    }

    // Prefer the URL's own extension; fall back to the content type, then to wav.
    let parsed = reqwest::Url::parse(url).with_context(|| format!("Not a valid URL: {url}"))?;
    let from_url = Path::new(parsed.path())
        .extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
        .filter(|ext| SUPPORTED.contains(&ext.as_str()));
    let extension = from_url
        .or_else(|| {
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(extension_for_content_type)
        })
        .unwrap_or_else(|| "wav".to_string());

    std::fs::create_dir_all(directory)?;
    let destination = directory.join(format!("audio.{extension}"));
    let bytes = response
        .bytes()
        .await
        .context("The download was cut short")?;
    if bytes.len() < 1024 {
        bail!(
            "That URL returned only {} bytes, which is not audio.",
            bytes.len()
        );
    }
    std::fs::write(&destination, &bytes)
        .with_context(|| format!("Could not write {}", destination.display()))?;
    Ok(destination)
}

fn extension_for_content_type(content_type: &str) -> Option<String> {
    let value = content_type.split(';').next()?.trim().to_lowercase();
    Some(
        match value.as_str() {
            "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
            "audio/mpeg" | "audio/mp3" => "mp3",
            "audio/mp4" | "audio/x-m4a" => "m4a",
            "audio/aac" => "aac",
            "audio/ogg" | "application/ogg" => "ogg",
            "audio/opus" => "opus",
            "audio/flac" | "audio/x-flac" => "flac",
            "audio/webm" | "video/webm" => "webm",
            "video/mp4" => "mp4",
            _ => return None,
        }
        .to_string(),
    )
}

/// 16 kHz mono WAV, for cutting a sample only.
fn convert_for_sampling(source: &Path, destination: &Path) -> Result<()> {
    let output = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(source)
        .args(["-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le"])
        .arg(destination)
        .output()
        .context("Could not run ffmpeg")?;

    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        bail!(
            "ffmpeg failed: {}",
            message.lines().last().unwrap_or("unknown error").trim()
        );
    }
    if !destination.is_file() {
        bail!("ffmpeg produced no output file");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_sources_are_recognised_by_scheme() {
        assert!(is_remote("https://example.com/a.mp3"));
        assert!(is_remote("HTTP://example.com/a.wav"));
        assert!(!is_remote("/Users/me/meeting.m4a"));
        assert!(!is_remote("file:///Users/me/meeting.wav"));
        assert!(!is_remote("~/recording.wav"));
    }

    #[test]
    fn content_types_map_to_the_extensions_sarvam_accepts() {
        assert_eq!(
            extension_for_content_type("audio/mpeg").as_deref(),
            Some("mp3")
        );
        assert_eq!(
            extension_for_content_type("audio/wav; charset=binary").as_deref(),
            Some("wav")
        );
        assert_eq!(
            extension_for_content_type("audio/x-m4a").as_deref(),
            Some("m4a")
        );
        // An unknown type must not invent an extension.
        assert_eq!(extension_for_content_type("application/octet-stream"), None);
        assert_eq!(extension_for_content_type("text/html"), None);
    }

    #[test]
    fn every_mapped_extension_is_one_the_provider_accepts() {
        for content_type in [
            "audio/wav",
            "audio/mpeg",
            "audio/mp4",
            "audio/aac",
            "audio/ogg",
            "audio/opus",
            "audio/flac",
            "audio/webm",
            "video/mp4",
        ] {
            let extension = extension_for_content_type(content_type).expect(content_type);
            assert!(
                SUPPORTED.contains(&extension.as_str()),
                "{content_type} -> {extension}"
            );
        }
    }

    #[test]
    fn a_missing_local_file_is_reported_by_path() {
        let error = copy_local("/definitely/not/here.wav", &std::env::temp_dir()).unwrap_err();
        assert!(error.to_string().contains("not found"), "{error}");
    }

    #[test]
    fn a_local_file_is_copied_with_its_extension_kept() {
        let dir = std::env::temp_dir().join(format!("nextbase-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let source = dir.join("original.m4a");
        std::fs::write(&source, b"not really audio").unwrap();
        let copied = copy_local(source.to_str().unwrap(), &dir.join("meeting")).unwrap();

        assert!(copied.ends_with("audio.m4a"));
        // The original must survive: later cleanup only touches the meeting directory.
        assert!(source.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_url_is_treated_as_a_local_path() {
        let dir = std::env::temp_dir().join(format!("nextbase-import-url-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let source = dir.join("clip.wav");
        std::fs::write(&source, b"stub").unwrap();
        let copied = copy_local(&format!("file://{}", source.display()), &dir).unwrap();
        assert!(copied.ends_with("audio.wav"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
