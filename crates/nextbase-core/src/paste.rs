//! Clipboard access and keystroke injection.
//!
//! Replaces `clipboardy` plus the `osascript` / `cscript`+VBScript helpers: no
//! temp script files, no per-paste subprocess.

use anyhow::{anyhow, Context, Result};
use std::time::Duration;

/// Waits used after injecting a copy shortcut before reading the clipboard. The
/// foreground app needs a moment to service the keystroke.
const COPY_SETTLE: Duration = Duration::from_millis(180);
const SELECT_ALL_COPY_SETTLE: Duration = Duration::from_millis(280);

fn clipboard() -> Result<arboard::Clipboard> {
    arboard::Clipboard::new().context("Could not open the system clipboard")
}

pub fn read_clipboard() -> Result<String> {
    match clipboard()?.get_text() {
        Ok(text) => Ok(text),
        // An empty or non-text clipboard is not an error for our purposes.
        Err(arboard::Error::ContentNotAvailable) => Ok(String::new()),
        Err(error) => Err(anyhow!(error)),
    }
}

pub fn write_clipboard(text: &str) -> Result<()> {
    clipboard()?
        .set_text(text.to_string())
        .map_err(|e| anyhow!(e))
}

// ------------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    const KEY_A: u16 = 0;
    const KEY_C: u16 = 8;
    const KEY_V: u16 = 9;

    fn tap_key_with_command(key: u16) -> Result<()> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow!("Could not create a keyboard event source"))?;

        for pressed in [true, false] {
            let event = CGEvent::new_keyboard_event(source.clone(), key, pressed)
                .map_err(|_| anyhow!("Could not create a keyboard event"))?;
            // Command must be set on both the down and the up event, or the
            // foreground app sees a bare keypress.
            event.set_flags(CGEventFlags::CGEventFlagCommand);
            event.post(CGEventTapLocation::HID);
        }
        Ok(())
    }

    pub fn send_paste() -> Result<()> {
        tap_key_with_command(KEY_V)
    }

    pub fn send_copy() -> Result<()> {
        tap_key_with_command(KEY_C)
    }

    pub fn send_select_all_and_copy() -> Result<()> {
        tap_key_with_command(KEY_A)?;
        std::thread::sleep(Duration::from_millis(50));
        tap_key_with_command(KEY_C)
    }
}

// ----------------------------------------------------------------- Windows

#[cfg(windows)]
mod platform {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        VIRTUAL_KEY, VK_A, VK_C, VK_CONTROL, VK_V,
    };

    fn key_input(key: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: key,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// Ctrl is held across the whole chord, matching what `SendKeys "^v"` did.
    fn send_with_control(key: VIRTUAL_KEY) -> Result<()> {
        let inputs = [
            key_input(VK_CONTROL, KEYBD_EVENT_FLAGS(0)),
            key_input(key, KEYBD_EVENT_FLAGS(0)),
            key_input(key, KEYEVENTF_KEYUP),
            key_input(VK_CONTROL, KEYEVENTF_KEYUP),
        ];

        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() {
            return Err(anyhow!("Could not inject keystrokes into the focused app"));
        }
        Ok(())
    }

    pub fn send_paste() -> Result<()> {
        send_with_control(VK_V)
    }

    pub fn send_copy() -> Result<()> {
        send_with_control(VK_C)
    }

    pub fn send_select_all_and_copy() -> Result<()> {
        send_with_control(VK_A)?;
        std::thread::sleep(Duration::from_millis(50));
        send_with_control(VK_C)
    }
}

// -------------------------------------------------------------- unsupported

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::*;

    fn unsupported() -> Result<()> {
        Err(anyhow!(
            "Pasting is not supported on {} yet.",
            std::env::consts::OS
        ))
    }

    pub fn send_paste() -> Result<()> {
        unsupported()
    }
    pub fn send_copy() -> Result<()> {
        unsupported()
    }
    pub fn send_select_all_and_copy() -> Result<()> {
        unsupported()
    }
}

/// Put `text` on the clipboard and paste it into whatever has focus.
pub fn paste_into_active_app(text: &str) -> Result<()> {
    write_clipboard(text)?;
    platform::send_paste()
}

/// Copy the current selection and return it.
pub fn copy_selected_text() -> Result<String> {
    platform::send_copy()?;
    std::thread::sleep(COPY_SETTLE);
    Ok(read_clipboard()?.trim().to_string())
}

/// Select everything in the focused field and return it.
///
/// The clipboard is cleared first on purpose: if the app refuses Ctrl/Cmd+A+C, or
/// nothing editable has focus, this must come back empty rather than returning a
/// stale clipboard entry that would then overwrite the user's field.
pub fn copy_focused_input_text() -> Result<String> {
    write_clipboard("")?;
    platform::send_select_all_and_copy()?;
    std::thread::sleep(SELECT_ALL_COPY_SETTLE);
    Ok(read_clipboard()?.trim().to_string())
}
