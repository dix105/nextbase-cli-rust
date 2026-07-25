//! Global shortcut registration.
//!
//! Replaces the per-platform helper subprocesses: no `swift` invocation compiling
//! `mac-hotkey.swift` at every listener start, and no inline PowerShell on
//! Windows. One process, one event tap.

use anyhow::Result;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    /// Shortcut pressed. Recording starts here.
    Down,
    /// Shortcut released. Recording stops here.
    Up,
}

/// Keeps the shortcut registered. Dropping it unregisters and releases the tap.
pub struct HotkeyHandle {
    #[cfg(any(target_os = "macos", windows))]
    inner: PlatformHandle,
    #[cfg(not(any(target_os = "macos", windows)))]
    _private: (),
}

#[cfg(target_os = "macos")]
type PlatformHandle = macos::MacHotkey;
#[cfg(windows)]
type PlatformHandle = windows::WindowsHotkey;

impl HotkeyHandle {
    pub fn stop(self) {
        #[cfg(any(target_os = "macos", windows))]
        self.inner.stop();
    }
}

/// Register `shortcut` and call `on_event` on press and release.
///
/// The callback runs on the platform's event thread, so it must not block. The
/// listener forwards straight into a channel.
#[allow(unused_variables)]
pub fn listen<F>(shortcut: &str, on_event: F) -> Result<HotkeyHandle>
where
    F: Fn(HotkeyEvent) + Send + 'static,
{
    #[cfg(target_os = "macos")]
    {
        Ok(HotkeyHandle {
            inner: macos::listen(shortcut, on_event)?,
        })
    }
    #[cfg(windows)]
    {
        Ok(HotkeyHandle {
            inner: windows::listen(shortcut, on_event)?,
        })
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        anyhow::bail!(
            "Global shortcuts are not supported on {} yet. Windows and macOS are supported.",
            std::env::consts::OS
        )
    }
}

/// True when this binary may observe keyboard events.
///
/// macOS ties Accessibility permission to *binary identity*, so a fresh build at a
/// new path starts untrusted even if the previous CLI was allowed.
pub fn has_permission() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::is_trusted()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Modifier state, independent of any terminal's key reporting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

impl Mods {
    pub fn count(&self) -> usize {
        [self.ctrl, self.alt, self.shift, self.meta]
            .iter()
            .filter(|held| **held)
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }
}

/// What the capture tap saw.
#[derive(Debug, Clone)]
pub enum CaptureUpdate {
    /// The held modifier set changed. Empty means everything was released.
    Mods(Mods),
    /// A non-modifier key went down while those modifiers were held.
    Key { mods: Mods, key: String },
}

/// Observe raw key and modifier events until the returned handle is dropped.
///
/// This exists because a terminal cannot see a bare modifier press unless it
/// implements the kitty keyboard protocol — Apple Terminal does not — so
/// modifier-only combos like `Ctrl+Command` could never be captured by reading
/// stdin. The event tap sees them regardless of which terminal is in use.
///
/// Key presses are swallowed while capturing, so a stray `Cmd+Q` cannot act on the
/// focused app mid-capture.
#[allow(unused_variables)]
pub fn capture() -> Result<(std::sync::mpsc::Receiver<CaptureUpdate>, HotkeyHandle)> {
    #[cfg(target_os = "macos")]
    {
        let (receiver, inner) = macos::capture()?;
        Ok((receiver, HotkeyHandle { inner }))
    }
    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!("Live key capture is only implemented on macOS. Type the shortcut instead.")
    }
}

pub fn permission_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "Grant Accessibility permission: System Settings > Privacy & Security > Accessibility, then add this binary (or your terminal) and restart the listener."
    } else {
        "Another application may already own this shortcut. Try a different key."
    }
}
