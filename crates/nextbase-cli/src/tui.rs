//! Inline terminal UI for the two moments that are genuinely live: capturing a
//! shortcut by pressing it, and watching microphone level while recording.
//!
//! Deliberately `Viewport::Inline`, never fullscreen or the alternate screen — a
//! finished command should leave its output in scrollback like every other command
//! here. Uses the crossterm that `ratatui` re-exports so there is only ever one
//! version of it fighting over raw mode.

use anyhow::Result;
use nextbase_core::audio::{Levels, Recording};
use nextbase_core::shortcut;
use ratatui::crossterm::event::{
    self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, ModifierKeyCode,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement,
};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Gauge, Paragraph};
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::io::stdout;
use std::time::{Duration, Instant};

type Backend = ratatui::backend::CrosstermBackend<std::io::Stdout>;

/// Raw mode and keyboard enhancements are torn down on drop, so an early return or
/// a panic cannot leave the terminal unusable.
struct Session {
    terminal: Terminal<Backend>,
    enhanced: bool,
}

impl Session {
    fn new(height: u16) -> Result<Self> {
        let mut out = stdout();
        enable_raw_mode()?;

        // Ask the terminal whether it can report key releases and modifier keys
        // before relying on it. Writing the escape sequence always "succeeds", so
        // only this query distinguishes iTerm2/kitty/WezTerm from Apple Terminal.
        let enhanced = supports_keyboard_enhancement().unwrap_or(false);
        let _ = execute!(
            out,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            )
        );

        let terminal = Terminal::with_options(
            Backend::new(stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )?;

        Ok(Self { terminal, enhanced })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.enhanced {
            let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = disable_raw_mode();
        let _ = self.terminal.clear();
        let _ = self.terminal.show_cursor();
    }
}

// ------------------------------------------------------------ shortcut capture

/// Modifiers currently held down.
///
/// Tracked from `KeyCode::Modifier` press/release events rather than the bitmask
/// on a key event, because a bare modifier press carries no key — which is exactly
/// why pressing Ctrl+Win used to do nothing here.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Held {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub sup: bool,
}

impl Held {
    fn from_mods(mods: nextbase_core::hotkey::Mods) -> Self {
        Self {
            ctrl: mods.ctrl,
            alt: mods.alt,
            shift: mods.shift,
            sup: mods.meta,
        }
    }

    fn from_bitmask(modifiers: KeyModifiers) -> Self {
        Self {
            ctrl: modifiers.contains(KeyModifiers::CONTROL),
            alt: modifiers.contains(KeyModifiers::ALT),
            shift: modifiers.contains(KeyModifiers::SHIFT),
            sup: modifiers.contains(KeyModifiers::SUPER) || modifiers.contains(KeyModifiers::META),
        }
    }

    fn set(&mut self, key: ModifierKeyCode, down: bool) {
        match key {
            ModifierKeyCode::LeftControl | ModifierKeyCode::RightControl => self.ctrl = down,
            ModifierKeyCode::LeftAlt | ModifierKeyCode::RightAlt => self.alt = down,
            ModifierKeyCode::LeftShift | ModifierKeyCode::RightShift => self.shift = down,
            ModifierKeyCode::LeftSuper
            | ModifierKeyCode::RightSuper
            | ModifierKeyCode::LeftMeta
            | ModifierKeyCode::RightMeta => self.sup = down,
            _ => {}
        }
    }

    fn union(self, other: Self) -> Self {
        Self {
            ctrl: self.ctrl || other.ctrl,
            alt: self.alt || other.alt,
            shift: self.shift || other.shift,
            sup: self.sup || other.sup,
        }
    }

    pub(crate) fn count(&self) -> usize {
        [self.ctrl, self.alt, self.shift, self.sup]
            .iter()
            .filter(|held| **held)
            .count()
    }

    fn is_empty(&self) -> bool {
        self.count() == 0
    }

    /// The primary modifier is normally written `CommandOrControl`, so a captured
    /// shortcut still means the right thing on the other platform. When both
    /// primaries are held the distinction matters, so name them explicitly.
    pub(crate) fn labels(&self) -> Vec<&'static str> {
        let mac = cfg!(target_os = "macos");
        let both_primaries = self.ctrl && self.sup;
        let mut parts = Vec::new();

        if self.sup {
            parts.push(if !mac {
                "Win"
            } else if both_primaries {
                "Command"
            } else {
                "CommandOrControl"
            });
        }
        if self.ctrl {
            parts.push(if mac || both_primaries {
                "Ctrl"
            } else {
                "CommandOrControl"
            });
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        parts
    }
}

fn key_label(code: KeyCode) -> Option<String> {
    Some(match code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) => c.to_ascii_uppercase().to_string(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::Enter => "Enter".to_string(),
        _ => return None,
    })
}

/// F-keys stand alone; anything else needs a modifier or it would swallow ordinary
/// typing system-wide.
fn is_standalone(key: &str) -> bool {
    key.strip_prefix('F')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=24).contains(&n))
}

/// Builds the shortcut string shown while keys are going down, so each press
/// appears as it happens: `Ctrl`, then `Ctrl + Command`, then `Ctrl + Command + K`.
pub(crate) fn preview_of(held: Held, pending: bool) -> String {
    let parts = held.labels();
    if parts.is_empty() {
        return "press your shortcut".to_string();
    }
    let mut text = parts.join(" + ");
    if pending {
        text.push_str(" + …");
    }
    text
}

/// What the capture UI shows. Kept separate from the event loop so the rendering
/// can be tested against a `TestBackend` — an inline viewport needs a terminal that
/// answers a cursor-position query, which no test harness does.
pub(crate) struct CaptureView<'a> {
    pub label: &'a str,
    pub current: &'a str,
    pub preview: &'a str,
    pub note: Option<&'a str>,
    pub error: Option<&'a str>,
}

pub(crate) fn draw_capture(frame: &mut ratatui::Frame, view: &CaptureView) {
    let rows = Layout::vertical([Constraint::Length(1); 7]).split(frame.area());

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{} shortcut", view.label),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("   currently {}", view.current),
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        rows[0],
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            view.preview.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))),
        rows[2],
    );

    if let Some(note) = view.note {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                note.to_string(),
                Style::default().fg(Color::Green),
            ))),
            rows[3],
        );
    }

    if let Some(text) = view.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                text.to_string(),
                Style::default().fg(Color::Red),
            ))),
            rows[4],
        );
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Esc to type it instead   ·   Ctrl+C to cancel",
            Style::default().fg(Color::DarkGray),
        ))),
        rows[6],
    );
}

/// Capture a shortcut by pressing it. `Ok(None)` means the user chose to type it
/// instead.
///
/// Two gestures are accepted: modifiers plus a key, or modifiers alone released
/// together — the second is how `Ctrl+Command` style shortcuts get captured, and it
/// needs a terminal that reports modifier keys.
pub fn capture_shortcut(label: &str, current: &str) -> Result<Option<String>> {
    // The event tap sees modifier keys in any terminal, so prefer it. Reading stdin
    // can only see them where the terminal implements the kitty keyboard protocol,
    // which Apple Terminal does not.
    match capture_via_event_tap(label, current) {
        Ok(result) => return Ok(result),
        Err(error) => {
            let reason = error.to_string();
            if reason.starts_with("Cancelled") {
                return Err(error);
            }
            // No permission, or not macOS: fall through to reading stdin.
        }
    }
    capture_via_stdin(label, current)
}

/// Capture through the CGEventTap. Works regardless of terminal, and is the only
/// way to capture a modifier-only combo such as `Ctrl+Command`.
fn capture_via_event_tap(label: &str, current: &str) -> Result<Option<String>> {
    let (updates, handle) = nextbase_core::hotkey::capture()?;
    let mut session = Session::new(10)?;

    let mut held = Held::default();
    let mut peak = Held::default();
    let mut pressed_normal = false;
    let mut error: Option<String> = None;
    let mut captured: Option<String> = None;

    loop {
        let preview = preview_of(held, !held.is_empty());
        let note = if held.count() >= 2 && !pressed_normal {
            Some(format!(
                "Release both keys to use {} on its own, or press a key to add one.",
                held.labels().join(" + ")
            ))
        } else {
            None
        };
        let error_text = error.clone();

        session.terminal.draw(|frame| {
            draw_capture(
                frame,
                &CaptureView {
                    label,
                    current,
                    preview: &preview,
                    note: note.as_deref(),
                    error: error_text.as_deref(),
                },
            )
        })?;

        // Esc and Ctrl+C still come from stdin: the tap swallows key presses, so
        // this is the reliable way out.
        if event::poll(Duration::from_millis(40))? {
            if let event::Event::Key(KeyEvent {
                code,
                modifiers,
                kind,
                ..
            }) = event::read()?
            {
                if kind != KeyEventKind::Release {
                    if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
                        handle.stop();
                        return Err(anyhow::anyhow!("Cancelled."));
                    }
                    if code == KeyCode::Esc {
                        break;
                    }
                }
            }
        }

        let Ok(update) = updates.recv_timeout(Duration::from_millis(60)) else {
            continue;
        };

        match update {
            nextbase_core::hotkey::CaptureUpdate::Mods(mods) => {
                let next = Held::from_mods(mods);
                if held.is_empty() && !next.is_empty() {
                    // First modifier down starts a fresh gesture.
                    peak = Held::default();
                    pressed_normal = false;
                    error = None;
                }
                held = next;
                peak = peak.union(held);

                if held.is_empty() {
                    if !pressed_normal && peak.count() >= 2 {
                        let combo = peak.labels().join("+");
                        match shortcut::validate(&combo) {
                            Ok(()) => {
                                captured = Some(combo);
                                break;
                            }
                            Err(reason) => error = Some(reason.to_string()),
                        }
                    } else if !pressed_normal && peak.count() == 1 {
                        error = Some(
                            "One modifier on its own is not a shortcut. Hold two, or add a key."
                                .to_string(),
                        );
                    }
                    peak = Held::default();
                    pressed_normal = false;
                }
            }
            nextbase_core::hotkey::CaptureUpdate::Key { mods, key } => {
                // Esc arrives through the tap too; treat it as "type it instead".
                if key == "ESC" {
                    break;
                }
                held = held.union(Held::from_mods(mods));
                peak = peak.union(held);
                pressed_normal = true;

                let parts = held.labels();
                if parts.is_empty() && !is_standalone(&key) {
                    error = Some(format!(
                        "{key} alone would capture every keypress. Add a modifier, or use F13-F24."
                    ));
                    continue;
                }

                let mut combo = parts.join("+");
                if !combo.is_empty() {
                    combo.push('+');
                }
                combo.push_str(&key);

                match shortcut::validate(&combo) {
                    Ok(()) => {
                        captured = Some(combo);
                        break;
                    }
                    Err(reason) => error = Some(reason.to_string()),
                }
            }
        }
    }

    handle.stop();
    Ok(captured)
}

/// Fallback for when the event tap is unavailable: read stdin. Cannot see bare
/// modifier presses unless the terminal reports them.
fn capture_via_stdin(label: &str, current: &str) -> Result<Option<String>> {
    let mut session = Session::new(10)?;

    let mut held = Held::default();
    // Largest set held during this gesture, so releasing keys one at a time still
    // yields the whole combo.
    let mut peak = Held::default();
    let mut pressed_normal = false;
    let mut error: Option<String> = None;
    let mut captured: Option<String> = None;

    let unsupported_note = if session.enhanced {
        None
    } else {
        Some(
            "This terminal does not report modifier keys, so modifier-only combos cannot be captured. Press Esc to type one."
                .to_string(),
        )
    };

    loop {
        let preview = preview_of(held, !held.is_empty());
        let note = if held.count() >= 2 && !pressed_normal {
            Some(format!(
                "Release both keys to use {} on its own, or press a key to add one.",
                held.labels().join(" + ")
            ))
        } else {
            unsupported_note.clone()
        };
        let error_text = error.clone();

        session.terminal.draw(|frame| {
            draw_capture(
                frame,
                &CaptureView {
                    label,
                    current,
                    preview: &preview,
                    note: note.as_deref(),
                    error: error_text.as_deref(),
                },
            )
        })?;

        if !event::poll(Duration::from_millis(120))? {
            continue;
        }

        let event::Event::Key(KeyEvent {
            code,
            modifiers,
            kind,
            ..
        }) = event::read()?
        else {
            continue;
        };

        // Cancelling has to win over capturing Ctrl+C as a shortcut.
        if kind != KeyEventKind::Release
            && code == KeyCode::Char('c')
            && modifiers.contains(KeyModifiers::CONTROL)
        {
            return Err(anyhow::anyhow!("Cancelled."));
        }
        if kind != KeyEventKind::Release && code == KeyCode::Esc {
            break;
        }

        if let KeyCode::Modifier(modifier) = code {
            match kind {
                KeyEventKind::Press | KeyEventKind::Repeat => {
                    // First modifier down starts a fresh gesture.
                    if held.is_empty() {
                        peak = Held::default();
                        pressed_normal = false;
                    }
                    held.set(modifier, true);
                    peak = peak.union(held);
                    error = None;
                }
                KeyEventKind::Release => {
                    held.set(modifier, false);
                    if held.is_empty() {
                        if !pressed_normal && peak.count() >= 2 {
                            let combo = peak.labels().join("+");
                            match shortcut::validate(&combo) {
                                Ok(()) => {
                                    captured = Some(combo);
                                    break;
                                }
                                Err(reason) => error = Some(reason.to_string()),
                            }
                        } else if !pressed_normal && peak.count() == 1 {
                            error = Some(
                                "One modifier on its own is not a shortcut. Hold two, or add a key."
                                    .to_string(),
                            );
                        }
                        peak = Held::default();
                        pressed_normal = false;
                    }
                }
            }
            continue;
        }

        if kind == KeyEventKind::Release {
            continue;
        }

        // A real key carries a reliable modifier bitmask, which also covers
        // terminals that never send modifier key events.
        held = held.union(Held::from_bitmask(modifiers));
        peak = peak.union(held);

        let Some(key) = key_label(code) else { continue };
        pressed_normal = true;

        let parts = held.labels();
        if parts.is_empty() && !is_standalone(&key) {
            error = Some(format!(
                "{key} alone would capture every keypress. Add a modifier, or use F13-F24."
            ));
            continue;
        }

        let mut combo = parts.join("+");
        if !combo.is_empty() {
            combo.push('+');
        }
        combo.push_str(&key);

        match shortcut::validate(&combo) {
            Ok(()) => {
                captured = Some(combo);
                break;
            }
            Err(reason) => error = Some(reason.to_string()),
        }
    }

    Ok(captured)
}

// ----------------------------------------------------------------- level meter

/// Amplitude is logarithmic to the ear, and speech mostly sits between 0.01 and
/// 0.3. A linear bar would look dead, so scale to roughly -60 dB..0 dB.
fn meter_ratio(peak: f32) -> f64 {
    if peak <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * peak.log10();
    (((db + 60.0) / 60.0) as f64).clamp(0.0, 1.0)
}

fn meter_color(levels: Levels) -> Color {
    if levels.is_silent() {
        Color::DarkGray
    } else if levels.peak > 0.9 {
        Color::Red
    } else if levels.peak > 0.02 {
        Color::Green
    } else {
        Color::Yellow
    }
}

pub(crate) struct MeterView {
    pub levels: Levels,
    pub hint: String,
}

pub(crate) fn draw_meter(frame: &mut ratatui::Frame, view: &MeterView) {
    let rows = Layout::vertical([Constraint::Length(1); 5]).split(frame.area());

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("● ", Style::default().fg(Color::Red)),
            Span::styled("Recording", Style::default().add_modifier(Modifier::BOLD)),
        ])),
        rows[0],
    );

    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(meter_color(view.levels)))
            .ratio(meter_ratio(view.levels.peak))
            .label(format!(
                "peak {:.4}   rms {:.4}",
                view.levels.peak, view.levels.rms
            )),
        rows[2],
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            view.hint.clone(),
            Style::default().fg(Color::DarkGray),
        ))),
        rows[4],
    );
}

/// Draw a live meter while recording. Stops after `limit`, or on Enter/Esc when
/// `limit` is `None`.
pub fn record_with_meter(recording: &Recording, limit: Option<Duration>) -> Result<()> {
    let mut session = Session::new(7)?;
    let started = Instant::now();

    loop {
        if let Some(limit) = limit {
            if started.elapsed() >= limit {
                break;
            }
        }

        let levels = recording.live_levels();
        let elapsed = started.elapsed();
        let hint = match limit {
            Some(limit) => format!(
                "{:.1}s of {:.0}s   ·   Esc to stop early",
                elapsed.as_secs_f32(),
                limit.as_secs_f32()
            ),
            None => format!("{:.1}s   ·   Enter or Esc to stop", elapsed.as_secs_f32()),
        };

        session
            .terminal
            .draw(|frame| draw_meter(frame, &MeterView { levels, hint }))?;

        if event::poll(Duration::from_millis(60))? {
            if let event::Event::Key(KeyEvent { code, kind, .. }) = event::read()? {
                if kind != KeyEventKind::Release
                    && matches!(code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q'))
                {
                    break;
                }
            }
        }
    }

    // The caller reports the verdict from the finalized file, so nothing to add here.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    /// Render one frame off-screen and flatten it, so content can be asserted
    /// without a real terminal.
    fn rendered(width: u16, height: u16, draw: impl FnOnce(&mut ratatui::Frame)) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(draw).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn capture_ui_shows_state_and_the_way_out() {
        let output = rendered(80, 6, |frame| {
            draw_capture(
                frame,
                &CaptureView {
                    label: "Dictation",
                    current: "Ctrl+Window",
                    preview: "CommandOrControl + Alt + …",
                    note: None,
                    error: None,
                },
            )
        });
        assert!(output.contains("Dictation shortcut"));
        assert!(output.contains("currently Ctrl+Window"));
        assert!(output.contains("CommandOrControl + Alt"));
        assert!(output.contains("Esc to type it instead"));
    }

    #[test]
    fn capture_ui_surfaces_validation_errors() {
        let output = rendered(80, 6, |frame| {
            draw_capture(
                frame,
                &CaptureView {
                    label: "Polish",
                    current: "F15",
                    preview: "Alt + …",
                    note: None,
                    error: Some("Unsupported macOS shortcut key: F30."),
                },
            )
        });
        assert!(output.contains("Unsupported macOS shortcut key"));
    }

    #[test]
    fn meter_shows_levels_and_how_to_stop() {
        let output = rendered(80, 5, |frame| {
            draw_meter(
                frame,
                &MeterView {
                    levels: Levels {
                        peak: 0.0812,
                        rms: 0.0034,
                    },
                    hint: "1.2s   ·   Enter or Esc to stop".to_string(),
                },
            )
        });
        assert!(output.contains("Recording"));
        assert!(output.contains("peak 0.0812"));
        assert!(output.contains("rms 0.0034"));
        assert!(output.contains("Enter or Esc to stop"));
    }

    #[test]
    fn quiet_speech_is_visible_on_the_meter() {
        // A linear bar would put 0.02 at 2%; the dB scale lifts it into view.
        let quiet = meter_ratio(0.02);
        assert!(quiet > 0.25, "0.02 mapped to {quiet}");
        assert!(quiet < 0.6);
    }

    #[test]
    fn meter_ends_are_clamped() {
        assert_eq!(meter_ratio(0.0), 0.0);
        assert_eq!(meter_ratio(1.0), 1.0);
        assert_eq!(meter_ratio(2.0), 1.0);
        assert_eq!(meter_ratio(0.000001), 0.0);
    }

    #[test]
    fn only_f_keys_may_stand_alone() {
        assert!(is_standalone("F13"));
        assert!(is_standalone("F1"));
        assert!(!is_standalone("F25"));
        assert!(!is_standalone("A"));
        assert!(!is_standalone("Space"));
    }

    #[test]
    fn primary_modifier_is_written_portably() {
        let held = Held {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        let labels = held.labels();
        assert!(labels.contains(&"Alt"));
        // Whichever key is primary on this platform, it is spelled the portable way.
        assert!(labels.contains(&"CommandOrControl") || labels.contains(&"Ctrl"));
    }

    #[test]
    fn ctrl_plus_win_captures_as_a_modifier_only_shortcut() {
        // The reported bug: this combo did nothing in the capture UI.
        let held = Held {
            ctrl: true,
            sup: true,
            ..Default::default()
        };
        let combo = held.labels().join("+");
        assert!(
            shortcut::validate(&combo).is_ok(),
            "{combo} must be a valid shortcut"
        );
        // Whatever it is called on this platform, it means the same two physical keys
        // as the config the TypeScript build wrote.
        assert_eq!(
            shortcut::normalize(&combo),
            shortcut::normalize("Ctrl+Window")
        );
    }

    #[test]
    fn both_primaries_held_are_named_explicitly() {
        // "CommandOrControl+Ctrl" would be nonsense to read, so once both are down
        // they get their real names.
        let held = Held {
            ctrl: true,
            sup: true,
            ..Default::default()
        };
        let labels = held.labels();
        assert!(!labels.contains(&"CommandOrControl"), "{labels:?}");
        assert!(labels.contains(&"Ctrl"), "{labels:?}");
    }

    #[test]
    fn preview_grows_as_keys_go_down() {
        assert_eq!(preview_of(Held::default(), false), "press your shortcut");

        let one = Held {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(preview_of(one, true), format!("{} + …", one.labels()[0]));

        let two = Held {
            ctrl: true,
            sup: true,
            ..Default::default()
        };
        let shown = preview_of(two, true);
        assert!(shown.contains(" + "), "{shown}");
        assert!(shown.ends_with(" + …"), "{shown}");
        assert_eq!(preview_of(two, false), two.labels().join(" + "));
    }

    #[test]
    fn a_single_modifier_is_not_enough() {
        assert_eq!(
            Held {
                ctrl: true,
                ..Default::default()
            }
            .count(),
            1
        );
        assert_eq!(
            Held {
                ctrl: true,
                sup: true,
                ..Default::default()
            }
            .count(),
            2
        );
    }

    #[test]
    fn key_labels_cover_the_documented_keys() {
        assert_eq!(key_label(KeyCode::Char('a')).unwrap(), "A");
        assert_eq!(key_label(KeyCode::Char(' ')).unwrap(), "Space");
        assert_eq!(key_label(KeyCode::F(15)).unwrap(), "F15");
        assert!(key_label(KeyCode::Left).is_none());
    }
}
