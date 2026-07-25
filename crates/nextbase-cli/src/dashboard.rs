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
    let Some(active) = meeting_state::load() else {
        return Json(json!({"active": false, "unfinished": unfinished_count()}));
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
        "error": active.error,
        "unfinished": unfinished_count(),
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

fn ok(body: serde_json::Value) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::OK, Json(body))
}

fn problem(status: StatusCode, message: &str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(json!({"error": message})))
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
        .route("/api/meeting/approve/{mode}", post(approve_meeting))
        .route("/api/meeting/reject", post(reject_meeting));

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
