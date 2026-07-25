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

/// Core Audio endpoint for the default playback device.
///
/// This replaces an earlier attempt that pressed the volume-down media key N
/// times. Those presses are *relative*, so ducking to "35%" walked the volume
/// down from wherever it was and usually hit zero, and restoring computed zero
/// presses and did nothing at all — leaving the volume stuck down.
#[cfg(windows)]
fn endpoint_volume() -> Result<windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume> {
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{
        eMultimedia, eRender, IMMDeviceEnumerator, MMDeviceEnumerator,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    };

    unsafe {
        // RPC_E_CHANGED_MODE only means COM is already initialised on this thread
        // with another model, which these calls do not care about.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
        Ok(device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)?)
    }
}

#[cfg(windows)]
fn read_volume() -> Option<u8> {
    unsafe {
        let scalar = endpoint_volume().ok()?.GetMasterVolumeLevelScalar().ok()?;
        Some((scalar * 100.0).round().clamp(0.0, 100.0) as u8)
    }
}

#[cfg(windows)]
fn set_volume(percent: u8) -> Result<()> {
    unsafe {
        // The event-context GUID is optional; a null pointer means "no context".
        endpoint_volume()?
            .SetMasterVolumeLevelScalar(f32::from(percent.min(100)) / 100.0, std::ptr::null())?;
    }
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

/// Put the volume back. Returns what it was restored to, or `None` if nothing was
/// ducked in this process.
pub fn restore() -> Result<Option<u8>> {
    if !is_supported() {
        return Ok(None);
    }

    let previous = PREVIOUS_VOLUME.lock().ok().and_then(|mut p| p.take());
    match previous {
        Some(volume) => {
            set_volume(volume)?;
            Ok(Some(volume))
        }
        None => Ok(None),
    }
}

pub fn current_volume() -> Option<u8> {
    read_volume()
}
