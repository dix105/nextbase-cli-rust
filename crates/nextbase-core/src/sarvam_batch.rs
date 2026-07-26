//! Sarvam Batch speech-to-text: long meeting audio with speaker diarization.
//!
//! Wisper's direct endpoint (`transcribe.rs`) caps out around 30 seconds. Batch
//! takes hours of audio and returns diarized, timestamped output, which is what a
//! meeting needs.
//!
//! Documented limits this module is built around: **2 hours per file** and **20
//! files per job**, with **chunk-level timestamps only** — never word-level. The
//! output JSON schema is *not* documented, so parsing is deliberately defensive and
//! a missing field means absent data, not an error.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

const BASE: &str = "https://api.sarvam.ai/speech-to-text/job/v1";

/// Documented ceiling for one input file.
pub const MAX_FILE_DURATION: Duration = Duration::from_secs(2 * 60 * 60);
/// Documented ceiling for one job.
pub const MAX_FILES_PER_JOB: usize = 20;

/// How the model should treat the audio. Only these two matter for meetings; the
/// API also accepts translate, verbatim and translit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Transcribe in the spoken language.
    Transcribe,
    /// Keep Indic speech in Latin script with English technical terms intact.
    Codemix,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Transcribe => "transcribe",
            Mode::Codemix => "codemix",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "transcribe" => Some(Mode::Transcribe),
            "codemix" | "code-mix" | "code_mix" => Some(Mode::Codemix),
            _ => None,
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct BatchOptions {
    pub model: String,
    pub mode: Mode,
    /// `unknown` lets Sarvam detect. Only set a specific code when the user has.
    pub language_code: String,
    pub with_diarization: bool,
    pub with_timestamps: bool,
    pub num_speakers: Option<u8>,
}

impl Default for BatchOptions {
    fn default() -> Self {
        Self {
            model: "saaras:v3".to_string(),
            mode: Mode::Transcribe,
            language_code: "unknown".to_string(),
            with_diarization: true,
            with_timestamps: true,
            num_speakers: None,
        }
    }
}

/// One diarized segment. Timestamps are phrase-level, never per word.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    /// Generic label straight from the provider. **Not** a person.
    pub speaker: Option<String>,
    pub text: String,
    pub start_seconds: Option<f64>,
    pub end_seconds: Option<f64>,
}

/// Parsed output for one input file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Transcription {
    pub text: String,
    pub segments: Vec<Segment>,
    /// Provider language *detection*. Never an accuracy figure.
    pub language_code: Option<String>,
}

impl Transcription {
    /// Distinct generic labels seen. Reported as a count, never resolved to names.
    pub fn speaker_labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = self
            .segments
            .iter()
            .filter_map(|s| s.speaker.clone())
            .collect();
        labels.sort();
        labels.dedup();
        labels
    }

    /// First and last timestamp, for comparing against the source duration.
    pub fn coverage(&self) -> Option<(f64, f64)> {
        let first = self
            .segments
            .iter()
            .filter_map(|s| s.start_seconds)
            .fold(f64::INFINITY, f64::min);
        let last = self
            .segments
            .iter()
            .filter_map(|s| s.end_seconds)
            .fold(f64::NEG_INFINITY, f64::max);
        if first.is_finite() && last.is_finite() {
            Some((first, last))
        } else {
            None
        }
    }

    /// Segments that overlap the one before them. Normal in conversation, but they
    /// lower attribution confidence, so the count is reported rather than hidden.
    pub fn overlapping_segments(&self) -> usize {
        let mut overlaps = 0;
        let mut previous_end: Option<f64> = None;
        for segment in &self.segments {
            if let (Some(start), Some(end)) = (segment.start_seconds, previous_end) {
                if start < end {
                    overlaps += 1;
                }
            }
            if let Some(end) = segment.end_seconds {
                previous_end = Some(end);
            }
        }
        overlaps
    }

    /// Diarized lines if present, otherwise the plain transcript.
    pub fn as_labelled_text(&self) -> String {
        if self.segments.is_empty() {
            return self.text.clone();
        }
        self.segments
            .iter()
            .map(|segment| {
                let stamp = segment
                    .start_seconds
                    .map(|s| format!("[{}] ", clock(s)))
                    .unwrap_or_default();
                let speaker = segment.speaker.clone().unwrap_or_else(|| "UNKNOWN".into());
                format!("{stamp}{speaker}: {}", segment.text)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// `h:mm:ss` for a seconds offset.
pub fn clock(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Everything a completed job produced, plus what it cost in wall-clock time.
#[derive(Debug, Clone)]
pub struct BatchResult {
    pub job_id: String,
    pub outputs: Vec<Transcription>,
    /// Input files the job could not process. Empty on a clean run.
    pub failed_inputs: Vec<String>,
    pub elapsed: Duration,
    /// True when the job finished as `PartiallyCompleted`.
    pub partial: bool,
}

impl BatchResult {
    /// All outputs joined, diarized where available.
    pub fn combined_text(&self) -> String {
        self.outputs
            .iter()
            .map(|output| output.as_labelled_text())
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn merged(&self) -> Transcription {
        let mut merged = Transcription::default();
        // Segment offsets restart per input file, so a split recording needs each
        // part shifted by the parts before it or every timestamp after the first
        // part would be wrong.
        let mut offset = 0.0_f64;
        for output in &self.outputs {
            let end = output
                .segments
                .iter()
                .filter_map(|s| s.end_seconds)
                .fold(0.0_f64, f64::max);
            for segment in &output.segments {
                merged.segments.push(Segment {
                    speaker: segment.speaker.clone(),
                    text: segment.text.clone(),
                    start_seconds: segment.start_seconds.map(|s| s + offset),
                    end_seconds: segment.end_seconds.map(|s| s + offset),
                });
            }
            if !merged.text.is_empty() && !output.text.is_empty() {
                merged.text.push(' ');
            }
            merged.text.push_str(&output.text);
            merged.language_code = merged
                .language_code
                .take()
                .or_else(|| output.language_code.clone());
            offset += end;
        }
        merged
    }
}

// ------------------------------------------------------------------ parsing

/// Pull a transcription out of a downloaded output file.
///
/// The schema is undocumented, so every field is optional and several spellings are
/// accepted. An unexpected shape yields empty data the caller can report, not a
/// parse error that loses an already-paid-for job.
pub fn parse_output(value: &serde_json::Value) -> Transcription {
    let text = ["transcript", "text"]
        .iter()
        .find_map(|key| value.get(*key).and_then(|v| v.as_str()))
        .unwrap_or_default()
        .trim()
        .to_string();

    let language_code = ["language_code", "language"]
        .iter()
        .find_map(|key| value.get(*key).and_then(|v| v.as_str()))
        .map(|v| v.to_string());

    let entries = value
        .pointer("/diarized_transcript/entries")
        .or_else(|| {
            value
                .get("diarized_transcript")
                .and_then(|d| d.as_array().map(|_| d))
        })
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let segments = entries
        .iter()
        .filter_map(|entry| {
            let text = ["transcript", "text"]
                .iter()
                .find_map(|key| entry.get(*key).and_then(|v| v.as_str()))
                .unwrap_or_default()
                .trim()
                .to_string();
            if text.is_empty() {
                return None;
            }
            Some(Segment {
                speaker: speaker_label(entry),
                text,
                start_seconds: number(entry, &["start_time_seconds", "start_time", "start"]),
                end_seconds: number(entry, &["end_time_seconds", "end_time", "end"]),
            })
        })
        .collect();

    Transcription {
        text,
        segments,
        language_code,
    }
}

/// Normalise whatever the provider used into `SPEAKER_00`.
///
/// Kept generic on purpose: the skill is explicit that these are never identities
/// unless the user validates a mapping.
fn speaker_label(entry: &serde_json::Value) -> Option<String> {
    let raw = ["speaker_id", "speaker", "speaker_label"]
        .iter()
        .find_map(|key| entry.get(*key))?;

    let label = match raw {
        serde_json::Value::Number(number) => format!("SPEAKER_{:02}", number.as_i64().unwrap_or(0)),
        serde_json::Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            match trimmed.parse::<i64>() {
                Ok(index) => format!("SPEAKER_{index:02}"),
                Err(_) => trimmed.to_string(),
            }
        }
        _ => return None,
    };
    Some(label)
}

fn number(entry: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| entry.get(*key))
        .and_then(|value| value.as_f64())
}

// ---------------------------------------------------------------- lifecycle

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobState {
    Accepted,
    Pending,
    Running,
    Completed,
    PartiallyCompleted,
    Failed,
    Unknown,
}

impl JobState {
    fn parse(value: &str) -> Self {
        match value.trim().to_lowercase().as_str() {
            "accepted" => Self::Accepted,
            "pending" => Self::Pending,
            "running" => Self::Running,
            "completed" => Self::Completed,
            // Undocumented but real: the old build hit it, and the skill expects it.
            "partiallycompleted" | "partially_completed" => Self::PartiallyCompleted,
            "failed" => Self::Failed,
            _ => Self::Unknown,
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::PartiallyCompleted | Self::Failed
        )
    }
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        // A two-hour upload needs room; the poll loop bounds total time instead.
        .timeout(Duration::from_secs(30 * 60))
        .build()?)
}

/// Sarvam's error bodies vary by endpoint, so all three shapes are tried before
/// falling back to the status code.
fn error_message(payload: &serde_json::Value, status: reqwest::StatusCode, what: &str) -> String {
    payload
        .pointer("/error/message")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("message").and_then(|v| v.as_str()))
        .or_else(|| payload.get("detail").and_then(|v| v.as_str()))
        .map(|m| m.to_string())
        .unwrap_or_else(|| format!("{what} failed: HTTP {}", status.as_u16()))
}

async fn post_json(
    client: &reqwest::Client,
    url: &str,
    key: &str,
    body: serde_json::Value,
    what: &str,
) -> Result<serde_json::Value> {
    let response = client
        .post(url)
        .header("api-subscription-key", key)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("Could not reach Sarvam to {what}"))?;

    let status = response.status();
    let payload: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        bail!("{}", error_message(&payload, status, what));
    }
    Ok(payload)
}

async fn get_json(
    client: &reqwest::Client,
    url: &str,
    key: &str,
    what: &str,
) -> Result<serde_json::Value> {
    let response = client
        .get(url)
        .header("api-subscription-key", key)
        .send()
        .await
        .with_context(|| format!("Could not reach Sarvam to {what}"))?;

    let status = response.status();
    let payload: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        bail!("{}", error_message(&payload, status, what));
    }
    Ok(payload)
}

/// Filename to register with the job. Providers infer the format from it.
fn upload_name(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "audio.wav".to_string());
    let known = matches!(
        path.extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .as_deref(),
        Some(
            "wav"
                | "mp3"
                | "flac"
                | "m4a"
                | "ogg"
                | "opus"
                | "webm"
                | "mp4"
                | "aac"
                | "amr"
                | "wma"
                | "aiff"
        )
    );
    if known {
        name
    } else {
        format!("{name}.wav")
    }
}

fn job_parameters(options: &BatchOptions) -> serde_json::Value {
    let mut parameters = serde_json::json!({
        "model": options.model,
        "mode": options.mode.as_str(),
        "language_code": options.language_code,
        "with_timestamps": options.with_timestamps,
        "with_diarization": options.with_diarization,
    });
    // Only sent when the user actually knows the count; guessing it would bias
    // diarization for no reason.
    if let Some(speakers) = options.num_speakers {
        parameters["num_speakers"] = serde_json::json!(speakers);
    }
    serde_json::json!({ "job_parameters": parameters })
}

/// Progress callback so a caller can show a spinner and log state changes: a queued
/// Batch job can sit for minutes and must never look like a hang.
pub type Progress<'a> = &'a (dyn Fn(&str) + Send + Sync);

/// Run one job end to end: initiate, upload, start, poll, download, parse.
///
/// `files` must already respect the documented limits; `submit` rejects a request
/// that cannot succeed rather than paying for a job that will fail.
pub async fn submit(
    files: &[PathBuf],
    key: &str,
    options: &BatchOptions,
    progress: Progress<'_>,
) -> Result<BatchResult> {
    if files.is_empty() {
        bail!("No audio files were given to transcribe.");
    }
    if files.len() > MAX_FILES_PER_JOB {
        bail!(
            "Sarvam Batch accepts {MAX_FILES_PER_JOB} files per job; {} were given.",
            files.len()
        );
    }

    let started = std::time::Instant::now();
    let client = client()?;

    progress("Creating the transcription job");
    let created = post_json(
        &client,
        BASE,
        key,
        job_parameters(options),
        "create the job",
    )
    .await?;
    let job_id = created
        .get("job_id")
        .and_then(|v| v.as_str())
        .context("Sarvam did not return a job_id.")?
        .to_string();

    let names: Vec<String> = files.iter().map(|path| upload_name(path)).collect();

    progress(&format!(
        "Requesting upload URLs for {} file(s)",
        files.len()
    ));
    let upload = post_json(
        &client,
        &format!("{BASE}/upload-files"),
        key,
        serde_json::json!({ "job_id": job_id, "files": names }),
        "request upload URLs",
    )
    .await?;

    let container = upload
        .get("storage_container_type")
        .or_else(|| created.get("storage_container_type"))
        .and_then(|value| value.as_str())
        .map(|value| value.to_string());

    for (path, name) in files.iter().zip(&names) {
        let details = signed_url(&upload, "upload_urls", name)
            .with_context(|| format!("Sarvam did not return an upload URL for {name}."))?;
        progress(&format!("Uploading {name}"));
        put_file(&client, &details, path, container.as_deref()).await?;
    }

    progress("Starting the job");
    post_json(
        &client,
        &format!("{BASE}/{job_id}/start"),
        key,
        serde_json::json!({}),
        "start the job",
    )
    .await?;

    let status = poll(&client, &job_id, key, progress).await?;
    let state = status
        .get("job_state")
        .and_then(|v| v.as_str())
        .map(JobState::parse)
        .unwrap_or(JobState::Unknown);

    if state == JobState::Failed {
        bail!(
            "{}",
            status
                .get("error_message")
                .and_then(|v| v.as_str())
                .filter(|m| !m.trim().is_empty())
                .unwrap_or("Sarvam Batch job failed.")
        );
    }

    let output_names = output_file_names(&status);
    let failed_inputs = failed_input_names(&status);
    if output_names.is_empty() {
        bail!(
            "{}",
            status
                .get("error_message")
                .and_then(|v| v.as_str())
                .filter(|m| !m.trim().is_empty())
                .unwrap_or("Sarvam Batch finished without producing any output.")
        );
    }

    progress(&format!(
        "Downloading {} output file(s)",
        output_names.len()
    ));
    let downloads = post_json(
        &client,
        &format!("{BASE}/download-files"),
        key,
        serde_json::json!({ "job_id": job_id, "files": output_names }),
        "request download URLs",
    )
    .await?;

    let mut outputs = Vec::new();
    for name in &output_names {
        let Some(details) = signed_url(&downloads, "download_urls", name) else {
            continue;
        };
        let body = get_json(&client, &details.url, key, "download the transcript").await?;
        outputs.push(parse_output(&body));
    }

    if outputs
        .iter()
        .all(|output| output.text.is_empty() && output.segments.is_empty())
    {
        bail!("Sarvam Batch returned an empty transcript.");
    }

    Ok(BatchResult {
        job_id,
        outputs,
        failed_inputs,
        elapsed: started.elapsed(),
        partial: state == JobState::PartiallyCompleted,
    })
}

struct SignedUrl {
    url: String,
    headers: Vec<(String, String)>,
}

/// Pick a file's signed URL out of the response, tolerating a key that does not
/// match the requested name exactly when only one entry came back.
fn signed_url(payload: &serde_json::Value, field: &str, name: &str) -> Option<SignedUrl> {
    let map = payload.get(field)?.as_object()?;
    let entry = map.get(name).or_else(|| {
        if map.len() == 1 {
            map.values().next()
        } else {
            None
        }
    })?;

    let url = entry.get("file_url").and_then(|v| v.as_str())?.to_string();
    let headers = entry
        .get("file_metadata")
        .and_then(|v| v.as_object())
        .map(|metadata| {
            metadata
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();

    Some(SignedUrl { url, headers })
}

/// Whether a signed URL points at Azure Blob Storage.
///
/// Sarvam reports this as `storage_container_type`, but the host is checked too: the
/// field is absent from some responses and the header below is mandatory, not optional.
fn is_azure_blob(url: &str, container: Option<&str>) -> bool {
    if let Some(container) = container {
        let lower = container.to_lowercase();
        if lower.starts_with("azure") {
            return true;
        }
        // A container Sarvam names explicitly as something else must not get an Azure
        // header: Google signed URLs reject headers they were not signed with.
        if lower == "google" || lower == "local" {
            return false;
        }
    }
    url.contains(".blob.core.windows.net")
}

async fn put_file(
    client: &reqwest::Client,
    details: &SignedUrl,
    path: &Path,
    container: Option<&str>,
) -> Result<()> {
    let bytes =
        std::fs::read(path).with_context(|| format!("Could not read {}", path.display()))?;

    let mut request = client.put(&details.url).body(bytes);

    // Azure's Put Blob requires `x-ms-blob-type` and answers 400 without it. Sarvam's
    // own examples use the Azure SDK, which sets it for you, which is why their docs
    // never mention it — a raw PUT has to send it itself. Skipping this is what made
    // "Sarvam signed upload failed: HTTP 400" happen on every long meeting.
    let already_typed = details
        .headers
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case("x-ms-blob-type"));
    if is_azure_blob(&details.url, container) && !already_typed {
        request = request.header("x-ms-blob-type", "BlockBlob");
    }

    // Anything the API did hand back is replayed as given: a signed URL rejects a
    // request whose headers differ from the ones it was signed with.
    for (key, value) in &details.headers {
        request = request.header(key.as_str(), value.as_str());
    }

    let response = request
        .send()
        .await
        .with_context(|| format!("Could not upload {}", path.display()))?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        // Azure explains the refusal in the body. Reporting only the status code is
        // what made the original failure impossible to diagnose.
        let detail = response
            .text()
            .await
            .ok()
            .map(|body| azure_error_message(&body))
            .filter(|message| !message.is_empty())
            .map(|message| format!(" — {message}"))
            .unwrap_or_default();
        bail!("Uploading {} failed: HTTP {status}{detail}", path.display());
    }
    Ok(())
}

/// Pull the human part out of Azure's XML error body.
fn azure_error_message(body: &str) -> String {
    let between = |open: &str, close: &str| -> Option<String> {
        let start = body.find(open)? + open.len();
        let end = body[start..].find(close)? + start;
        Some(body[start..end].trim().to_string())
    };

    let code = between("<Code>", "</Code>");
    let message = between("<Message>", "</Message>")
        // Azure appends a request id and timestamp on their own lines; the first line
        // is the sentence worth showing.
        .map(|message| {
            message
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string()
        });

    match (code, message) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (Some(code), None) => code,
        (None, Some(message)) => message,
        // Not XML: show a short prefix rather than a wall of HTML.
        (None, None) => body.trim().chars().take(200).collect(),
    }
}

/// Poll status on widening intervals until the job reaches a terminal state.
///
/// A two-hour recording can queue, so the deadline is generous; every state change
/// is surfaced so the wait is visibly progress rather than silence.
async fn poll(
    client: &reqwest::Client,
    job_id: &str,
    key: &str,
    progress: Progress<'_>,
) -> Result<serde_json::Value> {
    const FIRST: Duration = Duration::from_secs(5);
    const MAX: Duration = Duration::from_secs(15);
    const DEADLINE: Duration = Duration::from_secs(3 * 60 * 60);

    let started = std::time::Instant::now();
    let mut wait = FIRST;
    let mut last_state = String::new();

    loop {
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(MAX);

        let status = get_json(
            client,
            &format!("{BASE}/{job_id}/status"),
            key,
            "check the job status",
        )
        .await?;

        let state = status
            .get("job_state")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();
        if state != last_state {
            progress(&format!(
                "Job {state} ({} elapsed)",
                clock(started.elapsed().as_secs_f64())
            ));
            last_state = state.clone();
        }

        if JobState::parse(&state).is_terminal() {
            return Ok(status);
        }
        if started.elapsed() > DEADLINE {
            bail!(
                "Sarvam Batch job {job_id} was still {state} after {}. The job may finish later; the audio is kept.",
                clock(started.elapsed().as_secs_f64())
            );
        }
    }
}

fn output_file_names(status: &serde_json::Value) -> Vec<String> {
    status
        .get("job_details")
        .and_then(|v| v.as_array())
        .map(|details| {
            details
                .iter()
                .filter_map(|detail| detail.get("outputs")?.as_array())
                .flatten()
                .filter_map(|output| {
                    output
                        .get("file_name")
                        .or_else(|| output.get("name"))
                        .and_then(|v| v.as_str())
                        .map(|v| v.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Inputs whose task did not complete, so the caller can resubmit exactly those.
fn failed_input_names(status: &serde_json::Value) -> Vec<String> {
    status
        .get("job_details")
        .and_then(|v| v.as_array())
        .map(|details| {
            details
                .iter()
                .filter(|detail| {
                    let state = detail
                        .get("task_state")
                        .or_else(|| detail.get("state"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    let no_output = detail
                        .get("outputs")
                        .and_then(|v| v.as_array())
                        .map(|outputs| outputs.is_empty())
                        .unwrap_or(true);
                    JobState::parse(state) == JobState::Failed || no_output
                })
                .filter_map(|detail| {
                    detail
                        .get("inputs")
                        .and_then(|v| v.as_array())
                        .and_then(|inputs| inputs.first())
                        .and_then(|input| {
                            input
                                .get("file_name")
                                .or_else(|| input.get("name"))
                                .or(Some(input))
                                .and_then(|v| v.as_str())
                        })
                        .map(|v| v.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Transcribe `files`, retrying only the inputs that failed.
///
/// A failed job id can never be restarted, so a retry means a brand new job over the
/// failed inputs alone — resubmitting everything would pay twice for work that
/// already succeeded.
pub async fn submit_with_retry(
    files: &[PathBuf],
    key: &str,
    options: &BatchOptions,
    progress: Progress<'_>,
) -> Result<BatchResult> {
    let mut result = submit(files, key, options, progress).await?;
    if result.failed_inputs.is_empty() {
        return Ok(result);
    }

    let retry: Vec<PathBuf> = files
        .iter()
        .filter(|path| result.failed_inputs.contains(&upload_name(path)))
        .cloned()
        .collect();
    if retry.is_empty() {
        return Ok(result);
    }

    progress(&format!(
        "{} file(s) failed. Resubmitting only those in a new job",
        retry.len()
    ));

    match submit(&retry, key, options, progress).await {
        Ok(second) => {
            result.outputs.extend(second.outputs);
            result.failed_inputs = second.failed_inputs;
            result.elapsed += second.elapsed;
            result.partial = !result.failed_inputs.is_empty();
            Ok(result)
        }
        // The first pass still produced usable output; report the shortfall rather
        // than discarding it.
        Err(error) => {
            progress(&format!("Resubmission failed: {error}"));
            Ok(result)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip_through_their_names() {
        assert_eq!(Mode::from_name("transcribe"), Some(Mode::Transcribe));
        assert_eq!(Mode::from_name("Codemix"), Some(Mode::Codemix));
        assert_eq!(Mode::from_name("code-mix"), Some(Mode::Codemix));
        assert_eq!(Mode::from_name("translate"), None);
        assert_eq!(Mode::Codemix.to_string(), "codemix");
    }

    #[test]
    fn job_parameters_are_nested_and_omit_unknown_speaker_counts() {
        let body = job_parameters(&BatchOptions::default());
        let parameters = body.get("job_parameters").expect("nested object");
        assert_eq!(parameters["model"], "saaras:v3");
        assert_eq!(parameters["mode"], "transcribe");
        assert_eq!(parameters["language_code"], "unknown");
        assert_eq!(parameters["with_diarization"], true);
        // Guessing a speaker count would bias diarization.
        assert!(parameters.get("num_speakers").is_none());
        // The flat top-level copy the old TypeScript build also sent is gone.
        assert!(body.get("model").is_none());

        let body = job_parameters(&BatchOptions {
            num_speakers: Some(3),
            ..Default::default()
        });
        assert_eq!(body["job_parameters"]["num_speakers"], 3);
    }

    #[test]
    fn diarized_output_is_parsed_into_segments() {
        let value = serde_json::json!({
            "transcript": "hello there",
            "language_code": "gu-IN",
            "diarized_transcript": {
                "entries": [
                    {"transcript": "hello", "speaker_id": 0, "start_time_seconds": 1.0, "end_time_seconds": 2.0},
                    {"transcript": "there", "speaker_id": 1, "start_time_seconds": 2.5, "end_time_seconds": 4.0}
                ]
            }
        });

        let parsed = parse_output(&value);
        assert_eq!(parsed.text, "hello there");
        assert_eq!(parsed.language_code.as_deref(), Some("gu-IN"));
        assert_eq!(parsed.segments.len(), 2);
        assert_eq!(parsed.segments[0].speaker.as_deref(), Some("SPEAKER_00"));
        assert_eq!(parsed.segments[1].speaker.as_deref(), Some("SPEAKER_01"));
        assert_eq!(parsed.speaker_labels().len(), 2);
        assert_eq!(parsed.coverage(), Some((1.0, 4.0)));
        assert_eq!(parsed.overlapping_segments(), 0);
    }

    #[test]
    fn a_plain_transcript_without_diarization_still_parses() {
        let parsed = parse_output(&serde_json::json!({"text": "just words"}));
        assert_eq!(parsed.text, "just words");
        assert!(parsed.segments.is_empty());
        assert_eq!(parsed.as_labelled_text(), "just words");
        assert_eq!(parsed.coverage(), None);
    }

    #[test]
    fn an_unexpected_shape_yields_empty_data_rather_than_an_error() {
        // The output schema is undocumented, so a change upstream must not lose an
        // already-paid-for job.
        let parsed = parse_output(&serde_json::json!({"unexpected": {"nested": 1}}));
        assert!(parsed.text.is_empty());
        assert!(parsed.segments.is_empty());
        assert!(parsed.language_code.is_none());
    }

    #[test]
    fn entries_without_text_are_dropped_and_string_speakers_are_kept() {
        let value = serde_json::json!({
            "diarized_transcript": {
                "entries": [
                    {"transcript": "   ", "speaker_id": 0},
                    {"text": "kept", "speaker": "Speaker A", "start": 5.0, "end": 6.0},
                    {"transcript": "numeric string", "speaker_id": "2"}
                ]
            }
        });

        let parsed = parse_output(&value);
        assert_eq!(parsed.segments.len(), 2);
        assert_eq!(parsed.segments[0].speaker.as_deref(), Some("Speaker A"));
        assert_eq!(parsed.segments[0].start_seconds, Some(5.0));
        assert_eq!(parsed.segments[1].speaker.as_deref(), Some("SPEAKER_02"));
    }

    #[test]
    fn overlapping_segments_are_counted() {
        let value = serde_json::json!({
            "diarized_transcript": {"entries": [
                {"transcript": "a", "speaker_id": 0, "start_time_seconds": 0.0, "end_time_seconds": 5.0},
                {"transcript": "b", "speaker_id": 1, "start_time_seconds": 4.0, "end_time_seconds": 8.0},
                {"transcript": "c", "speaker_id": 0, "start_time_seconds": 9.0, "end_time_seconds": 10.0}
            ]}
        });
        assert_eq!(parse_output(&value).overlapping_segments(), 1);
    }

    #[test]
    fn labelled_text_carries_clock_stamps_and_generic_labels() {
        let value = serde_json::json!({
            "diarized_transcript": {"entries": [
                {"transcript": "opening", "speaker_id": 0, "start_time_seconds": 65.0}
            ]}
        });
        assert_eq!(
            parse_output(&value).as_labelled_text(),
            "[1:05] SPEAKER_00: opening"
        );
    }

    #[test]
    fn output_and_failed_names_are_read_from_job_details() {
        let status = serde_json::json!({
            "job_state": "PartiallyCompleted",
            "job_details": [
                {"inputs": [{"file_name": "audio-part01.wav"}], "outputs": [{"file_name": "0.json"}]},
                {"inputs": [{"file_name": "audio-part02.wav"}], "outputs": [], "task_state": "Failed"}
            ]
        });

        assert_eq!(output_file_names(&status), vec!["0.json".to_string()]);
        assert_eq!(
            failed_input_names(&status),
            vec!["audio-part02.wav".to_string()]
        );
    }

    #[test]
    fn merging_parts_shifts_later_timestamps_past_earlier_ones() {
        // Each part's offsets restart at zero, so without shifting, part two would
        // claim to happen during part one.
        let result = BatchResult {
            job_id: "j".into(),
            outputs: vec![
                Transcription {
                    text: "first".into(),
                    segments: vec![Segment {
                        speaker: Some("SPEAKER_00".into()),
                        text: "first".into(),
                        start_seconds: Some(0.0),
                        end_seconds: Some(100.0),
                    }],
                    language_code: Some("hi-IN".into()),
                },
                Transcription {
                    text: "second".into(),
                    segments: vec![Segment {
                        speaker: Some("SPEAKER_01".into()),
                        text: "second".into(),
                        start_seconds: Some(10.0),
                        end_seconds: Some(20.0),
                    }],
                    language_code: None,
                },
            ],
            failed_inputs: Vec::new(),
            elapsed: Duration::from_secs(1),
            partial: false,
        };

        let merged = result.merged();
        assert_eq!(merged.text, "first second");
        assert_eq!(merged.segments[1].start_seconds, Some(110.0));
        assert_eq!(merged.segments[1].end_seconds, Some(120.0));
        assert_eq!(merged.language_code.as_deref(), Some("hi-IN"));
    }

    #[test]
    fn upload_names_gain_an_extension_only_when_missing() {
        assert_eq!(upload_name(Path::new("/tmp/audio.wav")), "audio.wav");
        assert_eq!(upload_name(Path::new("/tmp/meeting.m4a")), "meeting.m4a");
        assert_eq!(upload_name(Path::new("/tmp/recording")), "recording.wav");
    }

    #[test]
    fn signed_urls_replay_metadata_headers() {
        let payload = serde_json::json!({
            "upload_urls": {
                "audio.wav": {
                    "file_url": "https://blob.example/put",
                    "file_metadata": {"x-ms-blob-type": "BlockBlob", "ignored": 5}
                }
            }
        });

        let details = signed_url(&payload, "upload_urls", "audio.wav").expect("url");
        assert_eq!(details.url, "https://blob.example/put");
        // Azure rejects a PUT without the headers the URL was signed with.
        assert_eq!(
            details.headers,
            vec![("x-ms-blob-type".to_string(), "BlockBlob".to_string())]
        );
    }

    #[test]
    fn azure_uploads_are_detected_from_the_container_type_or_the_host() {
        // Azure's Put Blob is 400 without x-ms-blob-type, and Sarvam's docs never
        // mention it because their examples use the Azure SDK.
        assert!(is_azure_blob(
            "https://x.blob.core.windows.net/c/f",
            Some("Azure")
        ));
        assert!(is_azure_blob(
            "https://x.blob.core.windows.net/c/f",
            Some("Azure_V1")
        ));
        // The field is missing from some responses, so the host is the fallback.
        assert!(is_azure_blob(
            "https://acct.blob.core.windows.net/c/f?sig=x",
            None
        ));

        // A URL that is explicitly not Azure must not get the header: a Google signed
        // URL rejects headers it was not signed with.
        assert!(!is_azure_blob(
            "https://storage.googleapis.com/c/f",
            Some("Google")
        ));
        assert!(!is_azure_blob("http://127.0.0.1/c/f", Some("Local")));
        assert!(!is_azure_blob("https://example.com/upload", None));
    }

    #[test]
    fn azure_error_bodies_are_reduced_to_the_sentence_that_matters() {
        // This is the body behind the bare "HTTP 400" that made the original failure
        // impossible to diagnose.
        let body = "<?xml version=\"1.0\"?><Error><Code>MissingRequiredHeader</Code>\
<Message>An HTTP header that's mandatory for this request is not specified.\nRequestId:abc\nTime:2026-07-26</Message></Error>";
        assert_eq!(
            azure_error_message(body),
            "MissingRequiredHeader: An HTTP header that's mandatory for this request is not specified."
        );

        assert_eq!(
            azure_error_message("<Error><Code>AuthenticationFailed</Code></Error>"),
            "AuthenticationFailed"
        );
        // Not XML: a short prefix, not a wall of HTML.
        let html = "<html>".to_string() + &"x".repeat(500);
        assert_eq!(azure_error_message(&html).len(), 200);
        assert!(azure_error_message("   ").is_empty());
    }

    #[test]
    fn a_single_returned_url_is_used_even_when_the_key_differs() {
        let payload = serde_json::json!({
            "download_urls": {"0.json": {"file_url": "https://blob.example/get"}}
        });
        assert!(signed_url(&payload, "download_urls", "unexpected.json").is_some());

        let two = serde_json::json!({
            "download_urls": {
                "0.json": {"file_url": "https://a"},
                "1.json": {"file_url": "https://b"}
            }
        });
        // With more than one, guessing would silently pair the wrong transcript.
        assert!(signed_url(&two, "download_urls", "2.json").is_none());
    }

    #[test]
    fn job_states_include_the_undocumented_partial_one() {
        assert_eq!(
            JobState::parse("PartiallyCompleted"),
            JobState::PartiallyCompleted
        );
        assert!(JobState::parse("PartiallyCompleted").is_terminal());
        assert!(JobState::parse("Completed").is_terminal());
        assert!(JobState::parse("Failed").is_terminal());
        assert!(!JobState::parse("Running").is_terminal());
        assert!(!JobState::parse("Accepted").is_terminal());
        assert_eq!(JobState::parse("something new"), JobState::Unknown);
    }

    #[test]
    fn error_messages_are_found_in_every_shape_sarvam_uses() {
        let status = reqwest::StatusCode::BAD_REQUEST;
        assert_eq!(
            error_message(
                &serde_json::json!({"error": {"message": "nested"}}),
                status,
                "x"
            ),
            "nested"
        );
        assert_eq!(
            error_message(&serde_json::json!({"message": "flat"}), status, "x"),
            "flat"
        );
        assert_eq!(
            error_message(&serde_json::json!({"detail": "detail"}), status, "x"),
            "detail"
        );
        assert_eq!(
            error_message(&serde_json::Value::Null, status, "create the job"),
            "create the job failed: HTTP 400"
        );
    }

    #[test]
    fn clock_formats_hours_only_when_needed() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(65.4), "1:05");
        assert_eq!(clock(3661.0), "1:01:01");
    }

    /// Stand in for Azure Blob: answer 201 only when `x-ms-blob-type` is present, and
    /// otherwise reply with Azure's own refusal. Serves exactly two requests.
    fn fake_azure() -> String {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            for stream in listener.incoming().take(2) {
                let Ok(mut stream) = stream else { continue };
                let mut raw = Vec::new();
                let mut buffer = [0u8; 8192];

                // Read the headers first.
                let header_end = loop {
                    if let Some(at) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                        break Some(at + 4);
                    }
                    match stream.read(&mut buffer) {
                        Ok(0) => break None,
                        Ok(n) => raw.extend_from_slice(&buffer[..n]),
                        Err(_) => break None,
                    }
                };
                let request = String::from_utf8_lossy(&raw).to_lowercase();

                // Then drain the body. Closing a socket with unread data queued makes
                // Windows send an RST, and the client sees "connection forcibly closed"
                // instead of the reply — which is a property of the harness, not of the
                // upload, and it made this test fail only on Windows.
                if let Some(header_end) = header_end {
                    let length = request
                        .split("\r\n")
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    let mut read = raw.len().saturating_sub(header_end);
                    while read < length {
                        match stream.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(n) => read += n,
                            Err(_) => break,
                        }
                    }
                }

                let response = if request.contains("x-ms-blob-type: blockblob") {
                    "HTTP/1.1 201 Created\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .to_string()
                } else {
                    let body = "<?xml version=\"1.0\" encoding=\"utf-8\"?><Error>\
<Code>MissingRequiredHeader</Code><Message>An HTTP header that's mandatory for this \
request is not specified.\nRequestId:test\nTime:2026-07-26</Message></Error>";
                    format!(
                        "HTTP/1.1 400 Bad Request\r\ncontent-type: application/xml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                // Half-close so the client reads the reply before the socket goes away.
                let _ = stream.shutdown(std::net::Shutdown::Write);
            }
        });

        format!("http://{address}/container/audio.wav?sig=x")
    }

    #[tokio::test]
    async fn the_blob_type_header_is_what_makes_an_azure_upload_succeed() {
        // The regression this pins: without the header every long meeting died on
        // "Sarvam signed upload failed: HTTP 400", with nothing to say why.
        let dir = std::env::temp_dir().join(format!("sarvam-upload-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("audio.wav");
        std::fs::write(&file, vec![7u8; 4096]).unwrap();

        let url = fake_azure();
        let client = super::client().unwrap();
        let details = SignedUrl {
            url,
            headers: Vec::new(),
        };

        // Recognised as Azure: the header is sent and the upload is accepted.
        put_file(&client, &details, &file, Some("Azure"))
            .await
            .expect("upload with x-ms-blob-type should be accepted");

        // Declared as something else: no header, and the refusal now explains itself.
        let error = put_file(&client, &details, &file, Some("Google"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("HTTP 400"), "{error}");
        assert!(error.contains("MissingRequiredHeader"), "{error}");
        assert!(error.contains("mandatory"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_job_over_the_documented_file_limit_is_refused_before_upload() {
        let files: Vec<PathBuf> = (0..21)
            .map(|i| PathBuf::from(format!("f{i}.wav")))
            .collect();
        let error = submit(&files, "key", &BatchOptions::default(), &|_| {})
            .await
            .unwrap_err();
        assert!(error.to_string().contains("20 files per job"), "{error}");

        let error = submit(&[], "key", &BatchOptions::default(), &|_| {})
            .await
            .unwrap_err();
        assert!(error.to_string().contains("No audio files"), "{error}");
    }
}
