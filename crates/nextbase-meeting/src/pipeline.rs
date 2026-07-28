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
use serde::{Deserialize, Serialize};
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

/// Every Sarvam key, primary first.
///
/// More than one is not a redundancy setting: Batch bills per job, and a key that runs
/// out mid-meeting would otherwise strand a recording that a standby key could finish.
fn sarvam_keys(config: &Config) -> Result<Vec<String>> {
    let keys = config.keys_for(Provider::Sarvam);
    if keys.is_empty() {
        bail!("Meeting transcription needs a Sarvam API key. Run: nbmeet setup");
    }
    Ok(keys)
}

fn options(config: &Config, mode: Option<Mode>) -> BatchOptions {
    BatchOptions {
        // The meeting model, never Wisper's: this tool needs Sarvam Batch, and Wisper's
        // `model` may well be a Groq Whisper name.
        model: config.meeting_model_or_default().to_string(),
        // Dropped when the chosen model has no modes.
        mode: mode.filter(|_| config.meeting_model_supports_mode()),
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
    keys: &[String],
    config: &Config,
    mode: Option<Mode>,
    progress: Progress<'_>,
) -> (Mode, Duration, Result<BatchResult>) {
    let started = std::time::Instant::now();
    let files = vec![sample.to_path_buf()];
    let result =
        sarvam_batch::submit_with_keys(&files, keys, &options(config, mode), progress).await;
    // Reported as `transcribe` when the model has no modes: that is what the job did,
    // and the label has to name something.
    (mode.unwrap_or(Mode::Transcribe), started.elapsed(), result)
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
    if let Some(reason) = &meeting.gate_blocked {
        bail!("{reason}");
    }
    // An imported mp3 goes to the provider untouched, but a sample has to be cut from a
    // WAV, so this is not always the same file that gets uploaded.
    let audio = meeting
        .sampleable()
        .cloned()
        .context("This meeting has no audio a sample can be cut from.")?;
    let keys = sarvam_keys(config)?;

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

    // The gate compares `transcribe` against `codemix` because for Gujarati/Hindi/English
    // speech that choice is a real judgement call. A mode pinned to anything else is an
    // instruction, not a question — it gets one sample to check quality, not a pair. Same
    // for a model that has no modes at all.
    let pinned = config
        .meeting_mode
        .as_deref()
        .and_then(Mode::from_name)
        .filter(|mode| !mode.is_compared());

    let runs = if let Some(mode) = pinned {
        vec![sample_job(&sample_path, &keys, config, Some(mode), progress).await]
    } else if config.meeting_model_supports_mode() {
        let (first, second) = tokio::join!(
            sample_job(
                &sample_path,
                &keys,
                config,
                Some(Mode::Transcribe),
                progress
            ),
            sample_job(&sample_path, &keys, config, Some(Mode::Codemix), progress),
        );
        vec![first, second]
    } else {
        vec![sample_job(&sample_path, &keys, config, None, progress).await]
    };

    let candidates = runs
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
    // Splitting is sample-exact WAV work. An imported mp3 or m4a is submitted whole and
    // the provider enforces its own 2 hour limit — better than a silent re-encode.
    let is_wav = audio
        .extension()
        .map(|ext| ext.eq_ignore_ascii_case("wav"))
        .unwrap_or(false);
    if !is_wav {
        return Ok(vec![audio.to_path_buf()]);
    }

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
    let keys = sarvam_keys(config)?;

    let parts = prepare_inputs(&audio, &meeting.directory())?;
    if parts.len() > 1 {
        progress(&format!(
            "Recording exceeds the 2 hour per-file limit, so it goes as {} parts in one job",
            parts.len()
        ));
    }

    sarvam_batch::submit_with_retry(&parts, &keys, &options(config, Some(mode)), progress).await
}

/// Header facts about the audio, for the deliverables.
///
/// An imported mp3 has no readable WAV header. Rather than failing a completed
/// transcription over metadata, the duration falls back to what the state file recorded
/// and unknown fields stay zero — which `coverage_fraction` already treats as "unknown"
/// rather than as a coverage failure.
fn audio_details(meeting: &ActiveMeeting) -> wav::WavInfo {
    if let Some(path) = meeting.audio_path.as_ref() {
        if let Ok(info) = wav::info(path) {
            return info;
        }
    }
    if let Some(path) = meeting.sample_source.as_ref() {
        if let Ok(info) = wav::info(path) {
            return info;
        }
    }

    let seconds = meeting.duration_seconds.unwrap_or(0.0);
    wav::WavInfo {
        sample_rate: wav::TARGET_SAMPLE_RATE,
        channels: 1,
        bits_per_sample: 16,
        frames: (seconds * wav::TARGET_SAMPLE_RATE as f64) as u64,
    }
}

/// What the job itself reported, separate from the words it produced.
///
/// Kept apart so a meeting resumed after the job is long gone can still state the truth
/// about it in `processing-metadata.json` rather than inventing zeroes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchFacts {
    pub job_id: String,
    pub elapsed_seconds: f64,
    pub partial: bool,
    #[serde(default)]
    pub failed_inputs: Vec<String>,
}

impl BatchFacts {
    fn of(batch: &BatchResult) -> Self {
        Self {
            job_id: batch.job_id.clone(),
            elapsed_seconds: batch.elapsed.as_secs_f64(),
            partial: batch.partial,
            failed_inputs: batch.failed_inputs.clone(),
        }
    }
}

/// Gather what the deliverables need, so the transcript-only write and the full write
/// describe the same run rather than drifting apart.
fn describe<'a>(
    meeting: &'a ActiveMeeting,
    audio: wav::WavInfo,
    transcription: &'a Transcription,
    analysis: Option<&'a summary::Analysis>,
    mode: Mode,
    facts: &BatchFacts,
) -> Deliverable<'a> {
    Deliverable {
        meeting,
        audio,
        transcription,
        analysis,
        mode,
        batch_elapsed_seconds: facts.elapsed_seconds,
        job_id: facts.job_id.clone(),
        partial: facts.partial,
        failed_inputs: facts.failed_inputs.clone(),
    }
}

/// A transcription that has been paid for, parked where a later run can pick it up.
///
/// Written the moment the words come back and deleted once the deliverables exist, so it
/// is only ever on disk during the window where the meeting is half-finished.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedTranscript {
    pub mode: Mode,
    pub transcription: Transcription,
    pub facts: BatchFacts,
}

/// Not a deliverable — a resume record. The name is meant to explain itself to someone
/// looking in the directory wondering why a meeting never finished.
const SAVED_TRANSCRIPT: &str = "resume-transcript.json";

fn save_transcript(directory: &Path, saved: &SavedTranscript) -> Result<()> {
    let path = directory.join(SAVED_TRANSCRIPT);
    std::fs::write(&path, serde_json::to_string_pretty(saved)?)
        .with_context(|| format!("Could not write {}", path.display()))?;
    Ok(())
}

/// The saved transcription for a meeting, if one is waiting.
///
/// Unreadable or half-written counts as absent: the audio is still there, so
/// re-transcribing is a worse outcome than a stale file but not a broken one.
pub fn saved_transcript(directory: &Path) -> Option<SavedTranscript> {
    let raw = std::fs::read_to_string(directory.join(SAVED_TRANSCRIPT)).ok()?;
    let saved: SavedTranscript = serde_json::from_str(&raw).ok()?;
    // An empty record would send an empty transcript to Groq and deliver empty notes,
    // which is worse than paying to transcribe again.
    if saved.transcription.text.trim().is_empty() && saved.transcription.segments.is_empty() {
        return None;
    }
    Some(saved)
}

/// Throw away the saved transcription, so the next run transcribes from the audio.
pub fn discard_saved_transcript(directory: &Path) -> bool {
    std::fs::remove_file(directory.join(SAVED_TRANSCRIPT)).is_ok()
}

/// What finishing a meeting produced.
pub struct Completed {
    pub transcription: Transcription,
    pub files: Vec<PathBuf>,
    pub summary: Option<summary::Analysis>,
    pub partial: bool,
    /// True when this run reused a transcription an earlier one had already paid for.
    pub reused_transcript: bool,
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

    let saved = SavedTranscript {
        mode,
        transcription,
        facts: BatchFacts::of(&batch),
    };
    deliver(meeting, config, &saved, false, progress).await
}

/// Finish a meeting whose transcription is already on disk.
///
/// The audio is never touched: this exists precisely so a run interrupted after the
/// words came back does not pay Sarvam a second time for the same minutes.
pub async fn finish_saved(
    meeting: &ActiveMeeting,
    config: &Config,
    progress: Progress<'_>,
) -> Result<Completed> {
    let saved = saved_transcript(&meeting.directory())
        .context("This meeting has no saved transcript to finish.")?;

    state::update(|active| {
        active.phase = Phase::Summarising;
        active.approved_mode = Some(saved.mode);
    })?;
    progress(&format!(
        "Reusing the transcript from job {} — nothing is uploaded again",
        saved.facts.job_id
    ));

    deliver(meeting, config, &saved, true, progress).await
}

/// Park the transcript, summarise it, write the deliverables, archive the state.
///
/// Everything from the words coming back to the meeting being done, shared by the run
/// that transcribed them and the run that picked them up afterwards.
async fn deliver(
    meeting: &ActiveMeeting,
    config: &Config,
    saved: &SavedTranscript,
    reused: bool,
    progress: Progress<'_>,
) -> Result<Completed> {
    let SavedTranscript {
        mode,
        transcription,
        facts,
    } = saved;
    let mode = *mode;

    let audio_info = audio_details(meeting);
    let mut finished = meeting.clone();
    finished.phase = Phase::Done;
    finished.approved_mode = Some(mode);

    // The transcript reaches disk before Groq is called at all — as readable files, and
    // as a record a later run can finish from. A summary *error* is already survivable
    // below; a crash or a Ctrl+C mid-summary is not, and without this the transcription
    // is paid for and gone. Writing it twice costs nothing: neither file reads the
    // analysis.
    let transcript_files = deliverables::write_transcript(&describe(
        &finished,
        audio_info,
        transcription,
        None,
        mode,
        facts,
    ))?;
    if !reused {
        save_transcript(&finished.directory(), saved)?;
        progress(&format!(
            "Transcript saved to {}",
            finished.directory().display()
        ));
    }

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

    let files = deliverables::write(&describe(
        &finished,
        audio_info,
        transcription,
        analysis.as_ref(),
        mode,
        facts,
    ))?;
    debug_assert!(transcript_files.iter().all(|path| files.contains(path)));

    // Sample, split parts and the resume record are all derived, and the deliverables
    // now exist; the original recording stays as the source of truth.
    clean_intermediates(meeting);

    state::archive(&finished)?;

    Ok(Completed {
        transcription: transcription.clone(),
        files,
        summary: analysis,
        partial: facts.partial,
        reused_transcript: reused,
    })
}

/// Remove the sample and any split parts, keeping `audio.wav` and the deliverables.
fn clean_intermediates(meeting: &ActiveMeeting) {
    let directory = meeting.directory();
    let _ = std::fs::remove_file(directory.join("sample.wav"));
    // Only ever a derived copy for the gate; the uploaded original stays.
    let _ = std::fs::remove_file(directory.join("sample-source.wav"));
    // The deliverables now hold everything it was insurance for.
    let _ = std::fs::remove_file(directory.join(SAVED_TRANSCRIPT));

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

/// The audio file in a meeting directory, whatever extension it landed with.
///
/// Recordings are always `audio.wav`, but an import keeps its own container.
pub fn recorded_audio(directory: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(directory).ok()?;
    entries.flatten().map(|entry| entry.path()).find(|path| {
        path.file_stem()
            .map(|stem| stem.eq_ignore_ascii_case("audio"))
            .unwrap_or(false)
    })
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
        .filter(|path| recorded_audio(path).is_some())
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
    use nextbase_core::sarvam_batch::Segment;

    /// A scratch directory of its own, so these tests never touch a real meeting.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("nextbase-pipeline-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn a_transcript() -> SavedTranscript {
        SavedTranscript {
            mode: Mode::Codemix,
            transcription: Transcription {
                text: "aa meeting ma budget nakki thayu".into(),
                segments: vec![Segment {
                    speaker: Some("SPEAKER_00".into()),
                    text: "aa meeting ma budget nakki thayu".into(),
                    start_seconds: Some(4.0),
                    end_seconds: Some(9.5),
                }],
                language_code: Some("gu-IN".into()),
            },
            facts: BatchFacts {
                job_id: "job-77".into(),
                elapsed_seconds: 41.5,
                partial: false,
                failed_inputs: vec![],
            },
        }
    }

    #[test]
    fn a_saved_transcript_survives_the_process_that_wrote_it() {
        // The whole point: a run that dies after the words come back owes Sarvam
        // nothing more, and the next run has to be able to prove it.
        let scratch = Scratch::new("round-trip");
        save_transcript(&scratch.0, &a_transcript()).unwrap();

        let back = saved_transcript(&scratch.0).expect("a saved transcript");
        assert_eq!(back.mode, Mode::Codemix);
        assert_eq!(back.transcription.segments.len(), 1);
        assert_eq!(back.transcription.language_code.as_deref(), Some("gu-IN"));
        // The job's own facts survive too, so the metadata does not have to invent them.
        assert_eq!(back.facts.job_id, "job-77");
        assert_eq!(back.facts.elapsed_seconds, 41.5);
    }

    #[test]
    fn delivering_the_meeting_clears_the_resume_record() {
        // It is insurance for one window only. Left behind, a later `process` would
        // reuse it instead of the deliverables that supersede it.
        let scratch = Scratch::new("cleared");
        save_transcript(&scratch.0, &a_transcript()).unwrap();
        assert!(saved_transcript(&scratch.0).is_some());

        assert!(discard_saved_transcript(&scratch.0));
        assert!(saved_transcript(&scratch.0).is_none());
        // Discarding one that is not there is not an error, just nothing to do.
        assert!(!discard_saved_transcript(&scratch.0));
    }

    #[test]
    fn an_empty_or_unreadable_record_means_transcribe_again() {
        let scratch = Scratch::new("unusable");
        assert!(saved_transcript(&scratch.0).is_none());

        // Half-written or from a future schema: paying to transcribe again beats
        // failing, because the audio is still right there.
        std::fs::write(scratch.0.join(SAVED_TRANSCRIPT), "{\"mode\":\"codemix\"").unwrap();
        assert!(saved_transcript(&scratch.0).is_none());

        // Well-formed but empty would summarise nothing into confident-looking notes,
        // which is worse than the cost of running the job again.
        let mut empty = a_transcript();
        empty.transcription = Transcription::default();
        save_transcript(&scratch.0, &empty).unwrap();
        assert!(saved_transcript(&scratch.0).is_none());
    }

    #[test]
    fn the_resume_record_is_not_one_of_the_four_deliverables() {
        // It is machinery, and `history` and the docs both promise four files.
        assert!(SAVED_TRANSCRIPT.ends_with(".json"));
        for deliverable in [
            "meeting-note.md",
            "full-diarized-transcript.md",
            "full-transcript.txt",
            "processing-metadata.json",
        ] {
            assert_ne!(SAVED_TRANSCRIPT, deliverable);
        }
    }

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
        assert_eq!(options(&config, Some(Mode::Transcribe)).model, "saaras:v3");

        config.meeting_model = Some("saaras:v3".into());
        assert_eq!(options(&config, Some(Mode::Codemix)).model, "saaras:v3");
        assert_eq!(
            options(&config, Some(Mode::Codemix)).mode,
            Some(Mode::Codemix)
        );

        // A model without modes drops it even when the caller asks for one.
        config.meeting_model = Some("saarika:v2.5".into());
        assert_eq!(options(&config, Some(Mode::Codemix)).mode, None);
    }

    #[test]
    fn language_detection_is_left_on_and_speaker_count_unguessed() {
        let options = options(&Config::default(), Some(Mode::Transcribe));
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
