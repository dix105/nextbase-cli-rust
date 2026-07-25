//! The detached recorder worker: `nbmeet _record <id>`.
//!
//! It owns the capture streams and nothing else. Its only job is to record until the
//! state file says `Stopping`, finalize the WAV, and mark the meeting `Recorded` —
//! transcription happens in the foreground process afterwards, so a long upload never
//! depends on this worker surviving.

use anyhow::{bail, Result};
use nextbase_core::capture::{self, CaptureOptions};
use nextbase_core::{config, log};
use std::time::Duration;

use crate::state::{self, ActiveMeeting, Phase, SourceLevel};

/// How often the worker checks whether a stop has been requested.
const POLL: Duration = Duration::from_millis(250);
/// A meeting this long is almost certainly one someone forgot to stop.
const MAX_DURATION: Duration = Duration::from_secs(6 * 60 * 60);

pub fn audio_path(id: &str) -> std::path::PathBuf {
    nextbase_core::paths::meeting_dir(id).join("audio.wav")
}

/// Record until stopped. Runs in the detached worker process.
pub fn run(id: &str) -> Result<()> {
    let Some(active) = state::load() else {
        bail!("No active meeting to record.");
    };
    // A stale worker from a previous meeting must not overwrite the current one's
    // audio, so the id has to match exactly.
    if active.id != id {
        bail!(
            "Meeting {id} is no longer the active meeting ({} is).",
            active.id
        );
    }
    if active.phase != Phase::Starting {
        bail!("Meeting {id} is {} , not starting.", active.phase);
    }

    let settings = config::load();
    let (mic, system) = settings.meeting_capture();
    let options = CaptureOptions {
        mic,
        system,
        device: settings.audio_device.clone(),
        keep_tracks: settings.meeting_keep_tracks == Some(true),
    };

    let path = audio_path(id);
    log::log(&format!("Meeting {id}: opening capture sources"));

    let recording = match capture::start(&options, path.clone()) {
        Ok(recording) => recording,
        Err(error) => {
            // Record why, so `nbmeet status` can explain it instead of the meeting
            // just never starting.
            let _ = state::update(|meeting| {
                meeting.phase = Phase::Failed;
                meeting.error = Some(error.to_string());
            });
            return Err(error);
        }
    };

    state::update(|meeting| {
        meeting.phase = Phase::Recording;
        meeting.audio_path = Some(path.clone());
        meeting.recorder_pid = Some(std::process::id());
    })?;
    log::log(&format!("Meeting {id}: recording to {}", path.display()));

    let mut stop_reason = "stop requested";
    loop {
        std::thread::sleep(POLL);

        match state::load() {
            // Someone started a different meeting, or the state was cleared: stop
            // rather than keep the microphone open forever.
            None => {
                stop_reason = "active meeting file disappeared";
                break;
            }
            Some(meeting) if meeting.id != id => {
                stop_reason = "another meeting became active";
                break;
            }
            Some(meeting) if meeting.phase == Phase::Stopping => break,
            Some(meeting) if !meeting.phase.is_capturing() => {
                stop_reason = "meeting left the recording phase";
                break;
            }
            Some(_) => {}
        }

        if recording.elapsed() > MAX_DURATION {
            stop_reason = "reached the 6 hour limit";
            break;
        }
    }

    log::log(&format!("Meeting {id}: stopping — {stop_reason}"));
    let finished = recording.stop()?;

    let levels: Vec<SourceLevel> = finished
        .per_source
        .iter()
        .map(|(kind, levels)| SourceLevel {
            source: kind.label().to_string(),
            peak: levels.peak,
            rms: levels.rms,
            silent: levels.is_silent(),
        })
        .collect();

    for level in &levels {
        if level.silent {
            // The worst failure this tool has: the recording looks fine and the far
            // side is simply absent from the transcript.
            log::log(&format!(
                "Meeting {id}: {} was silent for the whole recording",
                level.source
            ));
        }
    }

    state::update(|meeting| {
        meeting.phase = Phase::Recorded;
        meeting.audio_path = Some(finished.path.clone());
        meeting.duration_seconds = Some(finished.duration.as_secs_f64());
        meeting.source_levels = levels;
        meeting.recorder_pid = None;
    })?;

    log::log(&format!(
        "Meeting {id}: recorded {:.1}s to {}",
        finished.duration.as_secs_f64(),
        finished.path.display()
    ));
    Ok(())
}

/// Ask the recorder to stop, then wait for it to finalize the file.
///
/// Returns the meeting once it reaches `Recorded`. The wait matters: the WAV header is
/// only correct after the recorder finalizes it, so transcribing before that would
/// read a truncated file.
pub fn request_stop(timeout: Duration) -> Result<ActiveMeeting> {
    let Some(active) = state::load() else {
        bail!("No active meeting. Start one with: nbmeet start");
    };

    if active.phase == Phase::Recorded {
        return Ok(active);
    }
    if !active.phase.is_capturing() {
        bail!(
            "Meeting {} is {}, so there is nothing to stop.",
            active.id,
            active.phase
        );
    }

    state::update(|meeting| meeting.phase = Phase::Stopping)?;

    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(POLL);
        match state::load() {
            Some(meeting) if meeting.phase == Phase::Recorded => return Ok(meeting),
            Some(meeting) if meeting.phase == Phase::Failed => {
                bail!(
                    "{}",
                    meeting
                        .error
                        .unwrap_or_else(|| "The recorder failed.".to_string())
                );
            }
            _ => {}
        }
    }

    // Do not silently continue: the audio file's header is unfinalized, and only the
    // recorder can fix that. Say what state things are in.
    bail!(
        "The recorder did not finish within {}s. The audio is at {} and is readable up to its last checkpoint. Check: nbmeet status",
        timeout.as_secs(),
        audio_path(&active.id).display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_lands_beside_its_deliverables() {
        let path = audio_path("meeting-123");
        assert!(path.ends_with("meetings/meeting-123/audio.wav"), "{path:?}");
    }
}
