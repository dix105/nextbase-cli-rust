//! Local web dashboard.
//!
//! The page is embedded in the binary, so there is no asset directory to keep next
//! to the executable — the TypeScript build had to ship `web/` alongside `dist/`.

use anyhow::{Context, Result};
use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::{delete, get};
use axum::{Json, Router};
use nextbase_core::{config, process_state, storage};
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

pub async fn serve(port: u16) -> Result<String> {
    let router = Router::new()
        .route("/", get(index))
        .route("/api/state", get(state))
        .route("/api/history/{id}", delete(delete_transcript));

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
