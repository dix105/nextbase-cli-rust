use anyhow::{bail, Context, Result};
use inquire::{Confirm, InquireError, Password, PasswordDisplayMode, Select, Text};
use nextbase_core::config::{
    self, Provider, DEFAULT_DUCKING_VOLUME, DEFAULT_POLISH_MODEL, DEFAULT_POLISH_SHORTCUT,
    DEFAULT_SHORTCUT, DEFAULT_SPELL_SHORTCUT, DEFAULT_UPDATE_INTERVAL_MINUTES,
    MIN_UPDATE_INTERVAL_MINUTES, MODEL_OPTIONS,
};
use nextbase_core::polish::{self, RewriteMode};
use nextbase_core::{
    audio, autostart, hotkey, log, media, process_state, shortcut, storage, transcribe, updater,
    verify,
};
use std::io::IsTerminal;

use crate::ui;

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
    ui::field(
        "Provider",
        config
            .provider
            .map(|p| p.to_string())
            .as_deref()
            .unwrap_or("not set"),
    );
    ui::field("Model", config.model.as_deref().unwrap_or("not set"));
    ui::field("Shortcut", config.shortcut.as_deref().unwrap_or("not set"));
    ui::field(
        "Microphone",
        config.audio_device.as_deref().unwrap_or("default"),
    );
    ui::field(
        "API key",
        if config.is_configured() {
            "saved"
        } else {
            "not set"
        },
    );
    ui::field(
        "Auto polish",
        if config.auto_polish.unwrap_or(false) {
            "enabled"
        } else {
            "disabled"
        },
    );
    ui::field("Polish shortcut", config.polish_shortcut_or_default());
    ui::field("Spell-fix shortcut", config.spell_shortcut_or_default());
    ui::field(
        "Audio ducking",
        &match config.audio_ducking {
            Some(false) => "disabled".to_string(),
            _ => format!(
                "enabled at {}%",
                config
                    .audio_ducking_volume
                    .unwrap_or(DEFAULT_DUCKING_VOLUME)
            ),
        },
    );
    ui::field(
        "Autostart",
        if config.autostart.unwrap_or(false) {
            "enabled"
        } else {
            "not enabled"
        },
    );
    ui::field(
        "Auto update",
        if config.auto_update == Some(false) {
            "disabled"
        } else {
            "enabled"
        },
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

    // Press-to-capture is the good path. Esc inside it means "let me type it",
    // which is also the only way to enter a modifier-only combo, and the way out
    // when a terminal swallows F13-F24.
    match crate::tui::capture_shortcut(label, current) {
        Ok(Some(captured)) => {
            ui::success(&format!("Captured {captured}."));
            return Ok(captured);
        }
        Ok(None) => {}
        Err(error) => ui::warn(&format!(
            "Key capture unavailable ({error}). Type it instead."
        )),
    }

    type_shortcut(label, current)
}

fn type_shortcut(label: &str, current: &str) -> Result<String> {
    ui::hint("Modifier-only combos like Ctrl+Command can only be typed.");
    Text::new(&format!("{label} shortcut:"))
        .with_default(current)
        .with_help_message("e.g. F15, Ctrl+Alt+Space, CommandOrControl+Shift+P")
        .with_validator(|input: &str| {
            Ok(match shortcut::validate(input) {
                Ok(()) => inquire::validator::Validation::Valid,
                Err(error) => inquire::validator::Validation::Invalid(error.to_string().into()),
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
                if config.auto_polish.unwrap_or(false) {
                    "enabled"
                } else {
                    "disabled"
                },
            );
            ui::field("Polish model", config.polish_model_or_default());
            ui::field("Polish shortcut", config.polish_shortcut_or_default());
            ui::field(
                "Groq key",
                if config.key_for(Provider::Groq).is_some() {
                    "saved"
                } else {
                    "not set"
                },
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

    ui::heading(if update_mode {
        "Wisper update setup"
    } else {
        "Wisper setup"
    });
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
            if config.auto_polish == Some(true) {
                "on"
            } else {
                "off"
            }
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
            if config.autostart == Some(true) {
                "on"
            } else {
                "off"
            }
        ));
    }

    if !kept.is_empty() {
        println!();
        ui::hint(&format!("Kept: {}.", kept.join(", ")));
    }

    println!();
    status()?;
    println!();

    // Act on the autostart answer rather than only recording it. Setup used to
    // save the preference and stop, which left the listener to be started by hand.
    if autostart::legacy_autostart_present() {
        warn_about_legacy_autostart();
        ui::hint("Remove it, then run: wisper autostart on");
        return Ok(());
    }

    // Ask before starting the listener: a listener with no permission registers
    // nothing, and setup is the moment the user is present to answer.
    if !request_accessibility() {
        println!();
    }

    if config::load().autostart == Some(true) {
        let result = autostart::enable()?;
        ui::success(&result.message);
        report_listener_start()
    } else {
        ui::info("Starting the listener now. It will not come back at login.");
        ui::hint("Enable that later with: wisper autostart on");
        listen(false).await
    }
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

pub fn media(args: &[String]) -> Result<()> {
    let action = args
        .first()
        .map(|a| a.to_lowercase())
        .unwrap_or_else(|| "status".into());

    match action.as_str() {
        "status" => {
            let config = config::load();
            ui::field(
                "Audio ducking",
                if config.audio_ducking == Some(false) {
                    "disabled"
                } else {
                    "enabled"
                },
            );
            ui::field(
                "Duck volume",
                &format!("{}%", config.audio_ducking_volume.unwrap_or(35)),
            );
            // Worth showing: a bug here used to leave the system volume stuck down,
            // and the setting alone does not reveal that.
            if let Some(volume) = media::current_volume() {
                ui::field("System volume", &format!("{volume}%"));
            }
            if !media::is_supported() {
                ui::warn(&format!("Not supported on {}.", std::env::consts::OS));
            }
            Ok(())
        }
        "on" | "enable" | "enabled" => {
            let volume = args
                .get(1)
                .and_then(|v| v.parse::<u8>().ok())
                .unwrap_or(35)
                .min(100);
            config::update(|c| {
                c.audio_ducking = Some(true);
                c.audio_ducking_volume = Some(volume);
            })?;
            ui::success(&format!("Audio ducking enabled at {volume}%."));
            Ok(())
        }
        "off" | "disable" | "disabled" => {
            config::update(|c| c.audio_ducking = Some(false))?;
            match media::restore()? {
                Some(volume) => ui::success(&format!(
                    "Audio ducking disabled. Volume restored to {volume}%."
                )),
                None => {
                    ui::success("Audio ducking disabled.");
                    // Only the process that ducked knows the old value.
                    if let Some(volume) = media::current_volume() {
                        ui::info(&format!("System volume is {volume}%."));
                        if volume < 20 {
                            ui::hint("If that is lower than you expect, set it back with your volume keys.");
                        }
                    }
                }
            }
            Ok(())
        }
        "volume" => {
            let volume = args
                .get(1)
                .and_then(|v| v.parse::<u8>().ok())
                .context("Usage: wisper media volume <0-100>")?
                .min(100);
            config::update(|c| {
                c.audio_ducking = Some(true);
                c.audio_ducking_volume = Some(volume);
            })?;
            ui::success(&format!("Duck volume set to {volume}%."));
            Ok(())
        }
        "test" => {
            if !media::is_supported() {
                bail!(
                    "Audio ducking is not supported on {}.",
                    std::env::consts::OS
                );
            }
            let mut config = config::load();
            config.audio_ducking = Some(true);
            ui::info("Lowering volume for 2 seconds...");
            let before = media::current_volume();
            media::start(&config)?;
            let ducked = media::current_volume();
            std::thread::sleep(std::time::Duration::from_secs(2));
            media::restore()?;
            let after = media::current_volume();
            if let (Some(before), Some(ducked), Some(after)) = (before, ducked, after) {
                ui::field("Before", &format!("{before}%"));
                ui::field("While recording", &format!("{ducked}%"));
                ui::field("Restored to", &format!("{after}%"));
                if after != before {
                    ui::warn("Volume did not return to where it started.");
                }
            }
            ui::success("Volume restored.");
            Ok(())
        }
        other => bail!("Usage: wisper media on|off|status|volume <0-100>|test (got \"{other}\")"),
    }
}

pub fn autostart(args: &[String]) -> Result<()> {
    let action = args
        .first()
        .map(|a| a.to_lowercase())
        .unwrap_or_else(|| "status".into());

    match action.as_str() {
        "status" => {
            let status = autostart::status()?;
            config::update(|c| c.autostart = Some(status.enabled))?;
            if status.enabled {
                ui::success(&status.message);
            } else {
                ui::info(&status.message);
            }
            if autostart::legacy_autostart_present() {
                warn_about_legacy_autostart();
            }
            Ok(())
        }
        "on" | "enable" | "enabled" => {
            if autostart::legacy_autostart_present() {
                warn_about_legacy_autostart();
                bail!(
                    "Refusing to enable autostart while the TypeScript LaunchAgent is installed."
                );
            }
            // A listener started from a terminal belongs to that terminal; the
            // launcher owns its own copy from here.
            process_state::stop_other_listeners();
            let result = autostart::enable()?;
            config::update(|c| c.autostart = Some(result.enabled))?;
            ui::success(&result.message);
            report_listener_start()
        }
        "off" | "disable" | "disabled" => {
            let result = autostart::disable()?;
            config::update(|c| c.autostart = Some(false))?;
            process_state::stop_other_listeners();
            ui::success(&result.message);
            Ok(())
        }
        other => bail!("Usage: wisper autostart on|off|status (got \"{other}\")"),
    }
}

pub async fn autoupdate(args: &[String]) -> Result<()> {
    let action = args
        .first()
        .map(|a| a.to_lowercase())
        .unwrap_or_else(|| "status".into());

    match action.as_str() {
        "status" => {
            let config = config::load();
            ui::field("Version", updater::CURRENT_VERSION);
            let enabled = config.auto_update != Some(false);
            ui::field(
                "Update checks",
                if enabled { "enabled" } else { "disabled" },
            );
            if enabled {
                ui::field(
                    "Check interval",
                    &format!(
                        "{} minutes",
                        config
                            .auto_update_interval_minutes
                            .unwrap_or(DEFAULT_UPDATE_INTERVAL_MINUTES)
                    ),
                );
            }
            // Nothing installs by itself, so say so rather than let "enabled" imply it.
            ui::hint(
                "The listener only logs when an update exists. Install it with: wisper update",
            );
            Ok(())
        }
        "on" | "enable" | "enabled" => {
            let minutes = args
                .get(1)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(DEFAULT_UPDATE_INTERVAL_MINUTES)
                .max(MIN_UPDATE_INTERVAL_MINUTES);
            config::update(|c| {
                c.auto_update = Some(true);
                c.auto_update_interval_minutes = Some(minutes);
            })?;
            ui::success(&format!(
                "Update checks enabled. Checking every {minutes} minutes."
            ));
            // The listener reads the interval once, at startup.
            ui::hint("Restart the listener to pick it up: wisper restart");
            Ok(())
        }
        "off" | "disable" | "disabled" => {
            config::update(|c| c.auto_update = Some(false))?;
            ui::success("Update checks disabled.");
            ui::hint("Restart the listener to pick it up: wisper restart");
            Ok(())
        }
        // `--apply` is kept working because the old help text advertised it.
        "check" => update(!args.iter().any(|a| a == "--apply")).await,
        other => bail!("Usage: wisper autoupdate on|off|status|check (got \"{other}\")"),
    }
}

/// Install the latest release over this one.
pub async fn update(check_only: bool) -> Result<()> {
    // Windows leaves the previous binary behind because it cannot delete a running
    // image; this is the later run that can.
    updater::clean_stale();

    let bar = ui::spinner("Checking for updates...");
    let release = updater::latest_release().await;
    bar.finish_and_clear();

    let Some(release) = release? else {
        ui::info("No releases published yet, so there is nothing to update to.");
        return Ok(());
    };

    if !release.is_newer_than_current() {
        ui::success(&format!(
            "Already on the latest release (v{}).",
            updater::CURRENT_VERSION
        ));
        return Ok(());
    }

    ui::warn(&format!(
        "Update available: v{} -> {}",
        updater::CURRENT_VERSION,
        release.tag
    ));
    if check_only {
        ui::hint("Install it with: wisper update");
        return Ok(());
    }

    // The listener runs from the binary being replaced. Windows will not let
    // anything overwrite a running image, and on every platform a listener left
    // alive would carry on executing the old code.
    let was_running = process_state::listener_is_running();
    let supervised = autostart::suspend();
    process_state::stop_other_listeners();

    let survivors = process_state::stubborn_listeners();
    if !survivors.is_empty() {
        if supervised {
            autostart::resume();
        }
        ui::failure(&format!(
            "{} listener(s) could not be stopped: {survivors:?}",
            survivors.len()
        ));
        bail!("Not updating while a listener is running. Stop those PIDs and try again.");
    }

    let bar = ui::spinner(&format!("Downloading {}...", release.tag));
    let applied = updater::apply(&release).await;
    bar.finish_and_clear();

    let applied = match applied {
        Ok(applied) => applied,
        Err(error) => {
            // The old binaries are untouched on failure, so put the listener back.
            if supervised {
                autostart::resume();
            } else if was_running {
                let _ = autostart::spawn_detached();
            }
            return Err(error);
        }
    };

    ui::success(&format!("Updated v{} -> {}", applied.from, applied.to));
    for path in &applied.replaced {
        ui::field("Replaced", &path.display().to_string());
    }

    if supervised {
        if autostart::resume() {
            ui::success("Listener restarted through the login launcher.");
            report_listener_start()?;
        } else {
            ui::warn("Could not restart through the launcher. Run: wisper listen");
        }
    } else if was_running {
        match autostart::spawn_detached() {
            Ok(pid) => {
                ui::success(&format!("Listener restarted (pid {pid})."));
                report_listener_start()?;
            }
            Err(error) => {
                ui::warn(&format!("Could not restart the listener: {error}"));
                ui::hint("Start it with: wisper listen");
            }
        }
    }

    if cfg!(target_os = "macos") {
        // macOS ties Accessibility permission to binary identity, and these builds
        // are not signed yet, so the replacement counts as a different program.
        ui::warn("macOS may no longer trust the new binary for global shortcuts.");
        ui::hint("If the shortcut stops working, run: wisper doctor");
        ui::hint("It will ask macOS for permission again. An old entry in System Settings >");
        ui::hint("Privacy & Security > Accessibility may need removing with the minus button.");
    }
    Ok(())
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
            if probe.has_signal {
                ""
            } else {
                ", silent during test"
            }
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
        "Recording from {}...",
        device.unwrap_or(audio::DEFAULT_DEVICE)
    ));
    let recording = audio::start(device, path)?;
    let limit = std::time::Duration::from_secs(seconds);

    // A live meter turns "did it record anything?" into something you can see
    // while speaking, instead of a number printed after the fact.
    if let Err(error) = crate::tui::record_with_meter(&recording, Some(limit)) {
        ui::warn(&format!("Meter unavailable ({error}). Recording anyway."));
        std::thread::sleep(limit);
    }
    let finished = recording.stop()?;

    ui::field("File", &finished.path.display().to_string());
    ui::field(
        "Duration",
        &format!("{:.1}s", finished.duration.as_secs_f32()),
    );
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
        // A launcher with KeepAlive would revive its own copy and sweep this one
        // away, so foreground debugging needs the launcher paused first.
        if autostart::managed() {
            ui::warn(
                "Autostart is enabled. Stop it first so it does not restart a rival listener:",
            );
            ui::hint("  wisper autostart off");
        }
        return crate::listener::run().await;
    }

    warn_about_legacy_autostart();

    // A listener without this permission starts, registers nothing, and looks
    // fine — so ask while there is still a terminal to ask in.
    if std::io::stdin().is_terminal() {
        request_accessibility();
    }

    process_state::stop_other_listeners();
    let survivors = process_state::stubborn_listeners();
    if !survivors.is_empty() {
        // Starting another one now would mean every press fires twice.
        ui::failure(&format!(
            "{} listener(s) could not be stopped: {survivors:?}",
            survivors.len()
        ));
        bail!("Refusing to start a second listener. Stop those PIDs first.");
    }

    if autostart::managed() {
        if autostart::restart() {
            ui::success("Listener restarted through the login launcher.");
            return report_listener_start();
        }
        ui::warn("Could not restart through the launcher. Starting directly.");
    }

    let pid = autostart::spawn_detached()?;
    ui::success(&format!("Listener started in the background (pid {pid})."));
    report_listener_start()
}

/// Confirm the listener actually came up. A detached process that dies instantly
/// used to look identical to one that started fine.
fn report_listener_start() -> Result<()> {
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let tail: Vec<String> = log::read_logs()
        .lines()
        .rev()
        .take(25)
        .map(|line| line.to_string())
        .collect();

    if tail
        .iter()
        .any(|line| line.contains("Shortcut registered:"))
    {
        ui::success("Verified: shortcut registered.");
    } else if tail
        .iter()
        .any(|line| line.contains("could not be registered"))
    {
        ui::failure("Listener started but no shortcut registered. Run: wisper doctor");
    } else {
        ui::warn("Listener start requested. If the shortcut does nothing, run: wisper doctor");
    }
    Ok(())
}

/// The TypeScript LaunchAgent and this build would both register the same
/// shortcuts, so one press would fire twice.
fn warn_about_legacy_autostart() {
    if autostart::legacy_autostart_present() {
        ui::warn("The TypeScript LaunchAgent (com.wisper.cli) is still installed.");
        ui::hint("Both builds would register the same shortcuts. Disable the old one first:");
        ui::hint("  launchctl bootout gui/$(id -u)/com.wisper.cli");
    }
}

pub fn stop() -> Result<()> {
    // With a KeepAlive launcher, killing the process alone is pointless: it comes
    // straight back. The launcher has to be stopped too.
    if autostart::managed() {
        autostart::disable()?;
        config::update(|c| c.autostart = Some(false))?;
        process_state::stop_other_listeners();
        ui::success("Listener stopped and autostart disabled.");
        ui::hint("Re-enable it with: wisper autostart on");
        return Ok(());
    }

    let stopped = process_state::stop_other_listeners();
    let remaining = process_state::stubborn_listeners();

    if !remaining.is_empty() {
        // Reporting success while listeners survive is how several ended up
        // running at once, each pasting the same dictation.
        ui::failure(&format!(
            "{} listener(s) are still running: {remaining:?}",
            remaining.len()
        ));
        ui::hint("Stop them by PID, then run: wisper doctor");
        return Ok(());
    }

    if stopped > 0 {
        ui::success(&format!("Stopped {stopped} listener(s)."));
    } else {
        ui::info("No running listener found.");
    }
    Ok(())
}

pub async fn restart() -> Result<()> {
    listen(false).await
}

/// How long to keep watching after the system dialog appears. Long enough to find
/// the switch, short enough not to look hung if the dialog was dismissed.
const PERMISSION_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Ask macOS for Accessibility permission with its own dialog, then wait for it.
///
/// Reading out a path and a menu trail was leaving the work to the user. macOS has
/// an API for this: the dialog names the binary, its button opens the right pane,
/// and the binary is added to the Accessibility list so only the switch is left.
///
/// Returns whether permission is held by the time this returns.
fn request_accessibility() -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    /// `setup` asks, then calls `listen`, which asks again — without this the user
    /// would face two dialogs and two waits for one decision.
    static ASKED: AtomicBool = AtomicBool::new(false);

    if !hotkey::permission_is_required() || hotkey::has_permission() {
        return true;
    }
    if ASKED.swap(true, Ordering::SeqCst) {
        return false;
    }

    ui::warn("Global shortcuts need Accessibility permission.");
    match hotkey::request_permission() {
        Ok(true) => return true,
        Ok(false) => {
            ui::info("macOS is asking for it now.");
            ui::hint("Choose \"Open System Settings\", then switch wisper on.");
        }
        Err(error) => {
            ui::hint(&error.to_string());
            return false;
        }
    }

    // The dialog is the system's, not ours, so there is nothing to await — poll.
    let bar = ui::spinner("Waiting for permission...");
    let start = std::time::Instant::now();
    // The dialog is easy to dismiss by accident, so if nothing has happened after a
    // few seconds, put the pane on screen directly rather than waiting it out.
    let open_settings_after = std::time::Duration::from_secs(8);
    let mut opened_settings = false;

    while start.elapsed() < PERMISSION_WAIT {
        if hotkey::has_permission() {
            bar.finish_and_clear();
            ui::success("Accessibility granted.");
            return true;
        }
        if !opened_settings && start.elapsed() > open_settings_after {
            opened_settings = hotkey::open_permission_settings().is_ok();
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    bar.finish_and_clear();

    ui::warn("Still not granted, carrying on without it.");
    ui::hint("Turn it on later, then run: wisper restart");
    false
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
        // Prompting is the fix, so offer it here rather than only naming the pane.
        // Doctor is diagnostic, so this asks before taking over the screen.
        if std::io::stdin().is_terminal()
            && Confirm::new("Ask macOS for permission now?")
                .with_default(true)
                .prompt()
                .unwrap_or(false)
        {
            request_accessibility();
        } else {
            ui::hint(hotkey::permission_hint());
        }
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
                if audio::is_likely_virtual(device) {
                    "  (virtual)"
                } else {
                    ""
                }
            ));
        }
        ui::field(
            "Configured",
            config.audio_device.as_deref().unwrap_or("default"),
        );
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
                    ui::warn(&format!(
                        "{label}: {value} is the same combo as the dictation shortcut"
                    ));
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

pub async fn open(port: Option<u16>) -> Result<()> {
    let url = crate::dashboard::serve(port.unwrap_or(3838)).await?;
    crate::dashboard::open_in_browser(&url);
    ui::success(&format!("Dashboard running at {url}"));
    ui::hint("Press Ctrl+C to stop.");
    tokio::signal::ctrl_c().await?;
    Ok(())
}
