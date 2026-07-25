use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

const HISTORY_LIMIT: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transcript {
    pub id: String,
    pub text: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

pub fn load_history() -> Vec<Transcript> {
    let Ok(raw) = std::fs::read_to_string(paths::history_file()) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn write_history(history: &[Transcript]) -> Result<()> {
    let dir = paths::wisper_dir();
    std::fs::create_dir_all(&dir)?;
    let body = serde_json::to_string_pretty(history)?;
    let target = paths::history_file();
    let temp = dir.join(format!("history.json.{}.tmp", std::process::id()));
    std::fs::write(&temp, body.as_bytes())?;
    std::fs::rename(&temp, &target)
        .with_context(|| format!("Could not write {}", target.display()))?;
    Ok(())
}

/// Remove one transcript. `false` means it was already gone.
pub fn delete_transcript(id: &str) -> Result<bool> {
    let mut history = load_history();
    let before = history.len();
    history.retain(|item| item.id != id);
    if history.len() == before {
        return Ok(false);
    }
    write_history(&history)?;
    Ok(true)
}

/// Newest first, capped at 500 entries — same shape the web dashboard reads.
pub fn save_transcript(text: &str, source: &str) -> Result<Transcript> {
    let item = Transcript {
        id: uuid::Uuid::new_v4().to_string(),
        text: text.to_string(),
        created_at: crate::log::timestamp(),
        source: Some(source.to_string()),
    };

    let mut history = load_history();
    history.insert(0, item.clone());
    history.truncate(HISTORY_LIMIT);
    write_history(&history)?;
    Ok(item)
}
