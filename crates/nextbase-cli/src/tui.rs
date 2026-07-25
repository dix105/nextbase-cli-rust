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
    self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
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

        // Lets supported terminals report key releases and F13-F24, which is what
        // makes live modifier feedback and high F-keys possible at all. Apple
        // Terminal ignores it; the UI degrades to press-only.
        let enhanced = execute!(
            out,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            )
        )
        .is_ok();

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

/// The primary modifier is written as `CommandOrControl` so a captured shortcut
/// still means the right thing if the config moves between macOS and Windows.
fn modifier_labels(modifiers: KeyModifiers) -> Vec<&'static str> {
    let mut parts = Vec::new();
    let primary_is_super = cfg!(target_os = "macos");

    if modifiers.contains(KeyModifiers::SUPER) {
        parts.push(if primary_is_super {
            "CommandOrControl"
        } else {
            "Win"
        });
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        parts.push(if primary_is_super {
            "Ctrl"
        } else {
            "CommandOrControl"
        });
    }
    if modifiers.contains(KeyModifiers::ALT) {
        parts.push("Alt");
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("Shift");
    }
    parts
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

/// What the capture UI shows. Kept separate from the event loop so the rendering
/// can be tested against a `TestBackend` — an inline viewport needs a terminal that
/// answers a cursor-position query, which no test harness does.
pub(crate) struct CaptureView<'a> {
    pub label: &'a str,
    pub current: &'a str,
    pub preview: &'a str,
    pub error: Option<&'a str>,
}

pub(crate) fn draw_capture(frame: &mut ratatui::Frame, view: &CaptureView) {
    let rows = Layout::vertical([Constraint::Length(1); 6]).split(frame.area());

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

    if let Some(text) = view.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                text.to_string(),
                Style::default().fg(Color::Red),
            ))),
            rows[3],
        );
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Esc to type it instead   ·   Ctrl+C to cancel",
            Style::default().fg(Color::DarkGray),
        ))),
        rows[5],
    );
}

/// Capture a shortcut by pressing it. `Ok(None)` means the user chose to type it
/// instead, which is the escape hatch for modifier-only combos and for terminals
/// that swallow the key.
pub fn capture_shortcut(label: &str, current: &str) -> Result<Option<String>> {
    let mut session = Session::new(9)?;
    let mut held = KeyModifiers::NONE;
    let mut error: Option<String> = None;
    let mut captured: Option<String> = None;

    loop {
        let held_parts = modifier_labels(held);
        let preview = if held_parts.is_empty() {
            "hold Ctrl / Alt / Shift / Cmd, then press a key".to_string()
        } else {
            format!("{} + …", held_parts.join(" + "))
        };
        let error_text = error.clone();

        session.terminal.draw(|frame| {
            draw_capture(
                frame,
                &CaptureView {
                    label,
                    current,
                    preview: &preview,
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

        // Without keyboard enhancements only presses arrive, so `held` simply
        // tracks the modifiers reported with each press.
        if kind == KeyEventKind::Release {
            held = modifiers;
            continue;
        }
        held = modifiers;

        if code == KeyCode::Esc {
            break;
        }
        if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            // Ctrl+C is a plausible shortcut, but cancelling has to win.
            return Err(anyhow::anyhow!("Cancelled."));
        }

        let Some(key) = key_label(code) else { continue };
        let parts = modifier_labels(modifiers);

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
        let labels = modifier_labels(KeyModifiers::CONTROL | KeyModifiers::ALT);
        assert!(labels.contains(&"Alt"));
        // Whichever key is primary on this platform, it is spelled the portable way.
        assert!(labels.contains(&"CommandOrControl") || labels.contains(&"Ctrl"));
    }

    #[test]
    fn key_labels_cover_the_documented_keys() {
        assert_eq!(key_label(KeyCode::Char('a')).unwrap(), "A");
        assert_eq!(key_label(KeyCode::Char(' ')).unwrap(), "Space");
        assert_eq!(key_label(KeyCode::F(15)).unwrap(), "F15");
        assert!(key_label(KeyCode::Left).is_none());
    }
}
