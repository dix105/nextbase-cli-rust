//! Local web dashboard.
//!
//! The page is embedded in the binary, so there is no asset directory to keep next
//! to the executable — the TypeScript build had to ship `web/` alongside `dist/`.

use anyhow::{Context, Result};
use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use nextbase_core::config::Provider;
use nextbase_core::sarvam_batch::Mode;
use nextbase_core::{autostart, config, paths, process_state, storage, wav};
use nextbase_meeting::state::{self as meeting_state, Phase};
use serde_json::json;

const PAGE: &str = include_str!("../web/index.html");

async fn index() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-store")], Html(PAGE))
}

async fn state() -> impl IntoResponse {
    let config = config::load();

    Json(json!({
        "setup": {
            "provider": config.provider.map(|p| p.to_string()),
            "model": config.model,
            "shortcut": config.shortcut_or_default(),
            "polishShortcut": config.polish_shortcut_or_default(),
            "spellShortcut": config.spell_shortcut_or_default(),
            "autoPolish": config.auto_polish.unwrap_or(false),
        },
        "models": config::MODEL_OPTIONS.iter().map(|option| json!({
            "label": option.label,
            "model": option.model,
            "provider": option.provider.as_str(),
            "hasKey": config.key_for(option.provider).map(|key| !key.is_empty()).unwrap_or(false),
        })).collect::<Vec<_>>(),
        "listeners": process_state::other_listener_pids().len(),
        "history": storage::load_history(),
    }))
}

async fn delete_transcript(Path(id): Path<String>) -> impl IntoResponse {
    match storage::delete_transcript(&id) {
        Ok(true) => StatusCode::NO_CONTENT,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

// ------------------------------------------------------------------ meeting

/// The active meeting, plus enough measured detail for the approval pane.
async fn meeting() -> impl IntoResponse {
    let settings = config::load();
    let setup = json!({
        "model": settings.meeting_model_or_default(),
        "summaryModel": settings.meeting_summary_model_or_default(),
        "supportsMode": settings.meeting_model_supports_mode(),
        "models": config::MEETING_MODEL_OPTIONS.iter().map(|option| json!({
            "label": option.label,
            "model": option.model,
            "supportsMode": option.supports_mode,
        })).collect::<Vec<_>>(),
        "summaryModels": config::SUMMARY_MODEL_OPTIONS,
        "modes": Mode::ALL.iter().map(|mode| json!({
            "mode": mode.as_str(),
            "describe": mode.describe(),
            "compared": mode.is_compared(),
        })).collect::<Vec<_>>(),
        "gate": settings.meeting_gate_enabled(),
        "mode": settings.meeting_mode,
        "hasKey": settings.key_for(Provider::Sarvam).map(|key| !key.is_empty()).unwrap_or(false),
    });

    let Some(active) = meeting_state::load() else {
        return Json(json!({"active": false, "unfinished": unfinished_count(), "setup": setup}));
    };

    // Read the header rather than the recorder's intent: this is what is safely on
    // disk, which is the honest number to show while recording.
    let on_disk = active
        .audio_path
        .as_ref()
        .and_then(|path| wav::info(path).ok())
        .map(|info| info.duration_seconds());

    Json(json!({
        "active": true,
        "id": active.id,
        "phase": active.phase.as_str(),
        "startedAt": active.started_at,
        "elapsedSeconds": active.elapsed_seconds(),
        "recordedSeconds": active.duration_seconds,
        "onDiskSeconds": on_disk,
        "capturing": active.phase.is_capturing(),
        "processing": active.phase.is_processing(),
        "sourceLevels": active.source_levels,
        "sample": active.sample,
        "approvedMode": active.approved_mode,
        "progress": active.progress,
        "progressAgeSeconds": meeting_state::progress_age_seconds(&active),
        "imported": active.imported,
        "gateBlocked": active.gate_blocked,
        "error": active.error,
        "unfinished": unfinished_count(),
        "setup": setup,
    }))
}

/// Past meetings, newest first, with what each produced.
async fn meeting_history() -> impl IntoResponse {
    let Ok(entries) = std::fs::read_dir(paths::meetings_dir()) else {
        return Json(json!([]));
    };

    let mut directories: Vec<std::path::PathBuf> =
        entries.flatten().map(|entry| entry.path()).collect();
    directories.sort();
    directories.reverse();

    let meetings: Vec<serde_json::Value> = directories
        .into_iter()
        .take(50)
        .map(|directory| {
            let id = directory
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            let note = directory.join("meeting-note.md");
            let title = std::fs::read_to_string(&note).ok().and_then(|body| {
                body.lines()
                    .next()
                    .map(|line| line.trim_start_matches('#').trim().to_string())
            });
            let audio = nextbase_meeting::pipeline::recorded_audio(&directory);

            json!({
                "id": id,
                "title": title,
                "transcribed": note.is_file(),
                "directory": directory.display().to_string(),
                "seconds": audio.as_ref().and_then(|path| wav::info(path).ok())
                    .map(|info| info.duration_seconds()),
            })
        })
        .collect();

    Json(json!(meetings))
}

fn unfinished_count() -> usize {
    nextbase_meeting::pipeline::resumable().len()
}

async fn start_meeting() -> impl IntoResponse {
    if let Err(error) = nextbase_meeting::check_ready() {
        return problem(StatusCode::BAD_REQUEST, &error.to_string());
    }
    if let Some(active) = meeting_state::load() {
        if active.phase.is_capturing() {
            return problem(
                StatusCode::CONFLICT,
                &format!("Meeting {} is already {}.", active.id, active.phase),
            );
        }
    }
    // Consent is a decision for a person, and the browser is not where it was given.
    if config::load().meeting_consent != Some(true) {
        return problem(
            StatusCode::BAD_REQUEST,
            "Run `nbmeet start` in a terminal once to confirm that meeting audio is uploaded for transcription.",
        );
    }

    let id = nextbase_meeting::new_meeting_id();
    if let Err(error) = std::fs::create_dir_all(paths::meeting_dir(&id))
        .map_err(anyhow::Error::from)
        .and_then(|_| meeting_state::save(&meeting_state::ActiveMeeting::new(&id)))
    {
        return problem(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
    }

    match autostart::spawn_sibling_detached("nbmeet", &["_record", &id]) {
        Ok(pid) => ok(json!({"id": id, "pid": pid})),
        Err(error) => problem(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

/// Stop the recorder and hand the meeting to the CLI to transcribe.
///
/// Transcription is deliberately *not* run inside the request: a Batch job can queue
/// for minutes, and an HTTP handler is the wrong place to hold that. The button stops
/// the recording; a detached `nbmeet process` takes it from there, so closing the tab
/// cannot abandon a job.
async fn stop_meeting() -> impl IntoResponse {
    let stopped = nextbase_meeting::recorder::request_stop(std::time::Duration::from_secs(30));
    match stopped {
        Ok(active) => {
            let handed_off = autostart::spawn_sibling_detached("nbmeet", &["process"]).is_ok();
            ok(json!({
                "id": active.id,
                "recordedSeconds": active.duration_seconds,
                "processing": handed_off,
            }))
        }
        Err(error) => problem(StatusCode::BAD_REQUEST, &error.to_string()),
    }
}

/// Import a local file or a remote URL and transcribe it.
///
/// The work is handed to a detached `nbmeet audio` for the same reason as stop: a
/// download plus a queued Batch job is far too long to hold an HTTP request open.
async fn import_audio(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    let Some(source) = body
        .get("source")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|source| !source.is_empty())
    else {
        return problem(
            StatusCode::BAD_REQUEST,
            "Give a file path or an http(s) URL.",
        );
    };

    if let Err(error) = nextbase_meeting::check_ready() {
        return problem(StatusCode::BAD_REQUEST, &error.to_string());
    }
    if let Some(active) = meeting_state::load() {
        if active.phase.is_capturing() || active.phase.is_processing() {
            return problem(
                StatusCode::CONFLICT,
                &format!("Meeting {} is {}.", active.id, active.phase),
            );
        }
    }
    if config::load().meeting_consent != Some(true) {
        return problem(
            StatusCode::BAD_REQUEST,
            "Run `nbmeet start` or `nbmeet audio` in a terminal once to confirm that audio is uploaded for transcription.",
        );
    }
    // A local path is resolved by the spawned process, which shares this machine — but
    // reject a path that does not exist now, so the error arrives in the browser rather
    // than only in the log.
    if !nextbase_core::import::is_remote(source) && !std::path::Path::new(source).is_file() {
        return problem(StatusCode::BAD_REQUEST, &format!("No file at {source}"));
    }

    match autostart::spawn_sibling_detached("nbmeet", &["audio", source]) {
        Ok(_) => ok(json!({"source": source})),
        Err(error) => problem(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

/// Receive an uploaded audio file and transcribe it.
///
/// A browser cannot give a page the path of a chosen file — only its contents — so the
/// bytes are written next to the other meeting data and handed to `nbmeet audio`, which
/// is the same path a typed path or URL takes.
async fn upload_audio(
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    if let Err(error) = nextbase_meeting::check_ready() {
        return problem(StatusCode::BAD_REQUEST, &error.to_string());
    }
    if let Some(active) = meeting_state::load() {
        if active.phase.is_capturing() || active.phase.is_processing() {
            return problem(
                StatusCode::CONFLICT,
                &format!("Meeting {} is {}.", active.id, active.phase),
            );
        }
    }
    if config::load().meeting_consent != Some(true) {
        return problem(
            StatusCode::BAD_REQUEST,
            "Run `nbmeet start` or `nbmeet audio` in a terminal once to confirm that audio is uploaded for transcription.",
        );
    }
    if body.len() < 1024 {
        return problem(
            StatusCode::BAD_REQUEST,
            &format!(
                "That file is only {} bytes, which is not audio.",
                body.len()
            ),
        );
    }

    let uploads = paths::nextbase_dir().join("uploads");
    // Yesterday's uploads are already copied into their meeting directories; this is
    // the only place they get tidied, since the import copies rather than moves.
    clean_old_uploads(&uploads);
    if let Err(error) = std::fs::create_dir_all(&uploads) {
        return problem(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
    }

    let name = safe_file_name(&query_name(query.as_deref()).unwrap_or_else(|| "upload.wav".into()));
    // The meeting id is minted by `nbmeet audio`; this only needs to be unique.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let path = uploads.join(format!("{stamp}-{name}"));
    if let Err(error) = std::fs::write(&path, &body) {
        return problem(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
    }

    let source = path.to_string_lossy().to_string();
    match autostart::spawn_sibling_detached("nbmeet", &["audio", &source]) {
        Ok(_) => ok(json!({"name": name, "bytes": body.len()})),
        Err(error) => problem(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

/// Pull `name=` out of the raw query string.
///
/// Hand-parsed rather than pulling serde into this crate for one optional field; the
/// value is percent-decoded only as far as `%20`, since `safe_file_name` throws away
/// everything unusual anyway.
fn query_name(query: Option<&str>) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "name").then(|| value.replace("%20", " ").replace('+', " "))
    })
}

/// Keep only the basename, and only characters that are safe in one.
///
/// The name arrives from a browser; a `../` in it would otherwise choose where the file
/// lands.
fn safe_file_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(['.', '-']).to_string();
    if cleaned.is_empty() {
        "upload.wav".to_string()
    } else {
        cleaned
    }
}

fn clean_old_uploads(directory: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let cutoff = std::time::Duration::from_secs(6 * 60 * 60);
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(|modified| modified.elapsed().map(|age| age > cutoff).unwrap_or(false))
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

async fn approve_meeting(Path(mode): Path<String>) -> impl IntoResponse {
    let Some(active) = meeting_state::load() else {
        return problem(StatusCode::NOT_FOUND, "No meeting is waiting for approval.");
    };
    if active.phase != Phase::AwaitingApproval {
        return problem(
            StatusCode::CONFLICT,
            &format!("Meeting {} is {}.", active.id, active.phase),
        );
    }
    let Some(mode) = Mode::from_name(&mode) else {
        return problem(
            StatusCode::BAD_REQUEST,
            "Unknown mode. Use transcribe or codemix.",
        );
    };

    // Same reasoning as stop: the full run is detached so the tab is not holding it.
    match autostart::spawn_sibling_detached("nbmeet", &["approve", mode.as_str()]) {
        Ok(_) => ok(json!({"mode": mode.as_str()})),
        Err(error) => problem(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn reject_meeting() -> impl IntoResponse {
    let Some(active) = meeting_state::load() else {
        return problem(StatusCode::NOT_FOUND, "No meeting is waiting for approval.");
    };
    if active.phase != Phase::AwaitingApproval {
        return problem(
            StatusCode::CONFLICT,
            &format!("Meeting {} is {}.", active.id, active.phase),
        );
    }

    match meeting_state::update(|meeting| {
        meeting.phase = Phase::Recorded;
        meeting.sample = None;
    }) {
        Ok(_) => ok(json!({"rejected": true})),
        Err(error) => problem(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

/// Change which model a tool uses, from the browser.
///
/// The two tools are kept apart here exactly as they are in the CLI: this only ever
/// writes the meeting fields, and `/api/wisper/model` only ever writes Wisper's.
async fn set_meeting_model(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    let model = body.get("model").and_then(|value| value.as_str());
    let summary = body.get("summaryModel").and_then(|value| value.as_str());
    if model.is_none() && summary.is_none() && body.get("mode").is_none() {
        return problem(StatusCode::BAD_REQUEST, "Nothing to change.");
    }

    if let Some(model) = model {
        if !config::MEETING_MODEL_OPTIONS
            .iter()
            .any(|option| option.model == model)
        {
            return problem(StatusCode::BAD_REQUEST, &format!("Unknown model {model}."));
        }
    }
    if let Some(summary) = summary {
        if !config::SUMMARY_MODEL_OPTIONS.contains(&summary) {
            return problem(
                StatusCode::BAD_REQUEST,
                &format!("Unknown summary model {summary}."),
            );
        }
    }

    let mode = body.get("mode").and_then(|value| value.as_str());
    if let Some(mode) = mode {
        if Mode::from_name(mode).is_none() {
            return problem(StatusCode::BAD_REQUEST, &format!("Unknown mode {mode}."));
        }
    }

    let model = model.map(str::to_string);
    let summary = summary.map(str::to_string);
    let mode = mode.map(str::to_string);
    match config::update(|c| {
        if let Some(model) = model {
            c.meeting_model = Some(model);
        }
        if let Some(summary) = summary {
            c.meeting_summary_model = Some(summary);
        }
        if let Some(mode) = mode {
            c.meeting_mode = Some(mode);
        }
    }) {
        Ok(_) => {
            let settings = config::load();
            ok(json!({
                "model": settings.meeting_model_or_default(),
                "summaryModel": settings.meeting_summary_model_or_default(),
                "supportsMode": settings.meeting_model_supports_mode(),
            }))
        }
        Err(error) => problem(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

/// Change Wisper's dictation model. Provider and model move together, since a provider
/// with another provider's model configured fails on every dictation.
async fn set_wisper_model(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    let Some(wanted) = body.get("model").and_then(|value| value.as_str()) else {
        return problem(StatusCode::BAD_REQUEST, "Name a model.");
    };
    let Some(option) = config::MODEL_OPTIONS
        .iter()
        .find(|option| option.model == wanted)
    else {
        return problem(StatusCode::BAD_REQUEST, &format!("Unknown model {wanted}."));
    };

    // Without a key the change would only surface as a failure at the next dictation.
    let has_key = config::load()
        .key_for(option.provider)
        .map(|key| !key.is_empty())
        .unwrap_or(false);
    if !has_key {
        return problem(
            StatusCode::BAD_REQUEST,
            &format!(
                "No {} key is saved. Add one first: wisper model {}",
                option.provider, option.model
            ),
        );
    }

    let provider = option.provider;
    let model = option.model.to_string();
    match config::update(|c| {
        c.provider = Some(provider);
        c.model = Some(model.clone());
    }) {
        Ok(_) => ok(json!({
            "provider": provider.as_str(),
            "model": option.model,
            // The listener reads config once, at startup.
            "restartRequired": true,
        })),
        Err(error) => problem(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

fn ok(body: serde_json::Value) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::OK, Json(body))
}

fn problem(status: StatusCode, message: &str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(json!({"error": message})))
}

/// Whether our dashboard is already answering on `port`.
///
/// Checked by asking for a known endpoint rather than just probing the socket: something
/// unrelated on that port must not be mistaken for the dashboard and opened in a browser.
pub async fn already_serving(port: u16) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Raw TCP rather than an HTTP client: this crate has no need of one otherwise, and
    // the check is a single request.
    let probe = async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .ok()?;
        stream
            .write_all(
                format!("GET /api/meeting HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .ok()?;

        let mut response = Vec::new();
        // Enough for the status line and the start of the body; the marker is small.
        let mut buffer = [0u8; 2048];
        while response.len() < 4096 {
            match stream.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => response.extend_from_slice(&buffer[..n]),
            }
        }
        Some(String::from_utf8_lossy(&response).to_string())
    };

    // Identified by a field only this endpoint returns, so an unrelated service on the
    // port is never mistaken for the dashboard and opened in a browser.
    match tokio::time::timeout(std::time::Duration::from_millis(700), probe).await {
        Ok(Some(body)) => body.contains("200 OK") && body.contains("\"active\""),
        _ => false,
    }
}

pub async fn serve(port: u16) -> Result<String> {
    let router = Router::new()
        .route("/", get(index))
        .route("/api/state", get(state))
        .route("/api/history/{id}", delete(delete_transcript))
        .route("/api/meeting", get(meeting))
        .route("/api/meeting/history", get(meeting_history))
        .route("/api/meeting/start", post(start_meeting))
        .route("/api/meeting/stop", post(stop_meeting))
        .route("/api/meeting/audio", post(import_audio))
        .route(
            "/api/meeting/upload",
            // Audio files are far larger than axum's 2 MB default. Raised only for this
            // route, and the server is loopback-only.
            post(upload_audio).layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024 * 1024)),
        )
        .route("/api/meeting/approve/{mode}", post(approve_meeting))
        .route("/api/meeting/reject", post(reject_meeting))
        .route("/api/meeting/model", post(set_meeting_model))
        .route("/api/wisper/model", post(set_wisper_model));

    // Loopback only. This exposes transcript history, so it must never bind 0.0.0.0.
    let address = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .with_context(|| format!("Could not bind {address}. Is the dashboard already running?"))?;

    let url = format!("http://{address}");
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            eprintln!("Dashboard stopped: {error}");
        }
    });
    Ok(url)
}

/// Best-effort: opening a browser is a convenience, not a requirement.
pub fn open_in_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
