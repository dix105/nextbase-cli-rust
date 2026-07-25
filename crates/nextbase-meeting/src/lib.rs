//! Meeting Agent: record a meeting, transcribe it with Sarvam Batch, summarise it.
//!
//! The second Nextbase tool. It shares `~/.wisper-cli/config.json` with Wisper — so a
//! Sarvam key saved by `wisper setup` already works here — but keeps its own
//! artifacts in `~/.nextbase/meetings/<id>/`, because those are files a person opens.
//!
//! Flow:
//!
//! ```text
//! start ──> [detached recorder] ──> stop ──> sample gate ──> full run ──> summary
//! ```
//!
//! The gate between recording and the full run is the point of the whole design: a
//! readable transcript is not necessarily a correct one, and a bad transcription mode
//! produces confident-sounding notes that corrupt names, numbers and commitments. A
//! short sample is transcribed both ways first, and the full run only happens once
//! someone has looked.

pub mod deliverables;
pub mod pipeline;
pub mod recorder;
pub mod state;
pub mod summary;

use anyhow::{bail, Result};
use nextbase_core::config::{self, Provider};

/// Confirm a meeting can actually be transcribed before recording one.
///
/// Discovering a missing key *after* an hour-long meeting is the worst possible time,
/// so `start` checks first.
pub fn check_ready() -> Result<()> {
    let settings = config::load();

    let sarvam = settings
        .key_for(Provider::Sarvam)
        .filter(|key| !key.is_empty());
    if sarvam.is_none() {
        bail!("Meeting transcription needs a Sarvam API key (Batch STT with speaker labels). Run: nbmeet setup");
    }

    // Summaries go through Groq; without it a meeting still transcribes, so this is a
    // warning at setup time rather than a hard requirement here.
    Ok(())
}

pub fn has_summary_key() -> bool {
    config::load()
        .key_for(Provider::Groq)
        .map(|key| !key.is_empty())
        .unwrap_or(false)
}

/// A fresh meeting id: sortable, readable, and safe as a directory name.
pub fn new_meeting_id() -> String {
    format!("meeting-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meeting_ids_sort_chronologically_and_are_path_safe() {
        let id = new_meeting_id();
        assert!(id.starts_with("meeting-"));
        assert_eq!(id.len(), "meeting-20260725-181500".len());
        assert!(
            id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{id}"
        );
    }
}
