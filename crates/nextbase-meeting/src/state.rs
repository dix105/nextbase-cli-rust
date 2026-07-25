//! The active-meeting state file, and the state machine it carries.
//!
//! **Stop is a state transition, not a signal.** The recorder polls this file and
//! finalizes its own WAV when it sees `Stopping`. That is deliberate: on Windows
//! `process_state` terminates with `TerminateProcess`, which gives a process no
//! chance to run cleanup, so a signal-based stop would leave every Windows recording
//! with an unfinalized header. Polling a file behaves identically on both platforms.
//!
//! The file is also the crash-recovery record. A meeting that was recorded but never
//! transcribed stays on disk as `Recorded`, so `nbmeet process <id>` can pick it up
//! rather than the audio being orphaned.

use anyhow::{Context, Result};
use nextbase_core::paths;
use nextbase_core::sarvam_batch::Mode;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Where a meeting is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// The worker has been spawned but has not opened the devices yet.
    Starting,
    Recording,
    /// Stop requested. The recorder is finalizing the file.
    Stopping,
    /// Audio is on disk and complete. Safe to transcribe, and safe to resume from.
    Recorded,
    /// Transcribing the sample for the quality gate.
    Sampling,
    /// Waiting for the user to pick a mode, or reject.
    AwaitingApproval,
    Transcribing,
    Summarising,
    Done,
    Failed,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::Recording => "recording",
            Phase::Stopping => "stopping",
            Phase::Recorded => "recorded",
            Phase::Sampling => "sampling",
            Phase::AwaitingApproval => "awaiting-approval",
            Phase::Transcribing => "transcribing",
            Phase::Summarising => "summarising",
            Phase::Done => "done",
            Phase::Failed => "failed",
        }
    }

    /// Nothing further will happen on its own.
    pub fn is_finished(&self) -> bool {
        matches!(self, Phase::Done | Phase::Failed)
    }

    /// The microphone is open, so a second meeting must not start.
    pub fn is_capturing(&self) -> bool {
        matches!(self, Phase::Starting | Phase::Recording | Phase::Stopping)
    }

    /// Work is in flight that a `stop` should not interrupt.
    pub fn is_processing(&self) -> bool {
        matches!(
            self,
            Phase::Sampling | Phase::Transcribing | Phase::Summarising
        )
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One transcription of the sample, for the quality gate to display.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SampleCandidate {
    pub mode: Mode,
    pub text: String,
    /// Measured, not estimated: how long the job actually took.
    pub elapsed_seconds: f64,
    pub segment_count: usize,
    pub speaker_labels: usize,
    pub overlapping_segments: usize,
    /// Provider language *detection*. Never an accuracy figure.
    pub detected_language: Option<String>,
    /// First and last timestamp, against the sample's own duration.
    pub covered_seconds: Option<f64>,
    pub sample_seconds: f64,
    pub error: Option<String>,
}

/// The quality gate's findings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SampleReport {
    pub window_start_seconds: f64,
    pub window_seconds: f64,
    /// RMS of the chosen window, so a near-silent sample is visible immediately.
    pub window_rms: f32,
    pub candidates: Vec<SampleCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveMeeting {
    pub id: String,
    pub phase: Phase,
    /// RFC3339, so the file is readable and comparable without parsing tricks.
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorder_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<SampleReport>,
    /// Mode approved for the full run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_mode: Option<Mode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Per-source levels once recording has finished, so a source that stayed silent
    /// for the whole meeting is on the record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_levels: Vec<SourceLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceLevel {
    pub source: String,
    pub peak: f32,
    pub rms: f32,
    pub silent: bool,
}

impl ActiveMeeting {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            phase: Phase::Starting,
            started_at: chrono::Utc::now().to_rfc3339(),
            audio_path: None,
            recorder_pid: None,
            duration_seconds: None,
            sample: None,
            approved_mode: None,
            note: None,
            error: None,
            source_levels: Vec::new(),
        }
    }

    pub fn directory(&self) -> PathBuf {
        paths::meeting_dir(&self.id)
    }

    pub fn elapsed_seconds(&self) -> f64 {
        chrono::DateTime::parse_from_rfc3339(&self.started_at)
            .map(|started| {
                (chrono::Utc::now() - started.with_timezone(&chrono::Utc))
                    .num_milliseconds()
                    .max(0) as f64
                    / 1000.0
            })
            .unwrap_or(0.0)
    }
}

pub fn load() -> Option<ActiveMeeting> {
    let raw = std::fs::read_to_string(paths::active_meeting_file()).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Write the state file atomically.
///
/// A torn write here would strand a meeting: the recorder polls this file to learn
/// when to stop, and a half-written file parses as nothing at all.
pub fn save(meeting: &ActiveMeeting) -> Result<()> {
    let directory = paths::nextbase_dir();
    std::fs::create_dir_all(&directory)?;
    let target = paths::active_meeting_file();
    let temporary = directory.join(format!("active-meeting.json.{}.tmp", std::process::id()));

    std::fs::write(&temporary, serde_json::to_string_pretty(meeting)?)?;
    std::fs::rename(&temporary, &target)
        .with_context(|| format!("Could not write {}", target.display()))?;
    Ok(())
}

/// Read, modify and write back in one step.
pub fn update(change: impl FnOnce(&mut ActiveMeeting)) -> Result<Option<ActiveMeeting>> {
    let Some(mut meeting) = load() else {
        return Ok(None);
    };
    change(&mut meeting);
    save(&meeting)?;
    Ok(Some(meeting))
}

pub fn clear() {
    let _ = std::fs::remove_file(paths::active_meeting_file());
}

/// Move a finished meeting's state into its own directory and clear the active slot.
///
/// Keeps a record next to the deliverables, so a meeting's history survives the next
/// `start` overwriting the active file.
pub fn archive(meeting: &ActiveMeeting) -> Result<()> {
    let directory = meeting.directory();
    std::fs::create_dir_all(&directory)?;
    std::fs::write(
        directory.join("meeting-state.json"),
        serde_json::to_string_pretty(meeting)?,
    )?;
    clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_serialise_as_readable_kebab_case() {
        // The state file is meant to be readable when debugging a stuck meeting.
        let json = serde_json::to_string(&Phase::AwaitingApproval).unwrap();
        assert_eq!(json, "\"awaiting-approval\"");
        assert_eq!(
            serde_json::from_str::<Phase>("\"recorded\"").unwrap(),
            Phase::Recorded
        );
    }

    #[test]
    fn capturing_phases_are_the_ones_holding_the_microphone() {
        for phase in [Phase::Starting, Phase::Recording, Phase::Stopping] {
            assert!(phase.is_capturing(), "{phase}");
        }
        for phase in [
            Phase::Recorded,
            Phase::Sampling,
            Phase::AwaitingApproval,
            Phase::Transcribing,
            Phase::Summarising,
            Phase::Done,
            Phase::Failed,
        ] {
            assert!(!phase.is_capturing(), "{phase}");
        }
    }

    #[test]
    fn only_done_and_failed_are_finished() {
        assert!(Phase::Done.is_finished());
        assert!(Phase::Failed.is_finished());
        // Awaiting approval is emphatically not finished: it is waiting on a person.
        assert!(!Phase::AwaitingApproval.is_finished());
        assert!(!Phase::Recorded.is_finished());
    }

    #[test]
    fn processing_phases_have_work_in_flight() {
        for phase in [Phase::Sampling, Phase::Transcribing, Phase::Summarising] {
            assert!(phase.is_processing(), "{phase}");
        }
        assert!(!Phase::AwaitingApproval.is_processing());
        assert!(!Phase::Recording.is_processing());
    }

    #[test]
    fn a_meeting_round_trips_through_json_with_its_sample() {
        let mut meeting = ActiveMeeting::new("meeting-1");
        meeting.phase = Phase::AwaitingApproval;
        meeting.approved_mode = Some(Mode::Codemix);
        meeting.sample = Some(SampleReport {
            window_start_seconds: 60.0,
            window_seconds: 180.0,
            window_rms: 0.05,
            candidates: vec![SampleCandidate {
                mode: Mode::Transcribe,
                text: "hello".into(),
                elapsed_seconds: 12.5,
                segment_count: 4,
                speaker_labels: 2,
                overlapping_segments: 1,
                detected_language: Some("gu-IN".into()),
                covered_seconds: Some(178.0),
                sample_seconds: 180.0,
                error: None,
            }],
        });

        let json = serde_json::to_string(&meeting).unwrap();
        let back: ActiveMeeting = serde_json::from_str(&json).unwrap();
        assert_eq!(back.phase, Phase::AwaitingApproval);
        assert_eq!(back.approved_mode, Some(Mode::Codemix));
        assert_eq!(back.sample.unwrap().candidates[0].mode, Mode::Transcribe);
    }

    #[test]
    fn elapsed_is_measured_from_the_recorded_start_time() {
        let mut meeting = ActiveMeeting::new("meeting-2");
        meeting.started_at = (chrono::Utc::now() - chrono::Duration::seconds(90)).to_rfc3339();
        let elapsed = meeting.elapsed_seconds();
        assert!((89.0..=92.0).contains(&elapsed), "{elapsed}");
    }

    #[test]
    fn an_unparseable_start_time_reports_zero_rather_than_panicking() {
        let mut meeting = ActiveMeeting::new("meeting-3");
        meeting.started_at = "not a timestamp".into();
        assert_eq!(meeting.elapsed_seconds(), 0.0);
    }
}
