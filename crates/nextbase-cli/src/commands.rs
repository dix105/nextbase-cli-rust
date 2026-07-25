use anyhow::{bail, Context, Result};
use inquire::{Confirm, InquireError, Password, PasswordDisplayMode, Select, Text};
use nextbase_core::config::{
    self, Provider, DEFAULT_DUCKING_VOLUME, DEFAULT_POLISH_MODEL, DEFAULT_POLISH_SHORTCUT,
    DEFAULT_SHORTCUT, DEFAULT_SPELL_SHORTCUT, DEFAULT_UPDATE_INTERVAL_MINUTES, MODEL_OPTIONS,
};
use nextbase_core::polish::{self, RewriteMode};
use nextbase_core::{audio, hotkey, log, process_state, shortcut, storage, transcribe, verify};
use std::io::IsTerminal;

use crate::ui;

/// Commands whose platform layer has not been ported yet fail loudly with the
/// phase that will bring them, instead of pretending to work.
fn not_yet(what: &str, phase: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "`{what}` is not in the Rust build yet ({phase}). Use the current CLI for it in the meantime."
    )
}

fn require_interactive(what: &str) -> Result<()> {
    if std::io::stdin().is_terminal() {
        return Ok(());
    }
    bail!("{what} needs an interactive terminal. Run it directly in your shell.")
}

/// Ctrl-C and Esc are normal ways to leave a wizard, not errors to dump.
fn prompt_error(error: InquireError) -> anyhow::Error {
    match error {
        InquireError::OperationCanceled | InquireError::OperationInterrupted => {
            anyhow::anyhow!("Setup cancelled. Nothing was saved for this step.")
        }
        other => anyhow::anyhow!(other),
    }
}

// ---------------------------------------------------------------- read-only

pub fn status() -> Result<()> {
    let config = config::load();
    ui::heading("Wisper setup");
    ui::field("Provider", config.provider.map(|p| p.to_string()).as_deref().unwrap_or("not set"));
    ui::field("Model", config.model.as_deref().unwrap_or("not set"));
    ui::field("Shortcut", config.shortcut.as_deref().unwrap_or("not set"));
    ui::field("Microphone", config.audio_device.as_deref().unwrap_or("default"));
    ui::field(
        "API key",
        if config.is_configured() { "saved" } else { "not set" },
    );
    ui::field(
        "Auto polish",
        if config.auto_polish.unwrap_or(false) { "enabled" } else { "disabled" },
    );
    ui::field("Polish shortcut", config.polish_shortcut_or_default());
    ui::field("Spell-fix shortcut", config.spell_shortcut_or_default());
    ui::field(
        "Audio ducking",
        &match config.audio_ducking {
            Some(false) => "disabled".to_string(),
            _ => format!(
                "enabled at {}%",
                config.audio_ducking_volume.unwrap_or(DEFAULT_DUCKING_VOLUME)
            ),
        },
    );
    ui::field(
        "Autostart",
        if config.autostart.unwrap_or(false) { "enabled" } else { "not enabled" },
    );
    ui::field(
        "Auto update",
        if config.auto_update == Some(false) { "disabled" } else { "enabled" },
    );
    Ok(())
}

pub fn shortcuts() -> Result<()> {
    let config = config::load();
    ui::heading("Shortcuts");
    ui::field("Dictation", config.shortcut_or_default());
    ui::field("Polish selection", config.polish_shortcut_or_default());
    ui::field("Spell-fix input", config.spell_shortcut_or_default());

    println!();
    ui::heading("Supported keys");
    ui::info("Windows: A-Z, 0-9, Space, Tab, Enter, Esc, F1-F24");
    ui::info("macOS:   A-Z, 0-9, Space, Tab, Enter, Esc, F1-F20");
    ui::info("Both also accept modifier-only combos, e.g. Ctrl+Command");

    println!();
    ui::heading("Examples");
    ui::info("wisper shortcut F15");
    ui::info("wisper shortcut Ctrl+Alt+Space");
    ui::info("wisper polish shortcut F16");
    ui::info("wisper spell shortcut CommandOrControl+Alt+S");
    println!();
    ui::hint("F13-F24 often cannot be captured inside a terminal. Type them directly.");
    Ok(())
}

pub fn history(limit: Option<usize>) -> Result<()> {
    let history = storage::load_history();
    if history.is_empty() {
        ui::info("No transcripts yet.");
        return Ok(());
    }
    for item in history.iter().take(limit.unwrap_or(20)) {
        anstream::println!("{}  {}", item.created_at, item.text);
    }
    Ok(())
}

pub fn add(text: &[String]) -> Result<()> {
    let text = text.join(" ").trim().to_string();
    if text.is_empty() {
        bail!("Usage: wisper add \"text\"");
    }
    let item = storage::save_transcript(&text, "manual")?;
    ui::success(&format!("Saved transcript {}", item.id));
    Ok(())
}

pub fn logs() -> Result<()> {
    print!("{}", log::read_logs());
    Ok(())
}

// ---------------------------------------------------------------- shortcuts

/// Shared by all three setters: validate before writing, so an unregisterable key
/// can never reach config and break the listener at its next start.
fn store_shortcut(kind: &str, value: &str) -> Result<()> {
    shortcut::validate(value)?;
    let value = value.to_string();
    match kind {
        "dictation" => config::update(|c| c.shortcut = Some(value.clone()))?,
        "polish" => config::update(|c| c.polish_shortcut = Some(value.clone()))?,
        "spell" => config::update(|c| c.spell_shortcut = Some(value.clone()))?,
        other => bail!("Unknown shortcut kind: {other}"),
    };
    ui::success(&format!("{kind} shortcut set to {value}."));
    ui::hint("Restart the listener to pick it up: wisper restart");
    Ok(())
}

fn ask_shortcut(label: &str, current: &str) -> Result<String> {
    require_interactive("Setting a shortcut interactively")?;
    ui::hint("Live key capture arrives with the listener; type the combo for now.");
    Text::new(&format!("{label} shortcut:"))
        .with_default(current)
        .with_help_message("e.g. F15, Ctrl+Alt+Space, CommandOrControl+Shift+P")
        .with_validator(|input: &str| {
            Ok(match shortcut::validate(input) {
                Ok(()) => inquire::validator::Validation::Valid,
                Err(error) => {
                    inquire::validator::Validation::Invalid(error.to_string().into())
                }
            })
        })
        .prompt()
        .map_err(prompt_error)
}

pub fn set_shortcut(keys: &[String]) -> Result<()> {
    let direct = keys.join("+").trim().to_string();
    let value = if direct.is_empty() {
        let config = config::load();
        ask_shortcut("Dictation", config.shortcut_or_default())?
    } else {
        direct
    };
    store_shortcut("dictation", &value)
}

// ---------------------------------------------------------------- polish/spell

pub async fn polish(args: &[String]) -> Result<()> {
    let action = args.first().map(|a| a.to_lowercase()).unwrap_or_default();

    match action.as_str() {
        "" | "status" => {
            let config = config::load();
            ui::field(
                "Auto polish",
                if config.auto_polish.unwrap_or(false) { "enabled" } else { "disabled" },
            );
            ui::field("Polish model", config.polish_model_or_default());
            ui::field("Polish shortcut", config.polish_shortcut_or_default());
            ui::field(
                "Groq key",
                if config.key_for(Provider::Groq).is_some() { "saved" } else { "not set" },
            );
            Ok(())
        }
        "shortcut" => {
            let direct = args[1..].join("+").trim().to_string();
            let value = if direct.is_empty() {
                ask_shortcut("Polish", config::load().polish_shortcut_or_default())?
            } else {
                direct
            };
            store_shortcut("polish", &value)
        }
        "on" | "enable" | "enabled" => {
            let config = config::load();
            if config.key_for(Provider::Groq).is_none() {
                let key = ask_provider_key(Provider::Groq).await?;
                config::update(|c| c.set_key(Provider::Groq, key))?;
            }
            config::update(|c| {
                c.auto_polish = Some(true);
                c.polish_model = Some(DEFAULT_POLISH_MODEL.to_string());
            })?;
            ui::success("Auto polish enabled. Dictation will be polished before paste.");
            ui::hint("Restart the listener to pick it up: wisper restart");
            Ok(())
        }
        "off" | "disable" | "disabled" => {
            config::update(|c| c.auto_polish = Some(false))?;
            ui::success("Auto polish disabled.");
            ui::hint("Restart the listener to pick it up: wisper restart");
            Ok(())
        }
        _ => {
            // `polish <mode> "text"` or just `polish "text"`, matching the
            // existing CLI: an unrecognised first word is part of the text.
            let (mode, words) = match RewriteMode::from_name(&action) {
                Some(mode) => (mode, &args[1..]),
                None => (RewriteMode::Polish, args),
            };
            let text = words.join(" ").trim().to_string();
            if text.is_empty() {
                bail!("Usage: wisper polish \"text\" or wisper polish on|off|status|shortcut");
            }
            rewrite_and_print(&text, mode).await
        }
    }
}

async fn rewrite_and_print(text: &str, mode: RewriteMode) -> Result<()> {
    let config = config::load();
    let bar = ui::spinner("Rewriting...");
    let result = polish::rewrite_text(text, &config, mode).await;
    bar.finish_and_clear();
    anstream::println!("{}", result?);
    Ok(())
}

pub async fn spell(args: &[String]) -> Result<()> {
    let action = args.first().map(|a| a.to_lowercase()).unwrap_or_default();

    match action.as_str() {
        "" | "status" => {
            let config = config::load();
            ui::field("Spell-fix shortcut", config.spell_shortcut_or_default());
            ui::info("Selects all text in the focused input, fixes spelling only, replaces it.");
            Ok(())
        }
        "shortcut" => {
            let direct = args[1..].join("+").trim().to_string();
            let value = if direct.is_empty() {
                ask_shortcut("Spell-fix", config::load().spell_shortcut_or_default())?
            } else {
                direct
            };
            store_shortcut("spell", &value)
        }
        _ => {
            let text = args.join(" ").trim().to_string();
            if text.is_empty() {
                bail!("Usage: wisper spell \"text\" or wisper spell shortcut [key]");
            }
            rewrite_and_print(&text, RewriteMode::Spell).await
        }
    }
}

// ---------------------------------------------------------------- keys

/// Ask, verify, and only accept a key that actually works.
///
/// The TypeScript setup printed the verification failure and saved the key
/// anyway, so a typo surfaced much later as a failed dictation.
async fn ask_provider_key(provider: Provider) -> Result<String> {
    require_interactive("Entering an API key")?;

    loop {
        let key = Password::new(&format!("{}:", provider.key_prompt()))
            .with_display_mode(PasswordDisplayMode::Masked)
            .without_confirmation()
            .with_help_message("Paste the key — it stays hidden and is saved locally")
            .prompt()
            .map_err(prompt_error)?;

        let bar = ui::spinner(&format!("Verifying {provider} key..."));
        let result = verify::verify_provider_key(provider, &key).await;
        bar.finish_and_clear();

        if result.ok {
            ui::success(&result.message);
            return Ok(key);
        }

        ui::failure(&result.message);
        let retry = Confirm::new("Try a different key?")
            .with_default(true)
            .prompt()
            .map_err(prompt_error)?;
        if !retry {
            bail!("Setup stopped: no verified key for {provider}.");
        }
    }
}

pub async fn provider() -> Result<()> {
    require_interactive("Choosing a provider")?;

    let labels: Vec<String> = Provider::ALL.iter().map(|p| p.to_string()).collect();
    let chosen = Select::new("Select provider:", labels)
        .prompt()
        .map_err(prompt_error)?;
    let provider: Provider = chosen.parse()?;

    let key = ask_provider_key(provider).await?;
    config::update(|c| {
        c.provider = Some(provider);
        c.set_key(provider, key);
    })?;
    ui::success(&format!("Provider set to {provider}."));
    Ok(())
}

// ---------------------------------------------------------------- setup

pub async fn setup(update_mode: bool) -> Result<()> {
    require_interactive("wisper setup")?;

    ui::heading(if update_mode { "Wisper update setup" } else { "Wisper setup" });
    if update_mode {
        ui::hint("Only missing settings are asked for. Existing ones are kept.");
    }
    println!();

    let mut config = config::load();
    let mut kept: Vec<String> = Vec::new();

    // 1. Model + key
    if config.provider.is_none() || config.model.is_none() || !config.is_configured() {
        let labels: Vec<String> = MODEL_OPTIONS.iter().map(|o| o.label.to_string()).collect();
        let chosen = Select::new("Select model:", labels)
            .with_help_message("Transcription model used for dictation")
            .prompt()
            .map_err(prompt_error)?;
        let option = MODEL_OPTIONS
            .iter()
            .find(|o| o.label == chosen)
            .context("Unknown model selection")?;

        let key = ask_provider_key(option.provider).await?;
        config = config::update(|c| {
            c.provider = Some(option.provider);
            c.model = Some(option.model.to_string());
            c.set_key(option.provider, key);
        })?;
    } else {
        kept.push(format!(
            "model {}",
            config.model.as_deref().unwrap_or("unknown")
        ));
    }

    // 2. Dictation shortcut
    if config.shortcut.is_none() {
        let value = ask_shortcut("Dictation", DEFAULT_SHORTCUT)?;
        shortcut::validate(&value)?;
        config = config::update(|c| c.shortcut = Some(value.clone()))?;
    } else {
        kept.push(format!("shortcut {}", config.shortcut_or_default()));
    }

    // 3. Auto polish
    if config.auto_polish.is_none() {
        let wants = Confirm::new("Auto polish dictated text before paste?")
            .with_default(false)
            .with_help_message("Cleans up grammar and punctuation with Groq before pasting")
            .prompt()
            .map_err(prompt_error)?;

        if wants {
            if config.key_for(Provider::Groq).is_none() {
                let key = ask_provider_key(Provider::Groq).await?;
                config::update(|c| c.set_key(Provider::Groq, key))?;
            }
            config = config::update(|c| {
                c.auto_polish = Some(true);
                c.polish_model = Some(DEFAULT_POLISH_MODEL.to_string());
            })?;
        } else {
            config = config::update(|c| c.auto_polish = Some(false))?;
        }
    } else {
        kept.push(format!(
            "auto polish {}",
            if config.auto_polish == Some(true) { "on" } else { "off" }
        ));
    }

    // 4. Shortcut defaults that were previously announced rather than asked.
    if config.polish_shortcut.is_none() {
        config = config::update(|c| c.polish_shortcut = Some(DEFAULT_POLISH_SHORTCUT.to_string()))?;
    }
    if config.spell_shortcut.is_none() {
        config = config::update(|c| c.spell_shortcut = Some(DEFAULT_SPELL_SHORTCUT.to_string()))?;
    }

    // 5. Ducking
    if config.audio_ducking.is_none() {
        let wants = Confirm::new("Lower system volume while recording?")
            .with_default(true)
            .prompt()
            .map_err(prompt_error)?;
        config = config::update(|c| {
            c.audio_ducking = Some(wants);
            if wants {
                c.audio_ducking_volume = Some(DEFAULT_DUCKING_VOLUME);
            }
        })?;
    } else {
        kept.push("audio ducking".to_string());
    }

    // 6. Auto update
    if config.auto_update.is_none() {
        config = config::update(|c| {
            c.auto_update = Some(true);
            c.auto_update_interval_minutes = Some(DEFAULT_UPDATE_INTERVAL_MINUTES);
        })?;
    } else {
        kept.push("auto update".to_string());
    }

    // 7. Autostart
    if config.autostart.is_none() {
        let wants = Confirm::new("Start Wisper automatically at login?")
            .with_default(true)
            .prompt()
            .map_err(prompt_error)?;
        config::update(|c| c.autostart = Some(wants))?;
    } else {
        kept.push(format!(
            "autostart {}",
            if config.autostart == Some(true) { "on" } else { "off" }
        ));
    }

    if !kept.is_empty() {
        println!();
        ui::hint(&format!("Kept: {}.", kept.join(", ")));
    }

    println!();
    status()?;
    println!();
    ui::warn("The listener is not in the Rust build yet (phase 3-4).");
    ui::hint("Start dictation with the current CLI: wisper listen");
    Ok(())
}

// ---------------------------------------------------------------- not yet

pub async fn transcribe(file: &str) -> Result<()> {
    let path = std::path::Path::new(file);
    if !path.is_file() {
        bail!("Audio file not found: {file}");
    }

    let config = config::load();
    let provider = config
        .provider
        .map(|p| p.to_string())
        .unwrap_or_else(|| "provider".to_string());

    let bar = ui::spinner(&format!("Transcribing with {provider}..."));
    let result = transcribe::transcribe_file(path, &config).await;
    bar.finish_and_clear();

    let text = result?;
    if text.is_empty() {
        bail!("Empty transcript returned.");
    }

    storage::save_transcript(&text, file)?;
    anstream::println!("{text}");
    Ok(())
}

pub fn media(_args: &[String]) -> Result<()> {
    Err(not_yet("wisper media", "phase 3: platform layer"))
}

pub fn autostart(_args: &[String]) -> Result<()> {
    Err(not_yet("wisper autostart", "phase 4: autostart"))
}

pub fn autoupdate(_args: &[String]) -> Result<()> {
    Err(not_yet("wisper autoupdate", "phase 7: releases"))
}

pub fn mic(auto: bool) -> Result<()> {
    if auto {
        return auto_select_mic(false);
    }

    require_interactive("Choosing a microphone")?;
    let mut devices = vec![audio::DEFAULT_DEVICE.to_string()];
    devices.extend(audio::list_input_devices());
    if devices.len() == 1 {
        bail!("No microphone devices found.");
    }

    let chosen = Select::new("Select microphone:", devices)
        .prompt()
        .map_err(prompt_error)?;
    config::update(|c| c.audio_device = Some(chosen.clone()))?;
    ui::success(&format!("Microphone set to {chosen}."));
    ui::hint("Restart the listener to pick it up: wisper restart");
    Ok(())
}

/// Probe every input and keep the one that actually hears something.
pub fn auto_select_mic(quiet: bool) -> Result<()> {
    let configured = config::load().audio_device;
    let bar = ui::spinner("Testing microphones...");
    let result = audio::auto_detect_input_device(configured.as_deref());
    bar.finish_and_clear();

    config::update(|c| c.audio_device = Some(result.device.clone()))?;

    let chosen = result
        .probes
        .iter()
        .find(|probe| probe.device == result.device);
    match chosen {
        Some(probe) => ui::success(&format!(
            "Microphone set to {} (signal {:.5}{}).",
            result.device,
            probe.score,
            if probe.has_signal { "" } else { ", silent during test" }
        )),
        None => ui::success(&format!("Microphone set to {}.", result.device)),
    }

    if !quiet {
        for probe in result.probes.iter().filter(|p| !p.ok) {
            ui::warn(&format!(
                "Skipped {}: {}",
                probe.device,
                probe.error.as_deref().unwrap_or("could not be opened")
            ));
        }
    }
    Ok(())
}

/// Capture without the hotkey. The quickest way to tell a permission problem from
/// a device problem.
pub fn record(seconds: Option<u64>) -> Result<()> {
    let seconds = seconds.unwrap_or(3).clamp(1, 120);
    let config = config::load();
    let device = config.audio_device.as_deref();
    let path = audio::new_recording_path()?;

    ui::info(&format!(
        "Recording {seconds}s from {}...",
        device.unwrap_or(audio::DEFAULT_DEVICE)
    ));
    let recording = audio::start(device, path)?;
    std::thread::sleep(std::time::Duration::from_secs(seconds));
    let finished = recording.stop()?;

    ui::field("File", &finished.path.display().to_string());
    ui::field("Duration", &format!("{:.1}s", finished.duration.as_secs_f32()));
    ui::field(
        "Levels",
        &format!(
            "peak {:.5}, RMS {:.5}",
            finished.levels.peak, finished.levels.rms
        ),
    );

    if finished.levels.is_silent() {
        ui::warn("Recording is silent. Check the input in System Settings > Sound, and that your terminal has microphone permission.");
    } else {
        ui::success("Microphone is working.");
    }
    Ok(())
}

pub async fn listen(foreground: bool) -> Result<()> {
    if foreground {
        return crate::listener::run().await;
    }
    // Detaching is part of phase 4; running in the foreground is the honest
    // behaviour until then.
    ui::warn("Detached start is not wired yet. Running in the foreground.");
    ui::hint("Press Ctrl+C to stop.");
    crate::listener::run().await
}

pub fn stop() -> Result<()> {
    let stopped = process_state::stop_other_listeners();
    if stopped > 0 {
        ui::success(&format!("Stopped {stopped} listener(s)."));
    } else {
        ui::info("No running listener found.");
    }
    Ok(())
}

pub async fn restart() -> Result<()> {
    let stopped = process_state::stop_other_listeners();
    if stopped > 0 {
        ui::info(&format!("Stopped {stopped} listener(s)."));
    }
    listen(true).await
}

/// One place to see why dictation is not working. Permission problems are this
/// tool's most common failure, and they are invisible from a detached listener.
pub fn doctor() -> Result<()> {
    let config = config::load();

    ui::heading("Permissions");
    if hotkey::has_permission() {
        ui::success("Accessibility: granted (global shortcuts can be registered)");
    } else {
        ui::failure("Accessibility: missing");
        ui::hint(hotkey::permission_hint());
    }

    println!();
    ui::heading("Microphone");
    let devices = audio::list_input_devices();
    if devices.is_empty() {
        ui::failure("No input devices found");
    } else {
        for device in &devices {
            ui::info(&format!(
                "{device}{}",
                if audio::is_likely_virtual(device) { "  (virtual)" } else { "" }
            ));
        }
        ui::field("Configured", config.audio_device.as_deref().unwrap_or("default"));
    }

    println!();
    ui::heading("Shortcuts");
    let dictation = shortcut::normalize(config.shortcut_or_default());
    for (label, value) in [
        ("Dictation", config.shortcut_or_default()),
        ("Polish", config.polish_shortcut_or_default()),
        ("Spell-fix", config.spell_shortcut_or_default()),
    ] {
        match shortcut::validate(value) {
            Ok(()) => {
                let clash = label != "Dictation" && shortcut::normalize(value) == dictation;
                if clash {
                    ui::warn(&format!("{label}: {value} is the same combo as the dictation shortcut"));
                } else {
                    ui::success(&format!("{label}: {value}"));
                }
            }
            Err(error) => ui::failure(&format!("{label}: {value} — {error}")),
        }
    }

    println!();
    ui::heading("Provider");
    if config.is_configured() {
        ui::success(&format!(
            "{} / {}",
            config.provider.map(|p| p.to_string()).unwrap_or_default(),
            config.model.as_deref().unwrap_or("default model")
        ));
    } else {
        ui::failure("No provider or API key. Run: wisper setup");
    }

    println!();
    ui::heading("Listener");
    let others = process_state::other_listener_pids();
    match others.len() {
        0 => ui::info("Not running."),
        1 => ui::success(&format!("Running (pid {}).", others[0])),
        n => ui::failure(&format!(
            "{n} listeners running ({others:?}). Every shortcut press fires {n} times. Run: wisper stop"
        )),
    }
    Ok(())
}

pub fn open(_port: Option<u16>) -> Result<()> {
    Err(not_yet("wisper open", "phase 5: dashboard"))
}
