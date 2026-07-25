//! The `nbmeet` command surface.
//!
//! `start` and `stop` are the whole interface for a normal meeting; everything else
//! exists for when something went wrong or needs checking first.

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use inquire::{Confirm, Select};
use nextbase_core::capture::{self, SourceKind, SystemAudioStatus};
use nextbase_core::config::{self, Provider};
use nextbase_core::sarvam_batch::{clock, Mode};
use nextbase_core::{autostart, paths, wav};
use nextbase_meeting::state::{self, Phase, SampleCandidate};
use nextbase_meeting::{pipeline, recorder};
use std::io::IsTerminal;
use std::time::Duration;

use crate::ui;

/// How long `stop` waits for the recorder to finalize its WAV.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Parser)]
#[command(
    name = "nbmeet",
    version,
    about = "Meeting Agent — record a meeting, transcribe it, get notes"
)]
pub struct MeetingCli {
    #[command(subcommand)]
    pub command: Option<MeetingCommand>,
}

#[derive(Debug, Args)]
pub struct MeetingArgs {
    #[command(subcommand)]
    pub command: Option<MeetingCommand>,
}

#[derive(Debug, Subcommand)]
pub enum MeetingCommand {
    /// Store the transcription and summary keys
    Setup,
    /// Start recording a meeting
    Start {
        /// Skip the upload-consent prompt, for scripts
        #[arg(long)]
        yes: bool,
    },
    /// Stop recording, then transcribe and summarise
    Stop,
    /// Show the active meeting
    Status,
    /// Approve a sample and run the full transcription
    Approve {
        /// `transcribe` or `codemix`. Omit to be asked.
        mode: Option<String>,
    },
    /// Discard a meeting waiting for approval
    Reject,
    /// Transcribe an existing recording: a local file, or a remote URL
    Audio {
        /// Path or http(s) URL
        #[arg(trailing_var_arg = true)]
        source: Vec<String>,
    },
    /// Finish a meeting that was recorded but never transcribed
    Process {
        /// Meeting id. Omit for the most recent unfinished one.
        id: Option<String>,
    },
    /// List past meetings
    History { limit: Option<usize> },
    /// Check capture sources, permissions and keys
    Doctor,
    /// Open the local dashboard, where a meeting can be started and stopped
    #[command(alias = "app")]
    Open { port: Option<u16> },
    /// Require sample approval before each full run: `gate on|off|status`
    Gate {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Internal: the detached recorder
    #[command(hide = true, name = "_record")]
    RecordInternal { id: String },
}

pub async fn dispatch(command: Option<MeetingCommand>) -> Result<()> {
    match command {
        None => overview(),
        Some(MeetingCommand::Setup) => setup().await,
        Some(MeetingCommand::Start { yes }) => start(yes).await,
        Some(MeetingCommand::Stop) => stop().await,
        Some(MeetingCommand::Status) => status(),
        Some(MeetingCommand::Approve { mode }) => approve(mode.as_deref()).await,
        Some(MeetingCommand::Reject) => reject(),
        Some(MeetingCommand::Audio { source }) => audio(&source.join(" ")).await,
        Some(MeetingCommand::Process { id }) => process(id.as_deref()).await,
        Some(MeetingCommand::History { limit }) => history(limit),
        Some(MeetingCommand::Doctor) => doctor(),
        Some(MeetingCommand::Open { port }) => crate::commands::open(port).await,
        Some(MeetingCommand::Gate { args }) => gate(&args),
        Some(MeetingCommand::RecordInternal { id }) => recorder::run(&id),
    }
}

fn overview() -> Result<()> {
    ui::heading("Meeting Agent");
    match state::load() {
        Some(meeting) => {
            ui::field("Active meeting", &meeting.id);
            ui::field("State", meeting.phase.as_str());
            if meeting.phase.is_capturing() {
                ui::field("Elapsed", &clock(meeting.elapsed_seconds()));
            }
        }
        None => ui::info("No meeting in progress."),
    }
    println!();
    ui::heading("Common commands");
    ui::info("nbmeet start      Start recording");
    ui::info("nbmeet stop       Stop, transcribe, summarise");
    ui::info("nbmeet doctor     Check microphone, system audio and keys");
    ui::info("nbmeet history    Past meetings");
    ui::info("nbmeet audio <f>  Transcribe a file or URL you already have");
    ui::info("nbmeet open       Dashboard, with start and stop buttons");
    println!();
    ui::hint("Full list: nbmeet --help");
    Ok(())
}

// ------------------------------------------------------------------- setup

async fn setup() -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("Setup needs an interactive terminal. Run it directly in your shell.");
    }

    ui::heading("Meeting Agent setup");
    ui::info("Transcription uses Sarvam Batch: long audio, speaker labels, and");
    ui::info("Gujarati, Hindi, English and code-mixed speech.");
    println!();

    let settings = config::load();
    if settings
        .key_for(Provider::Sarvam)
        .filter(|key| !key.is_empty())
        .is_none()
    {
        let key = crate::commands::ask_provider_key(Provider::Sarvam).await?;
        config::update(|c| {
            c.set_key(Provider::Sarvam, key);
            // Shared config: only set the model if nothing usable is there, so this
            // never overwrites a Wisper provider choice.
            if c.model.is_none() {
                c.model = Some("saaras:v3".into());
            }
        })?;
        ui::success("Sarvam key saved.");
    } else {
        ui::success("Sarvam key already saved (shared with Wisper).");
    }

    if !nextbase_meeting::has_summary_key() {
        ui::info("Summaries, decisions and action items go through Groq.");
        let wants = Confirm::new("Add a Groq key for summaries?")
            .with_default(true)
            .prompt()
            .unwrap_or(false);
        if wants {
            let key = crate::commands::ask_provider_key(Provider::Groq).await?;
            config::update(|c| c.set_key(Provider::Groq, key))?;
            ui::success("Groq key saved.");
        } else {
            ui::warn("Without it, meetings are transcribed but not summarised.");
        }
    } else {
        ui::success("Groq summary key already saved.");
    }

    println!();
    let status = capture::system_audio_status();
    if let SystemAudioStatus::PermissionRequired { .. } = status {
        ui::warn("Recording the other participants needs system-audio permission.");
        request_system_permission();
    }

    println!();
    ui::success("Setup complete.");
    ui::hint("Check everything: nbmeet doctor");
    ui::hint("Then start a meeting: nbmeet start");
    Ok(())
}

/// Ask the OS for system-audio permission and wait briefly for it.
fn request_system_permission() -> bool {
    match capture::request_system_permission() {
        Ok(true) => {
            ui::success("System audio permission is granted.");
            return true;
        }
        Ok(false) => {
            ui::info("The system is asking for it now.");
            ui::hint("On macOS: allow Screen Recording, which is how system audio is captured.");
        }
        Err(error) => {
            ui::hint(&error.to_string());
            return false;
        }
    }

    let bar = ui::spinner("Waiting for permission...");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        if matches!(capture::system_audio_status(), SystemAudioStatus::Ready) {
            bar.finish_and_clear();
            ui::success("System audio permission is granted.");
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    bar.finish_and_clear();

    // macOS binds this grant to the binary, and a newly granted process often has to
    // restart before the capture API agrees.
    ui::warn("Not granted yet. Meetings will record your microphone only.");
    ui::hint("Grant it, then re-run: nbmeet doctor");
    false
}

// ------------------------------------------------------------ start / stop

async fn start(assume_yes: bool) -> Result<()> {
    nextbase_meeting::check_ready()?;

    if let Some(existing) = state::load() {
        if existing.phase.is_capturing() {
            bail!(
                "Meeting {} is already {}. Stop it first: nbmeet stop",
                existing.id,
                existing.phase
            );
        }
        if existing.phase == Phase::AwaitingApproval {
            bail!(
                "Meeting {} is waiting for sample approval. Run: nbmeet approve — or nbmeet reject",
                existing.id
            );
        }
        if existing.phase == Phase::Recorded {
            bail!(
                "Meeting {} was recorded but never transcribed. Finish it first: nbmeet process",
                existing.id
            );
        }
    }

    if !confirm_upload(assume_yes)? {
        return Ok(());
    }
    warn_about_capture_gaps();

    let id = nextbase_meeting::new_meeting_id();
    std::fs::create_dir_all(paths::meeting_dir(&id))?;
    state::save(&state::ActiveMeeting::new(&id))?;

    let pid = autostart::spawn_detached_with(&["_record", &id])?;
    ui::success(&format!("Recording started (pid {pid})."));

    // A detached recorder that dies immediately looks exactly like one that started,
    // so confirm it actually reached the recording phase.
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        match state::load() {
            Some(meeting) if meeting.phase == Phase::Recording => {
                ui::success("Verified: capture sources are open.");
                report_sources(&meeting);
                println!();
                ui::hint("When the meeting ends, run: nbmeet stop");
                return Ok(());
            }
            Some(meeting) if meeting.phase == Phase::Failed => {
                let reason = meeting.error.unwrap_or_else(|| "unknown".into());
                bail!("The recorder could not start: {reason}");
            }
            _ => {}
        }
    }

    ui::warn("The recorder has not confirmed yet. Check: nbmeet status");
    Ok(())
}

fn report_sources(meeting: &state::ActiveMeeting) {
    let settings = config::load();
    let (mic, system) = settings.meeting_capture();
    if mic {
        ui::field(
            "Microphone",
            settings.audio_device.as_deref().unwrap_or("default"),
        );
    }
    if system {
        match capture::system_source_name() {
            Some(name) => ui::field("System audio", &name),
            None => ui::warn("System audio could not be opened; only your side is recorded."),
        }
    }
    let _ = meeting;
}

/// State the upload plainly, once, and remember the answer.
fn confirm_upload(assume_yes: bool) -> Result<bool> {
    let settings = config::load();
    if settings.meeting_consent == Some(true) || assume_yes {
        if assume_yes && settings.meeting_consent != Some(true) {
            config::update(|c| c.meeting_consent = Some(true))?;
        }
        return Ok(true);
    }

    ui::warn("Meeting audio is uploaded to Sarvam to be transcribed.");
    ui::hint("Everyone being recorded should know that. Nothing is uploaded until you stop.");
    if !std::io::stdin().is_terminal() {
        bail!("Run `nbmeet start` in a terminal once to confirm this, or pass --yes.");
    }

    let agreed = Confirm::new("Record this meeting and upload the audio for transcription?")
        .with_default(true)
        .prompt()
        .unwrap_or(false);
    if !agreed {
        ui::info("Nothing was recorded.");
        return Ok(false);
    }
    config::update(|c| c.meeting_consent = Some(true))?;
    Ok(true)
}

/// Say up front when a source will be missing, rather than after the meeting.
fn warn_about_capture_gaps() {
    let settings = config::load();
    let (mic, system) = settings.meeting_capture();

    if !system {
        ui::warn("System audio is off, so the other participants will not be recorded.");
        return;
    }
    match capture::system_audio_status() {
        SystemAudioStatus::Ready => {}
        SystemAudioStatus::PermissionRequired { hint } => {
            ui::warn("System audio permission is missing, so only your side will be recorded.");
            ui::hint(&hint);
        }
        SystemAudioStatus::Unavailable { reason } => {
            ui::warn(&format!("System audio is unavailable: {reason}"));
            ui::hint("Only your microphone will be recorded.");
        }
    }
    if !mic {
        ui::warn("The microphone is off, so your own voice will not be recorded.");
    }
}

async fn stop() -> Result<()> {
    let bar = ui::spinner("Finalizing the recording...");
    let meeting = recorder::request_stop(STOP_TIMEOUT);
    bar.finish_and_clear();
    let meeting = meeting?;

    let duration = meeting.duration_seconds.unwrap_or(0.0);
    ui::success(&format!("Recorded {}.", clock(duration)));
    for level in &meeting.source_levels {
        if level.silent {
            // The worst failure mode: it all looks fine and the far side is absent.
            ui::failure(&format!(
                "{} was silent for the whole recording.",
                level.source
            ));
        } else {
            ui::field(&level.source, &format!("peak {:.3}", level.peak));
        }
    }

    if duration < 5.0 {
        ui::warn("That is too short to be a meeting. Nothing was transcribed.");
        ui::hint(&format!(
            "The audio is at {}",
            meeting
                .audio_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ));
        return Ok(());
    }

    println!();
    transcribe_and_finish(&meeting).await
}

/// The gate, then the full run.
async fn transcribe_and_finish(meeting: &state::ActiveMeeting) -> Result<()> {
    let settings = config::load();

    if let Some(reason) = &meeting.gate_blocked {
        ui::warn(reason);
        ui::hint("Install ffmpeg to enable the sample check for non-WAV files.");
        let mode = settings
            .meeting_mode
            .as_deref()
            .and_then(Mode::from_name)
            .unwrap_or(Mode::Transcribe);
        ui::info(&format!("Transcribing in `{mode}` mode."));
        return run_full(meeting, mode).await;
    }

    if !settings.meeting_gate_enabled() {
        let mode = settings
            .meeting_mode
            .as_deref()
            .and_then(Mode::from_name)
            .unwrap_or(Mode::Transcribe);
        ui::info(&format!(
            "Sample gate is off, transcribing directly in `{mode}` mode."
        ));
        return run_full(meeting, mode).await;
    }

    let bar = ui::spinner("Transcribing a sample in both modes...");
    let report = pipeline::run_sample_gate(meeting, &settings, &|message| {
        // Progress goes to the log so the spinner stays readable; a stuck job is
        // still diagnosable afterwards.
        nextbase_core::log::log(message);
    })
    .await;
    bar.finish_and_clear();

    let report = match report {
        Ok(report) => report,
        Err(error) => {
            state::update(|active| {
                active.phase = Phase::Recorded;
                active.error = Some(error.to_string());
            })?;
            ui::hint("The recording is kept. Retry with: nbmeet process");
            return Err(error);
        }
    };

    state::update(|active| {
        active.phase = Phase::AwaitingApproval;
        active.sample = Some(report.clone());
        active.error = None;
    })?;

    show_sample(&report);

    if !std::io::stdin().is_terminal() {
        ui::hint("Approve a mode to transcribe the whole recording:");
        ui::hint("  nbmeet approve transcribe");
        ui::hint("  nbmeet approve codemix");
        ui::hint("  nbmeet reject");
        return Ok(());
    }

    match ask_for_mode(&report)? {
        Some(mode) => {
            let latest = state::load().unwrap_or_else(|| meeting.clone());
            run_full(&latest, mode).await
        }
        None => {
            ui::info("Rejected. The recording is kept.");
            ui::hint("Retry the sample later with: nbmeet process");
            state::update(|active| active.phase = Phase::Recorded)?;
            Ok(())
        }
    }
}

/// Print the two candidates and the measurements behind them.
fn show_sample(report: &state::SampleReport) {
    ui::heading("Sample quality check");
    ui::field(
        "Window",
        &format!(
            "{} of audio from {}",
            clock(report.window_seconds),
            clock(report.window_start_seconds)
        ),
    );
    ui::field("Window RMS", &format!("{:.4}", report.window_rms));
    if report.window_rms < 0.005 {
        ui::warn("That window is nearly silent, so neither transcript means much.");
    }

    for candidate in &report.candidates {
        println!();
        ui::heading(&format!("Mode: {}", candidate.mode));
        if let Some(error) = &candidate.error {
            ui::failure(error);
            continue;
        }

        ui::field("Took", &clock(candidate.elapsed_seconds));
        ui::field(
            "Coverage",
            &match candidate.covered_seconds {
                Some(covered) => format!(
                    "{} of {} of sample",
                    clock(covered),
                    clock(candidate.sample_seconds)
                ),
                None => "no timestamps returned".to_string(),
            },
        );
        // Named as detection every time it is shown: it says which language was
        // heard, not whether the words are right.
        ui::field(
            "Language detected",
            candidate
                .detected_language
                .as_deref()
                .unwrap_or("not reported"),
        );
        ui::field(
            "Diarization",
            &format!(
                "{} segments, {} generic speaker label(s), {} overlapping",
                candidate.segment_count, candidate.speaker_labels, candidate.overlapping_segments
            ),
        );

        println!();
        for line in candidate.text.lines().take(12) {
            ui::info(line);
        }
        let extra = candidate.text.lines().count().saturating_sub(12);
        if extra > 0 {
            ui::hint(&format!("... and {extra} more line(s)"));
        }
    }

    println!();
    ui::hint("Speaker labels are generic, not people. Check names and numbers against the audio.");
    ui::hint("Language detection is not a measure of word accuracy.");
}

fn ask_for_mode(report: &state::SampleReport) -> Result<Option<Mode>> {
    let usable: Vec<&SampleCandidate> = report
        .candidates
        .iter()
        .filter(|candidate| candidate.error.is_none())
        .collect();
    if usable.is_empty() {
        bail!("Neither sample transcription succeeded.");
    }

    let mut choices: Vec<String> = usable
        .iter()
        .map(|candidate| format!("Use `{}`", candidate.mode))
        .collect();
    choices.push("Reject — do not transcribe".to_string());

    let answer = Select::new("Does one of these look good enough?", choices.clone())
        .prompt()
        .context("Nothing was transcribed.")?;

    if answer.starts_with("Reject") {
        return Ok(None);
    }
    let index = choices
        .iter()
        .position(|choice| *choice == answer)
        .unwrap_or(0);
    Ok(usable.get(index).map(|candidate| candidate.mode))
}

async fn run_full(meeting: &state::ActiveMeeting, mode: Mode) -> Result<()> {
    let settings = config::load();
    config::update(|c| c.meeting_mode = Some(mode.to_string()))?;

    let bar = ui::spinner(&format!("Transcribing the full recording in `{mode}`..."));
    let completed = pipeline::finish(meeting, &settings, mode, &|message| {
        nextbase_core::log::log(message);
    })
    .await;
    bar.finish_and_clear();

    let completed = match completed {
        Ok(completed) => completed,
        Err(error) => {
            state::update(|active| {
                active.phase = Phase::Recorded;
                active.error = Some(error.to_string());
            })?;
            ui::hint("The recording is kept. Retry with: nbmeet process");
            return Err(error);
        }
    };

    ui::success("Meeting notes are ready.");
    if completed.partial {
        ui::warn("The transcription job only partly completed; the notes say which parts.");
    }
    if let Some(analysis) = &completed.summary {
        println!();
        ui::field("Title", &analysis.title);
        ui::field("Decisions", &analysis.decisions.len().to_string());
        ui::field("Action items", &analysis.action_items.len().to_string());
        for item in &analysis.action_items {
            let owner = item
                .owner
                .as_deref()
                .map(|owner| format!(" — {owner}"))
                .unwrap_or_default();
            ui::info(&format!(
                "[{}] {}{owner}",
                item.confidence.as_str(),
                item.task
            ));
        }
    } else {
        ui::warn("No summary was produced; the transcript is complete.");
    }

    println!();
    for file in &completed.files {
        ui::field("Wrote", &file.display().to_string());
    }
    Ok(())
}

// -------------------------------------------------------- approve / reject

async fn approve(mode: Option<&str>) -> Result<()> {
    let Some(meeting) = state::load() else {
        bail!("No meeting is waiting for approval.");
    };
    if meeting.phase != Phase::AwaitingApproval {
        bail!(
            "Meeting {} is {}, not waiting for approval.",
            meeting.id,
            meeting.phase
        );
    }

    let mode = match mode {
        Some(name) => Mode::from_name(name)
            .with_context(|| format!("Unknown mode \"{name}\". Use transcribe or codemix."))?,
        None => {
            let report = meeting
                .sample
                .clone()
                .context("This meeting has no sample to approve.")?;
            if !std::io::stdin().is_terminal() {
                bail!("Name the mode: nbmeet approve transcribe|codemix");
            }
            show_sample(&report);
            match ask_for_mode(&report)? {
                Some(mode) => mode,
                None => return reject(),
            }
        }
    };

    run_full(&meeting, mode).await
}

fn reject() -> Result<()> {
    let Some(meeting) = state::load() else {
        bail!("No meeting is waiting for approval.");
    };
    if meeting.phase != Phase::AwaitingApproval {
        bail!("Meeting {} is not waiting for approval.", meeting.id);
    }

    state::update(|active| {
        active.phase = Phase::Recorded;
        active.sample = None;
    })?;
    ui::success("Sample rejected. The recording is kept and nothing was transcribed in full.");
    ui::hint("Try again with: nbmeet process");
    Ok(())
}

// ------------------------------------------------------------ status / etc

fn status() -> Result<()> {
    let Some(meeting) = state::load() else {
        ui::info("No meeting in progress.");
        let unfinished = pipeline::resumable();
        if !unfinished.is_empty() {
            ui::warn(&format!(
                "{} recorded meeting(s) were never transcribed.",
                unfinished.len()
            ));
            ui::hint("Finish the most recent with: nbmeet process");
        }
        return Ok(());
    };

    ui::heading(&format!("Meeting {}", meeting.id));
    ui::field("State", meeting.phase.as_str());
    ui::field("Started", &meeting.started_at);

    if meeting.phase.is_capturing() {
        ui::field("Elapsed", &clock(meeting.elapsed_seconds()));
    }
    if let Some(duration) = meeting.duration_seconds {
        ui::field("Recorded", &clock(duration));
    }
    if let Some(path) = &meeting.audio_path {
        ui::field("Audio", &path.display().to_string());
        if let Ok(info) = wav::info(path) {
            // Read from the header, so a still-recording file shows what is safely
            // on disk rather than what the recorder intends.
            ui::field("On disk", &clock(info.duration_seconds()));
        }
    }
    for level in &meeting.source_levels {
        ui::field(
            &level.source,
            &if level.silent {
                "silent for the whole recording".to_string()
            } else {
                format!("peak {:.3}, RMS {:.4}", level.peak, level.rms)
            },
        );
    }
    if let Some(error) = &meeting.error {
        ui::failure(error);
    }

    match meeting.phase {
        Phase::AwaitingApproval => {
            ui::hint("Approve a mode: nbmeet approve transcribe|codemix");
        }
        Phase::Recorded => ui::hint("Finish it: nbmeet process"),
        Phase::Recording => ui::hint("Stop it: nbmeet stop"),
        _ => {}
    }
    Ok(())
}

/// Transcribe audio that already exists, local or remote.
async fn audio(source: &str) -> Result<()> {
    let source = source.trim();
    if source.is_empty() {
        bail!("Usage: nbmeet audio <path-or-url>");
    }
    nextbase_meeting::check_ready()?;

    if let Some(existing) = state::load() {
        if existing.phase.is_capturing() || existing.phase.is_processing() {
            bail!(
                "Meeting {} is {}. Wait for it, or stop it first.",
                existing.id,
                existing.phase
            );
        }
    }

    // Uploading someone's recording is the same decision as recording one.
    if !confirm_upload(false)? {
        return Ok(());
    }

    let id = nextbase_meeting::new_meeting_id();
    let directory = paths::meeting_dir(&id);

    let bar = ui::spinner(if nextbase_core::import::is_remote(source) {
        "Downloading the audio..."
    } else {
        "Copying the audio..."
    });
    let imported = nextbase_core::import::prepare(source, &directory).await;
    bar.finish_and_clear();
    let imported = imported?;

    let mut meeting = state::ActiveMeeting::new(&id);
    meeting.phase = Phase::Recorded;
    meeting.audio_path = Some(imported.audio.clone());
    meeting.sample_source = imported.sampleable.clone();
    meeting.gate_blocked = imported.gate_blocked.clone();
    meeting.imported = true;
    // Duration comes from the header when it is a WAV; otherwise the provider's
    // timestamps are the only measure, and the note says the duration is unknown.
    meeting.duration_seconds = imported
        .sampleable
        .as_ref()
        .or(Some(&imported.audio))
        .and_then(|path| wav::info(path).ok())
        .map(|info| info.duration_seconds());
    state::save(&meeting)?;

    ui::success(&format!(
        "Imported {}{}.",
        imported.audio.display(),
        meeting
            .duration_seconds
            .map(|seconds| format!(" ({})", clock(seconds)))
            .unwrap_or_default()
    ));
    println!();
    transcribe_and_finish(&meeting).await
}

async fn process(id: Option<&str>) -> Result<()> {
    // Prefer the active meeting; otherwise pick up an orphaned recording.
    if let Some(meeting) = state::load() {
        if meeting.phase == Phase::Recorded && id.is_none() {
            return transcribe_and_finish(&meeting).await;
        }
        if meeting.phase == Phase::AwaitingApproval && id.is_none() {
            return approve(None).await;
        }
    }

    let directory = match id {
        Some(id) => paths::meeting_dir(id),
        None => pipeline::resumable()
            .into_iter()
            .next()
            .context("No recorded meeting is waiting to be transcribed.")?,
    };

    let audio = pipeline::recorded_audio(&directory)
        .with_context(|| format!("No audio found in {}", directory.display()))?;

    let recovered_id = directory
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(nextbase_meeting::new_meeting_id);
    let info = wav::info(&audio).ok();

    let mut meeting = state::ActiveMeeting::new(&recovered_id);
    meeting.phase = Phase::Recorded;
    meeting.audio_path = Some(audio.clone());
    meeting.duration_seconds = info.map(|info| info.duration_seconds());
    let sample_source = directory.join("sample-source.wav");
    if sample_source.is_file() {
        meeting.sample_source = Some(sample_source);
    }
    // Restore the recorded start time when the archived state is still there, so the
    // note does not claim the meeting happened now.
    if let Ok(raw) = std::fs::read_to_string(directory.join("meeting-state.json")) {
        if let Ok(archived) = serde_json::from_str::<state::ActiveMeeting>(&raw) {
            meeting.started_at = archived.started_at;
            meeting.source_levels = archived.source_levels;
        }
    }
    state::save(&meeting)?;

    ui::info(&format!(
        "Picking up {recovered_id}{}.",
        meeting
            .duration_seconds
            .map(|seconds| format!(" ({} of audio)", clock(seconds)))
            .unwrap_or_default()
    ));
    transcribe_and_finish(&meeting).await
}

fn history(limit: Option<usize>) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(paths::meetings_dir()) else {
        ui::info("No meetings yet.");
        return Ok(());
    };

    let mut directories: Vec<std::path::PathBuf> =
        entries.flatten().map(|entry| entry.path()).collect();
    directories.sort();
    directories.reverse();

    if directories.is_empty() {
        ui::info("No meetings yet.");
        return Ok(());
    }

    for directory in directories.into_iter().take(limit.unwrap_or(20)) {
        let id = directory
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let note = directory.join("meeting-note.md");
        let title = std::fs::read_to_string(&note)
            .ok()
            .and_then(|body| {
                body.lines()
                    .next()
                    .map(|line| line.trim_start_matches('#').trim().to_string())
            })
            .unwrap_or_else(|| {
                if directory.join("audio.wav").is_file() {
                    "recorded, not transcribed".to_string()
                } else {
                    "no notes".to_string()
                }
            });
        ui::info(&format!("{id}  {title}"));
    }
    Ok(())
}

fn doctor() -> Result<()> {
    let settings = config::load();

    ui::heading("Keys");
    let sarvam = settings
        .key_for(Provider::Sarvam)
        .filter(|key| !key.is_empty());
    match sarvam {
        Some(_) => ui::success("Sarvam: saved (transcription)"),
        None => {
            ui::failure("Sarvam: missing — meetings cannot be transcribed");
            ui::hint("Add it: nbmeet setup");
        }
    }
    if nextbase_meeting::has_summary_key() {
        ui::success("Groq: saved (summaries)");
    } else {
        ui::warn("Groq: missing — meetings transcribe but are not summarised");
    }

    println!();
    ui::heading("Capture");
    let (mic, system) = settings.meeting_capture();

    if mic {
        let probe = capture::probe(
            SourceKind::Mic,
            settings.audio_device.as_deref(),
            Duration::from_secs(3),
        );
        report_probe(&probe);
    } else {
        ui::warn("Microphone: disabled in config");
    }

    if system {
        match capture::system_audio_status() {
            SystemAudioStatus::Ready => {
                let probe = capture::probe(SourceKind::System, None, Duration::from_secs(3));
                report_probe(&probe);
                if !probe.heard_something() {
                    // Absence of signal is not failure — nothing may be playing.
                    ui::hint("Play some audio and re-run to confirm the far side is captured.");
                }
            }
            SystemAudioStatus::PermissionRequired { hint } => {
                ui::failure("System audio: permission required");
                ui::hint(&hint);
                if std::io::stdin().is_terminal()
                    && Confirm::new("Ask for it now?")
                        .with_default(true)
                        .prompt()
                        .unwrap_or(false)
                {
                    request_system_permission();
                }
            }
            SystemAudioStatus::Unavailable { reason } => {
                ui::failure(&format!("System audio: unavailable — {reason}"));
            }
        }
    } else {
        ui::warn("System audio: disabled in config — the other participants are not recorded");
    }

    println!();
    ui::heading("Settings");
    ui::field(
        "Sample gate",
        if settings.meeting_gate_enabled() {
            "on — every meeting waits for approval"
        } else {
            "off — full transcription runs immediately"
        },
    );
    ui::field(
        "Last approved mode",
        settings.meeting_mode.as_deref().unwrap_or("none yet"),
    );
    ui::field("Meetings", &paths::meetings_dir().display().to_string());
    Ok(())
}

fn report_probe(probe: &capture::SourceProbe) {
    let label = probe.kind.label();
    if !probe.opened {
        ui::failure(&format!(
            "{label}: could not open — {}",
            probe.error.as_deref().unwrap_or("unknown error")
        ));
        return;
    }

    let source = probe.source.as_deref().unwrap_or("unknown source");
    if probe.heard_something() {
        ui::success(&format!(
            "{label}: {source} — heard audio (peak {:.3})",
            probe.levels.peak
        ));
    } else {
        ui::warn(&format!("{label}: {source} — opened but heard nothing"));
    }
}

fn gate(args: &[String]) -> Result<()> {
    let action = args
        .first()
        .map(|a| a.to_lowercase())
        .unwrap_or_else(|| "status".into());

    match action.as_str() {
        "status" => {
            let settings = config::load();
            ui::field(
                "Sample gate",
                if settings.meeting_gate_enabled() {
                    "on"
                } else {
                    "off"
                },
            );
            ui::hint("On: a 3 minute sample is transcribed both ways and waits for your approval.");
            ui::hint(
                "Off: the full recording is transcribed immediately in the last approved mode.",
            );
            Ok(())
        }
        "on" | "enable" | "enabled" => {
            config::update(|c| c.meeting_gate = Some(true))?;
            ui::success("Sample gate on. Every meeting waits for approval before the full run.");
            Ok(())
        }
        "off" | "disable" | "disabled" => {
            config::update(|c| c.meeting_gate = Some(false))?;
            ui::success("Sample gate off.");
            // Say what is being given up, rather than only confirming.
            ui::warn(
                "A bad transcription mode will now produce confident-looking notes unchecked.",
            );
            Ok(())
        }
        other => bail!("Usage: nbmeet gate on|off|status (got \"{other}\")"),
    }
}
