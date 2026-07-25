//! Transcription across all four providers.
//!
//! Wisper records short hold-to-talk clips, so every provider is called through
//! its direct endpoint. Sarvam's Batch job API (upload, poll, download, speaker
//! diarization) exists for long meeting audio and belongs to NoteBot, not here.

use anyhow::{bail, Context, Result};
use reqwest::multipart::{Form, Part};
use std::path::Path;

use crate::config::{Config, Provider};

const CODEX_MAX_BYTES: u64 = 25 * 1024 * 1024;

/// Sarvam's direct endpoint rejects audio over ~30s. Only these markers trigger
/// the Groq fallback — anything else is a real error the user needs to see.
const DURATION_MARKERS: [&str; 5] = [
    "duration",
    "30 second",
    "maximum limit",
    "too long",
    "exceeds",
];

pub async fn transcribe_file(path: &Path, config: &Config) -> Result<String> {
    let Some(provider) = config.provider else {
        bail!("No provider configured. Run: wisper setup");
    };
    let key = config
        .key_for(provider)
        .filter(|key| !key.is_empty())
        .with_context(|| format!("No API key saved for {provider}. Run: wisper setup"))?;

    match provider {
        Provider::Groq => {
            let model = config.model.as_deref().unwrap_or("whisper-large-v3-turbo");
            groq(path, key, model).await
        }
        Provider::ElevenLabs => {
            let model = config.model.as_deref().unwrap_or("scribe_v2");
            eleven_labs(path, key, model).await
        }
        Provider::NextbaseCodex => nextbase_codex(path, key).await,
        Provider::Sarvam => {
            let model = config.model.as_deref().unwrap_or("saaras:v3");
            match sarvam(path, key, model).await {
                Ok(text) => Ok(text),
                Err(error) => {
                    let message = error.to_string().to_lowercase();
                    let too_long = DURATION_MARKERS.iter().any(|m| message.contains(m));
                    match config.key_for(Provider::Groq) {
                        // Keeps long files working without pulling in chunking,
                        // which needs the audio layer.
                        Some(groq_key) if too_long => {
                            eprintln!(
                                "Sarvam could not handle this audio ({error}). Falling back to Groq Whisper."
                            );
                            groq(path, groq_key, "whisper-large-v3-turbo").await
                        }
                        _ if too_long => bail!(
                            "{error}\nSarvam's direct endpoint is limited to ~30s of audio. Add a Groq key for longer files: wisper provider"
                        ),
                        _ => Err(error),
                    }
                }
            }
        }
    }
}

/// Providers infer the format from the filename, so a name without a known audio
/// extension gets `.wav` appended — matching what the recorder writes.
fn audio_part(path: &Path) -> Result<Part> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("Could not read audio file: {}", path.display()))?;

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "audio.wav".to_string());
    let known = matches!(
        path.extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .as_deref(),
        Some("wav" | "mp3" | "flac" | "m4a" | "ogg" | "opus" | "webm" | "mp4" | "mpeg" | "mpga")
    );
    let file_name = if known { name } else { format!("{name}.wav") };

    Ok(Part::bytes(bytes)
        .file_name(file_name)
        .mime_str("audio/wav")?)
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        // Transcription of a long file can legitimately take a while.
        .timeout(std::time::Duration::from_secs(300))
        .build()?)
}

/// Providers disagree on where they put an error string, so try each shape.
fn error_message(body: &serde_json::Value, status: u16, provider: &str) -> String {
    let candidates = [
        body.pointer("/error/message"),
        body.get("message"),
        body.get("detail"),
        body.pointer("/detail/message"),
    ];

    for candidate in candidates.into_iter().flatten() {
        if let Some(text) = candidate.as_str() {
            if !text.trim().is_empty() {
                return text.to_string();
            }
        } else if !candidate.is_null() {
            return candidate.to_string();
        }
    }

    format!("{provider} transcription failed: HTTP {status}")
}

fn transcript_from(body: &serde_json::Value) -> String {
    body.get("text")
        .or_else(|| body.get("transcript"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string()
}

async fn send(request: reqwest::RequestBuilder, provider: &str) -> Result<String> {
    let response = request.send().await?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);

    if !status.is_success() {
        bail!("{}", error_message(&body, status.as_u16(), provider));
    }
    Ok(transcript_from(&body))
}

async fn groq(path: &Path, key: &str, model: &str) -> Result<String> {
    let form = Form::new()
        .part("file", audio_part(path)?)
        .text("model", model.to_string())
        .text("response_format", "json");

    send(
        client()?
            .post("https://api.groq.com/openai/v1/audio/transcriptions")
            .bearer_auth(key)
            .multipart(form),
        "Groq",
    )
    .await
}

async fn eleven_labs(path: &Path, key: &str, model: &str) -> Result<String> {
    let form = Form::new()
        .part("file", audio_part(path)?)
        .text("model_id", model.to_string());

    send(
        client()?
            .post("https://api.elevenlabs.io/v1/speech-to-text")
            .header("xi-api-key", key)
            .multipart(form),
        "ElevenLabs",
    )
    .await
}

/// `saarika:v2` was retired in favour of `v2.5`; keep old configs working.
fn sarvam_model(model: &str) -> &str {
    if model == "saarika:v2" {
        "saarika:v2.5"
    } else {
        model
    }
}

async fn sarvam(path: &Path, key: &str, model: &str) -> Result<String> {
    let model = sarvam_model(model);
    let mut form = Form::new()
        .part("file", audio_part(path)?)
        .text("model", model.to_string())
        .text("language_code", "unknown");
    if model == "saaras:v3" {
        form = form.text("mode", "transcribe");
    }

    send(
        client()?
            .post("https://api.sarvam.ai/speech-to-text")
            .header("api-subscription-key", key)
            .multipart(form),
        "Sarvam",
    )
    .await
}

async fn nextbase_codex(path: &Path, key: &str) -> Result<String> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("Could not read audio file: {}", path.display()))?
        .len();
    if size > CODEX_MAX_BYTES {
        bail!("Nextbase Codex Transcribe accepts audio files up to 25 MiB.");
    }

    let form = Form::new()
        .part("file", audio_part(path)?)
        .text("model", "codex-transcribe")
        .text("response_format", "json");

    send(
        client()?
            .post("https://nextbase-model-gateway.infinitycorp.tech/v1/openai-codex/audio/transcriptions")
            .header("x-api-key", key)
            .multipart(form),
        "Nextbase Codex",
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn retired_sarvam_model_is_upgraded() {
        assert_eq!(sarvam_model("saarika:v2"), "saarika:v2.5");
        assert_eq!(sarvam_model("saaras:v3"), "saaras:v3");
    }

    #[test]
    fn error_messages_are_found_in_every_provider_shape() {
        assert_eq!(
            error_message(&json!({"error": {"message": "bad key"}}), 401, "Groq"),
            "bad key"
        );
        assert_eq!(
            error_message(&json!({"detail": "not allowed"}), 403, "ElevenLabs"),
            "not allowed"
        );
        assert_eq!(
            error_message(&json!({"message": "nope"}), 400, "Sarvam"),
            "nope"
        );
        assert_eq!(
            error_message(&json!({}), 500, "Groq"),
            "Groq transcription failed: HTTP 500"
        );
    }

    #[test]
    fn transcript_is_read_from_either_field_name() {
        assert_eq!(transcript_from(&json!({"text": " hi "})), "hi");
        assert_eq!(
            transcript_from(&json!({"transcript": "namaste"})),
            "namaste"
        );
        assert_eq!(transcript_from(&json!({})), "");
    }

    #[test]
    fn only_duration_errors_trigger_the_groq_fallback() {
        let duration = "audio duration exceeds the maximum limit";
        assert!(DURATION_MARKERS.iter().any(|m| duration.contains(m)));

        let auth = "invalid api subscription key";
        assert!(!DURATION_MARKERS.iter().any(|m| auth.contains(m)));
    }
}
