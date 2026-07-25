//! The background listener: shortcuts in, transcript pasted out.
//!
//! Structure carried over from the TypeScript listener, including the fixes it
//! needed: one listener per machine, a shortcut that cannot be registered never
//! takes down the others, and shortcuts are compared normalized so the same combo
//! written two ways is not registered twice.

use anyhow::Result;
use nextbase_core::config::Config;
use nextbase_core::hotkey::{self, HotkeyEvent};
use nextbase_core::{audio, config, log, paste, polish, process_state, shortcut, storage, transcribe};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// SIGTERM is how `wisper stop` and launchd ask the listener to exit, so it has to
/// unwind the same way Ctrl+C does: release the microphone, clear the PID file.
struct Terminate {
    #[cfg(unix)]
    inner: tokio::signal::unix::Signal,
}

impl Terminate {
    fn new() -> Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                inner: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        {
            self.inner.recv().await;
        }
        #[cfg(not(unix))]
        {
            std::future::pending::<()>().await;
        }
    }
}

/// Anything shorter than this is a mis-tap rather than speech.
const MIN_RECORDING: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    Dictation(HotkeyEvent),
    PolishSelection,
    SpellFocusedInput,
}

pub async fn run() -> Result<()> {
    // Last listener wins. Without this, an autostart-revived copy and a manually
    // started one both stay registered and one press fires twice.
    let stopped = process_state::stop_other_listeners();
    if stopped > 0 {
        log::log(&format!("Stopped {stopped} listener(s) already running."));
    }
    process_state::write_pid()?;

    let config = config::load();
    let dictation = config.shortcut_or_default().to_string();

    log::log("Wisper listener running.");
    log::log(&format!(
        "Provider: {}",
        config
            .provider
            .map(|p| p.to_string())
            .unwrap_or_else(|| "not set".into())
    ));
    log::log(&format!(
        "Model: {}",
        config.model.as_deref().unwrap_or("not set")
    ));
    log::log(&format!("Shortcut: {dictation}"));

    if !hotkey::has_permission() {
        log::log(&format!(
            "Accessibility permission is missing. {}",
            hotkey::permission_hint()
        ));
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Job>();

    // Registration failures are logged and skipped. One bad shortcut used to throw
    // and kill the listener, which autostart then restarted in a silent loop.
    let register = |label: &'static str, value: &str, job: fn(HotkeyEvent) -> Option<Job>| {
        let tx = tx.clone();
        match hotkey::listen(value, move |event| {
            if let Some(job) = job(event) {
                let _ = tx.send(job);
            }
        }) {
            Ok(handle) => {
                log::log(&format!("Shortcut registered: {value}"));
                Some(handle)
            }
            Err(error) => {
                log::log(&format!(
                    "{label} shortcut \"{value}\" could not be registered: {error}"
                ));
                None
            }
        }
    };

    let dictation_key = shortcut::normalize(&dictation);
    let handles = vec![
        register("Dictation", &dictation, |event| Some(Job::Dictation(event))),
        {
            let polish_shortcut = config.polish_shortcut_or_default().to_string();
            let key = shortcut::normalize(&polish_shortcut);
            if key == dictation_key {
                log::log(&format!(
                    "Polish shortcut {polish_shortcut} matches the dictation shortcut. Skipped."
                ));
                None
            } else {
                // Only the press matters; ignore the release.
                register("Polish", &polish_shortcut, |event| {
                    matches!(event, HotkeyEvent::Down).then_some(Job::PolishSelection)
                })
            }
        },
        {
            let spell_shortcut = config.spell_shortcut_or_default().to_string();
            let key = shortcut::normalize(&spell_shortcut);
            let polish_key = shortcut::normalize(config.polish_shortcut_or_default());
            if key == dictation_key || key == polish_key {
                log::log(&format!(
                    "Spell-fix shortcut {spell_shortcut} matches another shortcut. Skipped."
                ));
                None
            } else {
                register("Spell-fix", &spell_shortcut, |event| {
                    matches!(event, HotkeyEvent::Down).then_some(Job::SpellFocusedInput)
                })
            }
        },
    ];

    if handles.iter().all(Option::is_none) {
        process_state::clear_pid();
        anyhow::bail!("No shortcuts could be registered. Nothing to listen for.");
    }

    log::log("Hold the shortcut to record, release to transcribe. Ctrl+C stops the listener.");

    let mut recording: Option<audio::Recording> = None;
    let mut terminate = Terminate::new()?;

    loop {
        let job = tokio::select! {
            job = rx.recv() => match job {
                Some(job) => job,
                None => break,
            },
            _ = tokio::signal::ctrl_c() => {
                log::log("Stopping Wisper listener.");
                break;
            }
            _ = terminate.recv() => {
                log::log("Stopping Wisper listener (SIGTERM).");
                break;
            }
        };

        match job {
            Job::Dictation(HotkeyEvent::Down) => {
                if recording.is_some() {
                    continue;
                }
                match start_recording(&config) {
                    Ok(active) => recording = Some(active),
                    Err(error) => log::log(&format!("Could not start recording: {error}")),
                }
            }
            Job::Dictation(HotkeyEvent::Up) => {
                let Some(active) = recording.take() else { continue };
                if let Err(error) = finish_recording(active).await {
                    log::log(&format!("Error: {error}"));
                }
                drain(&mut rx);
            }
            Job::PolishSelection => {
                if recording.is_some() {
                    log::log("Ignoring polish shortcut while recording.");
                    continue;
                }
                if let Err(error) = rewrite_selection(polish::RewriteMode::Polish).await {
                    log::log(&format!("Polish error: {error}"));
                }
                drain(&mut rx);
            }
            Job::SpellFocusedInput => {
                if recording.is_some() {
                    log::log("Ignoring spell-fix shortcut while recording.");
                    continue;
                }
                if let Err(error) = rewrite_focused_input().await {
                    log::log(&format!("Spell-fix error: {error}"));
                }
                drain(&mut rx);
            }
        }
    }

    if let Some(active) = recording.take() {
        active.cancel();
    }
    for handle in handles.into_iter().flatten() {
        handle.stop();
    }
    process_state::clear_pid();
    Ok(())
}

/// Discard shortcut presses that arrived while a long job was running.
///
/// Without this, holding the shortcut during a slow transcription queues presses
/// that then replay as phantom recordings. Only called after the long jobs, never
/// straight after a press, so a real release is never dropped.
fn drain(rx: &mut mpsc::UnboundedReceiver<Job>) {
    let mut dropped = 0;
    while rx.try_recv().is_ok() {
        dropped += 1;
    }
    if dropped > 0 {
        log::log(&format!("Ignored {dropped} shortcut event(s) received while busy."));
    }
}

fn start_recording(config: &Config) -> Result<audio::Recording> {
    let device = config.audio_device.as_deref();
    let path = audio::new_recording_path()?;
    log::log(&format!(
        "Recording from {}... release the shortcut to stop.",
        device.unwrap_or(audio::DEFAULT_DEVICE)
    ));
    audio::start(device, path)
}

async fn finish_recording(active: audio::Recording) -> Result<()> {
    let total = Instant::now();
    let finished = active.stop()?;

    if finished.duration < MIN_RECORDING {
        let _ = std::fs::remove_file(&finished.path);
        anyhow::bail!("Recording too short. Hold the shortcut while speaking, then release.");
    }

    log::log(&format!(
        "Audio level: peak {:.5}, RMS {:.5}.",
        finished.levels.peak, finished.levels.rms
    ));
    if finished.levels.is_silent() {
        let _ = std::fs::remove_file(&finished.path);
        anyhow::bail!(
            "Recording appears silent. Check System Settings > Sound > Input, and that this binary has microphone permission."
        );
    }
    log::log(&format!(
        "Recorded {:.1}s audio.",
        finished.duration.as_secs_f32()
    ));

    // Reload: settings may have changed since the listener started.
    let config = config::load();

    log::log("Sending audio to transcription provider...");
    let transcribe_start = Instant::now();
    let text = transcribe::transcribe_file(&finished.path, &config).await?;
    let transcribe_ms = transcribe_start.elapsed().as_millis();
    if text.trim().is_empty() {
        anyhow::bail!("Empty transcript returned.");
    }

    let polish_start = Instant::now();
    if config.auto_polish == Some(true) {
        log::log("Polishing dictated text before paste...");
    }
    let final_text = polish::polish_dictation_if_enabled(&text, &config).await;
    let polish_ms = polish_start.elapsed().as_millis();

    let save_start = Instant::now();
    storage::save_transcript(&final_text, &finished.path.display().to_string())?;
    let save_ms = save_start.elapsed().as_millis();

    let paste_start = Instant::now();
    paste::paste_into_active_app(&final_text)?;
    let paste_ms = paste_start.elapsed().as_millis();

    let _ = audio::cleanup_old_recordings(30, Duration::from_secs(24 * 60 * 60));

    log::log(&format!(
        "Timing: transcribe {transcribe_ms}ms, polish {polish_ms}ms, save {save_ms}ms, paste {paste_ms}ms, total {}ms.",
        total.elapsed().as_millis()
    ));
    log::log(&format!("Inserted: {final_text}"));
    Ok(())
}

async fn rewrite_selection(mode: polish::RewriteMode) -> Result<()> {
    log::log("Polishing selected text...");
    let selected = paste::copy_selected_text()?;
    if selected.is_empty() {
        anyhow::bail!("Select text first, then press the polish shortcut.");
    }

    let config = config::load();
    let polished = polish::rewrite_text(&selected, &config, mode).await?;
    paste::paste_into_active_app(&polished)?;
    storage::save_transcript(&polished, "polish-shortcut")?;
    log::log("Selected text polished and replaced.");
    Ok(())
}

async fn rewrite_focused_input() -> Result<()> {
    log::log("Fixing spelling in focused input...");
    let text = paste::copy_focused_input_text()?;
    if text.is_empty() {
        anyhow::bail!("Focus an editable text field with content first, then press the spell-fix shortcut.");
    }

    let config = config::load();
    let fixed = polish::rewrite_text(&text, &config, polish::RewriteMode::Spell).await?;
    paste::paste_into_active_app(&fixed)?;
    storage::save_transcript(&fixed, "spell-shortcut")?;
    log::log("Focused input spelling fixed and replaced.");
    Ok(())
}
