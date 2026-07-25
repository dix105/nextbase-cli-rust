//! Optional audio ducking: drop system volume while recording, restore after.
//!
//! The previous volume is remembered in memory rather than read back at restore
//! time, because by then the volume has already been changed.

use anyhow::Result;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use crate::config::{Config, DEFAULT_DUCKING_VOLUME};

/// Volume before ducking, as a 0-100 percentage.
static PREVIOUS_VOLUME: Mutex<Option<u8>> = Mutex::new(None);

#[cfg(target_os = "macos")]
fn read_volume() -> Option<u8> {
    let output = Command::new("osascript")
        .args(["-e", "output volume of (get volume settings)"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u8>()
        .ok()
}

#[cfg(target_os = "macos")]
fn set_volume(percent: u8) -> Result<()> {
    Command::new("osascript")
        .args(["-e", &format!("set volume output volume {percent}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(())
}

#[cfg(windows)]
fn read_volume() -> Option<u8> {
    // Windows exposes no simple CLI for the master volume, so ducking restores to
    // a remembered value only. 100 is the safe assumption for a first run.
    Some(100)
}

#[cfg(windows)]
fn set_volume(percent: u8) -> Result<()> {
    // Nudges the master volume with the standard media keys via a WScript shim.
    let steps = ((100 - percent as i32) / 2).clamp(0, 50);
    let script = format!(
        "$w = New-Object -ComObject WScript.Shell; 1..{steps} | ForEach-Object {{ $w.SendKeys([char]174) }}"
    );
    Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn read_volume() -> Option<u8> {
    None
}

#[cfg(not(any(target_os = "macos", windows)))]
fn set_volume(_percent: u8) -> Result<()> {
    Ok(())
}

pub fn is_supported() -> bool {
    cfg!(any(target_os = "macos", windows))
}

/// Duck if enabled in config. Never fails the caller: losing volume control must
/// not cost the user a dictation.
pub fn start(config: &Config) -> Result<()> {
    if config.audio_ducking != Some(true) || !is_supported() {
        return Ok(());
    }

    let target = config
        .audio_ducking_volume
        .unwrap_or(DEFAULT_DUCKING_VOLUME)
        .min(100);

    if let Ok(mut previous) = PREVIOUS_VOLUME.lock() {
        if previous.is_none() {
            *previous = read_volume();
        }
    }
    set_volume(target)
}

pub fn restore() -> Result<()> {
    if !is_supported() {
        return Ok(());
    }

    let previous = PREVIOUS_VOLUME.lock().ok().and_then(|mut p| p.take());
    if let Some(volume) = previous {
        set_volume(volume)?;
    }
    Ok(())
}

pub fn current_volume() -> Option<u8> {
    read_volume()
}
