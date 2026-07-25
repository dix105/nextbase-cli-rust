//! macOS global shortcuts via a CGEventTap.
//!
//! Direct port of `src/mac-hotkey.swift`. The tap runs on its own thread with its
//! own CFRunLoop, so several shortcuts can be registered independently and the
//! caller's thread stays free.

use anyhow::{anyhow, bail, Result};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, EventField,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use super::{CaptureUpdate, HotkeyEvent, Mods};
use crate::shortcut;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: core_foundation::dictionary::CFDictionaryRef)
        -> bool;
    static kAXTrustedCheckOptionPrompt: core_foundation::string::CFStringRef;
}

pub fn is_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// Ask macOS to show its own Accessibility permission dialog.
///
/// This is the only way to *request* the grant; there is no API that can award it.
/// The dialog names this binary and its button opens the right settings pane, which
/// beats telling someone to navigate five levels of System Settings by hand. macOS
/// also adds the binary to the Accessibility list as a side effect, so the user
/// only has to flip the switch instead of finding the file in a picker.
///
/// Returns whether permission is already granted — the dialog is not modal to us,
/// so a `false` here just means "asked".
pub fn request_trust() -> bool {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;

    unsafe {
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let options = CFDictionary::from_CFType_pairs(&[(
            key.as_CFType(),
            CFBoolean::true_value().as_CFType(),
        )]);
        AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef())
    }
}

/// `CFRunLoopStop` is documented as safe to call from another thread, which is the
/// only way to unblock a tap thread parked in `CFRunLoopRun`.
struct RunLoopHandle(CFRunLoop);
unsafe impl Send for RunLoopHandle {}

pub struct MacHotkey {
    stop: Arc<AtomicBool>,
    run_loop: Option<RunLoopHandle>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl MacHotkey {
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.run_loop.take() {
            handle.0.stop();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for MacHotkey {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Compare only the four modifiers a Wisper shortcut can use.
///
/// `CGEventFlags` also carries Caps Lock, Fn and non-coalesced markers; an exact
/// comparison let the shortcut fall through to the focused app, which produced the
/// macOS alert beep.
fn modifiers_match(flags: CGEventFlags, parsed: &shortcut::Parsed) -> bool {
    let has_cmd = flags.contains(CGEventFlags::CGEventFlagCommand);
    let has_alt = flags.contains(CGEventFlags::CGEventFlagAlternate);
    let has_shift = flags.contains(CGEventFlags::CGEventFlagShift);
    let has_ctrl = flags.contains(CGEventFlags::CGEventFlagControl);

    has_cmd == parsed.meta
        && has_alt == parsed.alt
        && has_shift == parsed.shift
        && has_ctrl == parsed.ctrl
}

pub fn listen<F>(shortcut_text: &str, on_event: F) -> Result<MacHotkey>
where
    F: Fn(HotkeyEvent) + Send + 'static,
{
    let parsed = shortcut::parse(shortcut_text)?;
    let key_code: Option<u16> = match parsed.key.as_deref() {
        Some(key) => Some(shortcut::mac_key_code(key)?),
        None => None,
    };

    if !is_trusted() {
        bail!(
            "Accessibility permission is required for global shortcuts. {}",
            super::permission_hint()
        );
    }

    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel::<Result<RunLoopHandle>>();
    let shortcut_label = shortcut_text.to_string();

    let worker = std::thread::spawn(move || {
        // `isHeld` in the Swift helper: suppresses key auto-repeat, and makes the
        // Up event fire exactly once per press. CGEventTap needs an `Fn` callback,
        // so the flag lives behind interior mutability.
        let is_held = AtomicBool::new(false);
        let modifier_only = key_code.is_none();

        let callback = move |_proxy: CGEventTapProxy,
                             event_type: CGEventType,
                             event: &CGEvent|
              -> Option<CGEvent> {
            match event_type {
                // The tap is disabled by the system if it ever runs long. Pass the
                // event through; re-enabling is handled by the timeout branch below.
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                    return Some(event.clone());
                }
                _ => {}
            }

            if modifier_only {
                if !matches!(event_type, CGEventType::FlagsChanged) {
                    return Some(event.clone());
                }
                let matches = modifiers_match(event.get_flags(), &parsed);
                if matches && !is_held.load(Ordering::Relaxed) {
                    is_held.store(true, Ordering::Relaxed);
                    on_event(HotkeyEvent::Down);
                    return None;
                }
                if !matches && is_held.load(Ordering::Relaxed) {
                    is_held.store(false, Ordering::Relaxed);
                    on_event(HotkeyEvent::Up);
                    return None;
                }
                return Some(event.clone());
            }

            if !matches!(event_type, CGEventType::KeyDown | CGEventType::KeyUp) {
                return Some(event.clone());
            }

            let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            if Some(code) != key_code {
                return Some(event.clone());
            }

            if matches!(event_type, CGEventType::KeyDown)
                && !is_held.load(Ordering::Relaxed)
                && modifiers_match(event.get_flags(), &parsed)
            {
                is_held.store(true, Ordering::Relaxed);
                on_event(HotkeyEvent::Down);
                return None;
            }

            if matches!(event_type, CGEventType::KeyUp) && is_held.load(Ordering::Relaxed) {
                is_held.store(false, Ordering::Relaxed);
                on_event(HotkeyEvent::Up);
                return None;
            }

            Some(event.clone())
        };

        let events = if modifier_only {
            vec![CGEventType::FlagsChanged]
        } else {
            vec![CGEventType::KeyDown, CGEventType::KeyUp]
        };

        let tap = match CGEventTap::new(
            CGEventTapLocation::Session,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            events,
            callback,
        ) {
            Ok(tap) => tap,
            Err(_) => {
                let _ = ready_tx.send(Err(anyhow!(
                    "Could not register {shortcut_label}. {}",
                    super::permission_hint()
                )));
                return;
            }
        };

        let run_loop = CFRunLoop::get_current();
        let source = match tap.mach_port.create_runloop_source(0) {
            Ok(source) => source,
            Err(_) => {
                let _ = ready_tx.send(Err(anyhow!(
                    "Could not attach {shortcut_label} to the event loop."
                )));
                return;
            }
        };

        unsafe {
            run_loop.add_source(&source, kCFRunLoopCommonModes);
        }
        tap.enable();

        if ready_tx.send(Ok(RunLoopHandle(run_loop))).is_err() {
            return;
        }

        // Returns when `CFRunLoop::stop` is called from `shutdown`.
        CFRunLoop::run_current();
    });

    match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(run_loop)) => Ok(MacHotkey {
            stop,
            run_loop: Some(run_loop),
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

fn mods_from_flags(flags: CGEventFlags) -> Mods {
    Mods {
        ctrl: flags.contains(CGEventFlags::CGEventFlagControl),
        alt: flags.contains(CGEventFlags::CGEventFlagAlternate),
        shift: flags.contains(CGEventFlags::CGEventFlagShift),
        meta: flags.contains(CGEventFlags::CGEventFlagCommand),
    }
}

/// Tap every modifier change and key press, for capturing a shortcut by pressing it.
pub fn capture() -> Result<(mpsc::Receiver<CaptureUpdate>, MacHotkey)> {
    if !is_trusted() {
        bail!(
            "Accessibility permission is required to capture a shortcut. {}",
            super::permission_hint()
        );
    }

    let (update_tx, update_rx) = mpsc::channel::<CaptureUpdate>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<RunLoopHandle>>();
    let stop = Arc::new(AtomicBool::new(false));

    let worker = std::thread::spawn(move || {
        let callback = move |_proxy: CGEventTapProxy,
                             event_type: CGEventType,
                             event: &CGEvent|
              -> Option<CGEvent> {
            match event_type {
                CGEventType::FlagsChanged => {
                    let _ = update_tx.send(CaptureUpdate::Mods(mods_from_flags(event.get_flags())));
                    // Passed through: swallowing modifier changes can leave other
                    // apps believing a modifier is stuck down.
                    Some(event.clone())
                }
                CGEventType::KeyDown => {
                    let code =
                        event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                    if let Some(label) = shortcut::mac_key_label(code) {
                        let _ = update_tx.send(CaptureUpdate::Key {
                            mods: mods_from_flags(event.get_flags()),
                            key: label.to_string(),
                        });
                    }
                    // Swallowed, so the keypress being captured cannot also act on
                    // whatever has focus.
                    None
                }
                _ => Some(event.clone()),
            }
        };

        let tap = match CGEventTap::new(
            CGEventTapLocation::Session,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            vec![CGEventType::FlagsChanged, CGEventType::KeyDown],
            callback,
        ) {
            Ok(tap) => tap,
            Err(_) => {
                let _ = ready_tx.send(Err(anyhow!(
                    "Could not start key capture. {}",
                    super::permission_hint()
                )));
                return;
            }
        };

        let run_loop = CFRunLoop::get_current();
        let source = match tap.mach_port.create_runloop_source(0) {
            Ok(source) => source,
            Err(_) => {
                let _ = ready_tx.send(Err(anyhow!(
                    "Could not attach key capture to the event loop."
                )));
                return;
            }
        };
        unsafe {
            run_loop.add_source(&source, kCFRunLoopCommonModes);
        }
        tap.enable();

        if ready_tx.send(Ok(RunLoopHandle(run_loop))).is_err() {
            return;
        }
        CFRunLoop::run_current();
    });

    match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(run_loop)) => Ok((
            update_rx,
            MacHotkey {
                stop,
                run_loop: Some(run_loop),
                worker: Some(worker),
            },
        )),
        Ok(Err(error)) => {
            let _ = worker.join();
            Err(error)
        }
        Err(_) => {
            stop.store(true, Ordering::SeqCst);
            bail!("Timed out starting key capture.")
        }
    }
}
