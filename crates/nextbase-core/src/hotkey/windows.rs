//! Windows global shortcuts.
//!
//! Replaces the inline PowerShell helper. `RegisterHotKey` is used with
//! `MOD_NOREPEAT`, which the PowerShell version lacked: without it, holding the
//! shortcut queues one WM_HOTKEY per keyboard auto-repeat, and those queued
//! notifications replay as phantom press/release pairs after the key is released.

use anyhow::{anyhow, bail, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL,
    MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE, WM_HOTKEY};

use super::HotkeyEvent;
use crate::shortcut::{self, Parsed};

const HOTKEY_ID: i32 = 9123;
const POLL_INTERVAL: Duration = Duration::from_millis(25);

// Virtual key codes for the modifier halves, used when polling held state.
const VK_CONTROL: i32 = 0x11;
const VK_MENU: i32 = 0x12;
const VK_SHIFT: i32 = 0x10;
const VK_LWIN: i32 = 0x5B;
const VK_RWIN: i32 = 0x5C;
const VK_LCONTROL: i32 = 0xA2;
const VK_RCONTROL: i32 = 0xA3;
const VK_LSHIFT: i32 = 0xA0;
const VK_RSHIFT: i32 = 0xA1;
const VK_LMENU: i32 = 0xA4;
const VK_RMENU: i32 = 0xA5;

pub struct WindowsHotkey {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl WindowsHotkey {
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for WindowsHotkey {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn key_down(vk: i32) -> bool {
    // The high bit of GetAsyncKeyState means "currently held".
    (unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000) != 0
}

fn any_down(keys: &[i32]) -> bool {
    keys.iter().copied().any(key_down)
}

/// True while every modifier the shortcut needs is held.
fn required_modifiers_down(parsed: &Parsed) -> bool {
    if parsed.ctrl && !any_down(&[VK_CONTROL, VK_LCONTROL, VK_RCONTROL]) {
        return false;
    }
    if parsed.alt && !any_down(&[VK_MENU, VK_LMENU, VK_RMENU]) {
        return false;
    }
    if parsed.shift && !any_down(&[VK_SHIFT, VK_LSHIFT, VK_RSHIFT]) {
        return false;
    }
    if parsed.meta && !any_down(&[VK_LWIN, VK_RWIN]) {
        return false;
    }
    true
}

fn modifier_flags(parsed: &Parsed) -> HOT_KEY_MODIFIERS {
    let mut flags = MOD_NOREPEAT;
    if parsed.alt {
        flags |= MOD_ALT;
    }
    if parsed.ctrl {
        flags |= MOD_CONTROL;
    }
    if parsed.shift {
        flags |= MOD_SHIFT;
    }
    if parsed.meta {
        flags |= MOD_WIN;
    }
    flags
}

pub fn listen<F>(shortcut_text: &str, on_event: F) -> Result<WindowsHotkey>
where
    F: Fn(HotkeyEvent) + Send + 'static,
{
    let parsed = shortcut::parse(shortcut_text)?;
    let virtual_key = match parsed.key.as_deref() {
        Some(key) => Some(shortcut::windows_virtual_key(key)?),
        None => None,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    let label = shortcut_text.to_string();

    let worker = std::thread::spawn(move || {
        match virtual_key {
            // Modifier-only combos cannot be registered, so poll held state.
            None => {
                let _ = ready_tx.send(Ok(()));
                let mut held = false;
                while !thread_stop.load(Ordering::SeqCst) {
                    let down = required_modifiers_down(&parsed);
                    if down && !held {
                        held = true;
                        on_event(HotkeyEvent::Down);
                    } else if !down && held {
                        held = false;
                        on_event(HotkeyEvent::Up);
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
            Some(vk) => {
                // RegisterHotKey binds to the calling thread, so registration and
                // the message pump must both live here.
                let registered =
                    unsafe { RegisterHotKey(None, HOTKEY_ID, modifier_flags(&parsed), vk) };
                if registered.is_err() {
                    let _ = ready_tx.send(Err(anyhow!(
                        "Could not register {label}. Another application may already own it."
                    )));
                    return;
                }
                let _ = ready_tx.send(Ok(()));

                // PeekMessage rather than GetMessage: GetMessage blocks, and the
                // stop flag has to stay observable.
                let mut message = MSG::default();
                while !thread_stop.load(Ordering::SeqCst) {
                    let has_message =
                        unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool();

                    if has_message
                        && message.message == WM_HOTKEY
                        && message.wParam.0 as i32 == HOTKEY_ID
                    {
                        on_event(HotkeyEvent::Down);
                        // Hold until every key is released, so Up matches the
                        // physical release rather than the next notification.
                        while !thread_stop.load(Ordering::SeqCst)
                            && key_down(vk as i32)
                            && required_modifiers_down(&parsed)
                        {
                            std::thread::sleep(POLL_INTERVAL);
                        }
                        on_event(HotkeyEvent::Up);
                        continue;
                    }

                    if !has_message {
                        std::thread::sleep(POLL_INTERVAL);
                    }
                }

                unsafe {
                    let _ = UnregisterHotKey(None, HOTKEY_ID);
                }
            }
        }
    });

    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(WindowsHotkey {
            stop,
            worker: Some(worker),
        }),
        Ok(Err(error)) => {
            let _ = worker.join();
            Err(error)
        }
        Err(_) => {
            stop.store(true, Ordering::SeqCst);
            bail!("Timed out registering {shortcut_text}.")
        }
    }
}
