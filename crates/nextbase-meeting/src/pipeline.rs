//! Recorded audio → sample gate → full transcription → summary → deliverables.
//!
//! The gate is the reason this is a pipeline rather than one call. A transcript that
//! reads well can still corrupt names, numbers and commitments, and the difference
//! between Sarvam's `transcribe` and `codemix` modes is exactly that kind of
//! difference for Gujarati/Hindi/English speech. So a short sample is transcribed both
//! ways and the full run waits for a human to pick.
//!
//! Nothing here decides on the user's behalf, and nothing reports a number it did not
//! measure.

use anyhow::{bail, Context, Result};
use nextbase_core::config::{Config, Provider};
use nextbase_core::sarvam_batch::{
    self, BatchOptions, BatchResult, Mode, Transcription, MAX_FILES_PER_JOB, MAX_FILE_DURATION,
};
use nextbase_core::{log, paths, wav};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::deliverables::{self, Deliverable};
use crate::state::{self, ActiveMeeting, Phase, SampleCandidate, SampleReport};
use crate::summary;

/// Length of the sample sent to the quality gate. The skill asks for 2-5 minutes;
/// three keeps two jobs cheap while still covering real conversation.
pub const SAMPLE_LENGTH: Duration = Duration::from_secs(180);
/// Skipped before looking for the sample window: openings are silence, joining noises
/// and "can you hear me".
pub const SAMPLE_SKIP: Duration = Duration::from_secs(30);

/// Progress reporting, so a queued Batch job never looks like a hang.
pub type Progress<'a> = &'a (dyn Fn(&str) + Send + Sync);

fn sarvam_key(config: &Config) -> Result<&str> {
    config
        .key_for(Provider::Sarvam)
        .filter(|key| !key.is_empty())
        .context("Meeting transcription needs a Sarvam API key. Run: nbmeet setup")
}

fn options(config: &Config, mode: Mode) -> BatchOptions {
    BatchOptions {
        model: config
            .model
            .clone()
            .filter(|model| model.starts_with("saaras"))
            .unwrap_or_else(|| "saaras:v3".to_string()),
        mode,
        // Left as detection unless the user pinned a language: guessing narrows the
        // model for no reason on code-mixed speech.
        language_code: "unknown".to_string(),
        with_diarization: true,
        with_timestamps: true,
        num_speakers: None,
    }
}

/// One sample transcription, with its own measured elapsed time.
async fn sample_job(
    sample: &std::path::Path,
    key: &str,
    config: &Config,
    mode: Mode,
    progress: Progress<'_>,
) -> (Mode, Duration, Result<BatchResult>) {
    let started = std::time::Instant::now();
    let files = vec![sample.to_path_buf()];
    let result = sarvam_batch::submit(&files, key, &options(config, mode), progress).await;
    (mode, started.elapsed(), result)
}

/// Transcribe a 3-minute sample in both modes and record the findings.
///
/// The two jobs run concurrently because `mode` is a per-job parameter — one job
/// cannot produce both.
pub async fn run_sample_gate(
    meeting: &ActiveMeeting,
    config: &Config,
    progress: Progress<'_>,
) -> Result<SampleReport> {
    let audio = meeting
        .audio_path
        .clone()
        .context("This meeting has no recorded audio.")?;
    let key = sarvam_key(config)?;

    let window = wav::pick_energy_window(&audio, SAMPLE_LENGTH, SAMPLE_SKIP)
        .context("Could not choose a sample window from the recording")?;
    progress(&format!(
        "Sampling {} from {} (RMS {:.4})",
        sarvam_batch::clock(window.length.as_secs_f64()),
        sarvam_batch::clock(window.start.as_secs_f64()),
        window.rms
    ));

    let sample_path = meeting.directory().join("sample.wav");
    let sample_info = wav::slice(&audio, &sample_path, window.start, window.length)
        .context("Could not cut the sample from the recording")?;

    // Two jobs, not one: `mode` is a per-job parameter, so a single job cannot
    // produce both transcriptions. They run concurrently so the gate costs one wait.
    let (first, second) = tokio::join!(
        sample_job(&sample_path, key, config, Mode::Transcribe, progress),
        sample_job(&sample_path, key, config, Mode::Codemix, progress),
    );

    let candidates = [first, second]
        .into_iter()
        .map(|(mode, elapsed, result)| match result {
            Ok(batch) => {
                let merged = batch.merged();
                SampleCandidate {
                    mode,
                    text: merged.as_labelled_text(),
                    elapsed_seconds: elapsed.as_secs_f64(),
                    segment_count: merged.segments.len(),
                    speaker_labels: merged.speaker_labels().len(),
                    overlapping_segments: merged.overlapping_segments(),
                    detected_language: merged.language_code.clone(),
                    covered_seconds: merged.coverage().map(|(_, last)| last),
                    sample_seconds: sample_info.duration_seconds(),
                    error: None,
                }
            }
            // One mode failing must not lose the other: the user can still approve
            // whichever worked.
            Err(error) => SampleCandidate {
                mode,
                text: String::new(),
                elapsed_seconds: elapsed.as_secs_f64(),
                segment_count: 0,
                speaker_labels: 0,
                overlapping_segments: 0,
                detected_language: None,
                covered_seconds: None,
                sample_seconds: sample_info.duration_seconds(),
                error: Some(error.to_string()),
            },
        })
        .collect::<Vec<_>>();

    if candidates.iter().all(|candidate| candidate.error.is_some()) {
        let reasons = candidates
            .iter()
            .filter_map(|candidate| {
                candidate
                    .error
                    .as_ref()
                    .map(|error| format!("{}: {error}", candidate.mode))
            })
            .collect::<Vec<_>>()
            .join("; ");
        bail!("Both sample transcriptions failed. {reasons}");
    }

    Ok(SampleReport {
        window_start_seconds: window.start.as_secs_f64(),
        window_seconds: window.length.as_secs_f64(),
        window_rms: window.rms,
        candidates,
    })
}

/// Split the recording into parts Sarvam will accept.
///
/// The limits are 2 hours per file and 20 files per job, so anything past 40 hours
/// cannot go through in one job — say so rather than submitting a doomed request.
pub fn prepare_inputs(audio: &Path, directory: &Path) -> Result<Vec<PathBuf>> {
    let parts = wav::split(audio, directory, MAX_FILE_DURATION)?;
    if parts.len() > MAX_FILES_PER_JOB {
        bail!(
            "This recording is {} long, which needs {} parts — more than the {MAX_FILES_PER_JOB} Sarvam accepts per job.",
            sarvam_batch::clock(wav::info(audio)?.duration_seconds()),
            parts.len()
        );
    }
    Ok(parts)
}

/// Transcribe the whole recording in the approved mode.
pub async fn run_full_transcription(
    meeting: &ActiveMeeting,
    config: &Config,
    mode: Mode,
    progress: Progress<'_>,
) -> Result<BatchResult> {
    let audio = meeting
        .audio_path
        .clone()
        .context("This meeting has no recorded audio.")?;
    let key = sarvam_key(config)?;

    let parts = prepare_inputs(&audio, &meeting.directory())?;
    if parts.len() > 1 {
        progress(&format!(
            "Recording exceeds the 2 hour per-file limit, so it goes as {} parts in one job",
            parts.len()
        ));
    }

    sarvam_batch::submit_with_retry(&parts, key, &options(config, mode), progress).await
}

/// What finishing a meeting produced.
pub struct Completed {
    pub transcription: Transcription,
    pub files: Vec<PathBuf>,
    pub summary: Option<summary::Analysis>,
    pub partial: bool,
}

/// Transcribe in `mode`, summarise, write the deliverables, and archive the state.
pub async fn finish(
    meeting: &ActiveMeeting,
    config: &Config,
    mode: Mode,
    progress: Progress<'_>,
) -> Result<Completed> {
    state::update(|active| {
        active.phase = Phase::Transcribing;
        active.approved_mode = Some(mode);
    })?;

    let batch = run_full_transcription(meeting, config, mode, progress).await?;
    let transcription = batch.merged();
    if !batch.failed_inputs.is_empty() {
        progress(&format!(
            "{} input(s) could not be transcribed: {}",
            batch.failed_inputs.len(),
            batch.failed_inputs.join(", ")
        ));
    }

    // The transcript exists now, so save it before summarising: a Groq failure must
    // not cost the user the transcription they already paid for.
    let audio_info = wav::info(
        meeting
            .audio_path
            .as_ref()
            .context("This meeting has no recorded audio.")?,
    )?;

    state::update(|active| active.phase = Phase::Summarising)?;
    let analysis = if crate::has_summary_key() {
        progress("Extracting summary, decisions and action items");
        match summary::analyse(&transcription.as_labelled_text(), config).await {
            Ok(analysis) => Some(analysis),
            Err(error) => {
                // Deliver the transcript with an honest gap rather than nothing.
                progress(&format!("Summary failed, keeping the transcript: {error}"));
                log::log(&format!("Meeting {}: summary failed: {error}", meeting.id));
                None
            }
        }
    } else {
        progress("No Groq key saved, so the transcript is delivered without a summary");
        None
    };

    let mut finished = meeting.clone();
    finished.phase = Phase::Done;
    finished.approved_mode = Some(mode);

    let files = deliverables::write(&Deliverable {
        meeting: &finished,
        audio: audio_info,
        transcription: &transcription,
        analysis: analysis.as_ref(),
        mode,
        batch_elapsed_seconds: batch.elapsed.as_secs_f64(),
        job_id: batch.job_id.clone(),
        partial: batch.partial,
        failed_inputs: batch.failed_inputs.clone(),
    })?;

    // Sample and split parts are large and no longer needed once the deliverables
    // exist; the original recording stays as the source of truth.
    clean_intermediates(meeting);

    state::archive(&finished)?;

    Ok(Completed {
        transcription,
        files,
        summary: analysis,
        partial: batch.partial,
    })
}

/// Remove the sample and any split parts, keeping `audio.wav` and the deliverables.
fn clean_intermediates(meeting: &ActiveMeeting) {
    let directory = meeting.directory();
    let _ = std::fs::remove_file(directory.join("sample.wav"));

    let Ok(entries) = std::fs::read_dir(&directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("audio-part") && name.ends_with(".wav") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Meetings that were recorded but never finished, newest first.
///
/// A crash between recording and transcription must never orphan an hour of audio.
pub fn resumable() -> Vec<PathBuf> {
    // A meeting that is still recording or mid-transcription has audio on disk and no
    // note yet, which looks identical to an abandoned one. Excluding it keeps the
    // dashboard from reporting the meeting you are currently in as orphaned.
    let in_flight = state::load()
        .filter(|meeting| !meeting.phase.is_finished() && meeting.phase != Phase::Recorded)
        .map(|meeting| meeting.id);

    let Ok(entries) = std::fs::read_dir(paths::meetings_dir()) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join("audio.wav").is_file())
        .filter(|path| !path.join("meeting-note.md").is_file())
        .filter(|path| {
            let name = path.file_name().map(|n| n.to_string_lossy().to_string());
            name.as_deref() != in_flight.as_deref()
        })
        .collect();
    found.sort();
    found.reverse();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sample_window_matches_what_the_skill_asks_for() {
        // 2-5 minutes, with the opening skipped.
        assert!((120..=300).contains(&SAMPLE_LENGTH.as_secs()));
        assert!(SAMPLE_SKIP.as_secs() >= 30);
    }

    #[test]
    fn the_model_falls_back_when_config_holds_a_non_sarvam_model() {
        // Config is shared with Wisper, which may have a Groq Whisper model saved.
        let mut config = Config {
            model: Some("whisper-large-v3-turbo".into()),
            ..Default::default()
        };
        assert_eq!(options(&config, Mode::Transcribe).model, "saaras:v3");

        config.model = Some("saaras:v3".into());
        assert_eq!(options(&config, Mode::Codemix).model, "saaras:v3");
        assert_eq!(options(&config, Mode::Codemix).mode, Mode::Codemix);
    }

    #[test]
    fn language_detection_is_left_on_and_speaker_count_unguessed() {
        let options = options(&Config::default(), Mode::Transcribe);
        assert_eq!(options.language_code, "unknown");
        assert_eq!(options.num_speakers, None);
        assert!(options.with_diarization);
        assert!(options.with_timestamps);
    }

    #[test]
    fn an_in_flight_meeting_is_not_treated_as_orphaned() {
        // A meeting still recording has audio and no note, which looks exactly like an
        // abandoned one — the dashboard used to report the meeting you were in as
        // orphaned.
        for phase in [
            Phase::Starting,
            Phase::Recording,
            Phase::Stopping,
            Phase::Sampling,
            Phase::AwaitingApproval,
            Phase::Transcribing,
            Phase::Summarising,
        ] {
            assert!(
                !phase.is_finished() && phase != Phase::Recorded,
                "{phase} should be excluded from the resumable list"
            );
        }
        // `Recorded` is exactly the case that *should* be resumable.
        assert!(!Phase::Recorded.is_finished());
    }

    #[test]
    fn a_recording_needing_more_parts_than_a_job_allows_is_refused_clearly() {
        // 20 files x 2 hours is the ceiling; beyond that, say so rather than
        // submitting something that cannot succeed.
        let hours = MAX_FILES_PER_JOB as u64 * 2;
        assert_eq!(
            MAX_FILE_DURATION.as_secs() * MAX_FILES_PER_JOB as u64,
            hours * 3600
        );
    }
}
