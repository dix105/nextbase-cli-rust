//! System audio capture — everyone else on the call.
//!
//! Each platform gets there a different way:
//!
//! - **macOS**: ScreenCaptureKit. There is no loopback device to open, and the
//!   alternative is asking the user to install BlackHole and re-route their output.
//!   Costs a Screen Recording permission.
//! - **Windows**: WASAPI loopback on the default render endpoint. cpal does not
//!   expose it, so it is written directly against the `windows` crate.
//! - **Linux**: PulseAudio and PipeWire publish `.monitor` sources that enumerate as
//!   ordinary inputs, so cpal already handles it.

use anyhow::Result;

use super::SourceHandle;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows_loopback;

/// Whether system audio can be captured here, and what is in the way if not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemAudioStatus {
    Ready,
    /// Supported, but the OS has not granted permission yet.
    PermissionRequired {
        hint: String,
    },
    /// Not supported on this platform or configuration.
    Unavailable {
        reason: String,
    },
}

pub(crate) fn start() -> Result<SourceHandle> {
    #[cfg(target_os = "macos")]
    {
        macos::start()
    }
    #[cfg(windows)]
    {
        windows_loopback::start()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let name = linux_monitor_source().ok_or_else(|| {
            anyhow::anyhow!(
                "No monitor source found. PulseAudio or PipeWire publishes one as an input device named \"…monitor\"."
            )
        })?;
        super::mic::open(super::SourceKind::System, Some(&name))
    }
    #[cfg(not(any(target_os = "macos", windows, unix)))]
    {
        anyhow::bail!(
            "System audio capture is not supported on {}.",
            std::env::consts::OS
        )
    }
}

/// The device name system audio would be captured from, for `doctor` to display.
pub fn system_source_name() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        Some("ScreenCaptureKit (system output)".to_string())
    }
    #[cfg(windows)]
    {
        windows_loopback::render_device_name().ok()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        linux_monitor_source()
    }
    #[cfg(not(any(target_os = "macos", windows, unix)))]
    {
        None
    }
}

pub fn status() -> SystemAudioStatus {
    #[cfg(target_os = "macos")]
    {
        macos::status()
    }
    #[cfg(windows)]
    {
        match windows_loopback::render_device_name() {
            Ok(_) => SystemAudioStatus::Ready,
            Err(error) => SystemAudioStatus::Unavailable {
                reason: error.to_string(),
            },
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        match linux_monitor_source() {
            Some(_) => SystemAudioStatus::Ready,
            None => SystemAudioStatus::Unavailable {
                reason: "No PulseAudio or PipeWire monitor source is available.".to_string(),
            },
        }
    }
    #[cfg(not(any(target_os = "macos", windows, unix)))]
    {
        SystemAudioStatus::Unavailable {
            reason: format!("Not supported on {}.", std::env::consts::OS),
        }
    }
}

/// Ask the OS for whatever permission system capture needs. No-op where none is.
pub fn request_permission() -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        Ok(macos::request_screen_recording())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(true)
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn linux_monitor_source() -> Option<String> {
    crate::audio::list_input_devices()
        .into_iter()
        .find(|name| name.to_lowercase().contains("monitor"))
}
