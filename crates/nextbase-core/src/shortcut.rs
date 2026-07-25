//! Shortcut parsing, normalization and validation.
//!
//! Ported from `src/hotkey.ts`. This module is deliberately platform-parameterised
//! so both the macOS and Windows mappings stay testable from either host — the
//! shipped bugs here were exactly the kind unit tests catch.

use anyhow::{bail, Result};

const MODIFIERS: [&str; 4] = ["CTRL", "ALT", "SHIFT", "META"];

/// `CommandOrControl` means Command on macOS and Control everywhere else.
pub fn platform_control() -> &'static str {
    if cfg!(target_os = "macos") {
        "META"
    } else {
        "CTRL"
    }
}

fn normalize_key_with(key: &str, control: &str) -> String {
    let mut value = key.trim().to_uppercase();

    for prefix in ["LEFT ", "RIGHT "] {
        if let Some(rest) = value.strip_prefix(prefix) {
            value = rest.to_string();
        }
    }

    // Order matters: the compound spellings must resolve before the substrings
    // they contain (COMMANDORCONTROL before CONTROL, WINDOWS before WIN).
    for (from, to) in [
        ("COMMANDORCONTROL", control),
        ("COMMAND_OR_CONTROL", control),
        ("CONTROL", "CTRL"),
        ("COMMAND", "META"),
        ("CMD", "META"),
        ("WINDOWS", "META"),
        ("WINDOW", "META"),
        ("WIN", "META"),
        ("OPTION", "ALT"),
    ] {
        if value.contains(from) {
            value = value.replace(from, to);
        }
    }

    if value.contains(char::is_whitespace) {
        value = value.split_whitespace().collect::<Vec<_>>().join("SPACE");
    }

    value
}

pub fn normalize_with(shortcut: &str, control: &str) -> String {
    let mut parts: Vec<String> = shortcut
        .split('+')
        .map(|part| normalize_key_with(part, control))
        .filter(|part| !part.is_empty())
        .collect();
    parts.sort();
    parts.join("+")
}

/// Canonical form for comparisons. `Cmd+Alt+S` and `CommandOrControl+Alt+S` are
/// the same physical combo on macOS and must compare equal — registering both
/// makes a single press fire twice.
pub fn normalize(shortcut: &str) -> String {
    normalize_with(shortcut, platform_control())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    pub key: Option<String>,
}

impl Parsed {
    pub fn is_modifier_only(&self) -> bool {
        self.key.is_none()
    }
}

pub fn parse_with(shortcut: &str, control: &str) -> Result<Parsed> {
    let normalized = normalize_with(shortcut, control);
    let parts: Vec<&str> = normalized.split('+').filter(|p| !p.is_empty()).collect();
    let key = parts
        .iter()
        .find(|part| !MODIFIERS.contains(part))
        .map(|part| part.to_string());

    if key.is_none() && parts.len() < 2 {
        bail!("Shortcut needs a final key: {shortcut}");
    }

    Ok(Parsed {
        ctrl: parts.contains(&"CTRL"),
        alt: parts.contains(&"ALT"),
        shift: parts.contains(&"SHIFT"),
        meta: parts.contains(&"META"),
        key,
    })
}

pub fn parse(shortcut: &str) -> Result<Parsed> {
    parse_with(shortcut, platform_control())
}

pub fn mac_key_code(key: &str) -> Result<u16> {
    let code = match key {
        "A" => 0, "S" => 1, "D" => 2, "F" => 3, "H" => 4, "G" => 5, "Z" => 6, "X" => 7,
        "C" => 8, "V" => 9, "B" => 11, "Q" => 12, "W" => 13, "E" => 14, "R" => 15,
        "Y" => 16, "T" => 17, "O" => 31, "U" => 32, "I" => 34, "P" => 35, "L" => 37,
        "J" => 38, "K" => 40, "N" => 45, "M" => 46,
        "1" => 18, "2" => 19, "3" => 20, "4" => 21, "6" => 22, "5" => 23, "9" => 25,
        "7" => 26, "8" => 28, "0" => 29,
        "SPACE" => 49, "TAB" => 48, "ENTER" | "RETURN" => 36, "ESC" | "ESCAPE" => 53,
        "F1" => 122, "F2" => 120, "F3" => 99, "F4" => 118, "F5" => 96, "F6" => 97,
        "F7" => 98, "F8" => 100, "F9" => 101, "F10" => 109, "F11" => 103, "F12" => 111,
        "F13" => 105, "F14" => 107, "F15" => 113, "F16" => 106, "F17" => 64, "F18" => 79,
        "F19" => 80, "F20" => 90,
        other => bail!(
            "Unsupported macOS shortcut key: {other}. Use A-Z, 0-9, Space, Tab, Enter, Esc, or F1-F20."
        ),
    };
    Ok(code)
}

pub fn windows_virtual_key(key: &str) -> Result<u32> {
    if key.len() == 1 {
        let ch = key.chars().next().unwrap();
        if ch.is_ascii_uppercase() || ch.is_ascii_digit() {
            return Ok(ch as u32);
        }
    }

    let code = match key {
        "SPACE" => 0x20,
        "TAB" => 0x09,
        "ENTER" | "RETURN" => 0x0d,
        "ESC" | "ESCAPE" => 0x1b,
        other => {
            if let Some(digits) = other.strip_prefix('F') {
                if let Ok(n) = digits.parse::<u32>() {
                    if (1..=24).contains(&n) {
                        return Ok(0x70 + n - 1);
                    }
                }
            }
            bail!(
                "Unsupported Windows shortcut key: {other}. Use A-Z, 0-9, Space, Tab, Enter, Esc, or F1-F24."
            )
        }
    };
    Ok(code)
}

/// Reject anything the listener could not register later. A shortcut that only
/// fails at listener start is far worse than one rejected at set time.
pub fn validate_with(shortcut: &str, target_os: &str) -> Result<()> {
    let control = if target_os == "macos" { "META" } else { "CTRL" };
    let parsed = parse_with(shortcut, control)?;

    let Some(key) = parsed.key.as_deref() else {
        if target_os == "macos" || target_os == "windows" {
            return Ok(());
        }
        bail!(
            "Modifier-only shortcuts like {shortcut} are only supported on Windows and macOS. Add a final key, e.g. Ctrl+Alt+Space."
        );
    };

    match target_os {
        "macos" => mac_key_code(key).map(|_| ()),
        "windows" => windows_virtual_key(key).map(|_| ()),
        _ => Ok(()),
    }
}

pub fn validate(shortcut: &str) -> Result<()> {
    validate_with(shortcut, std::env::consts::OS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_combo_written_differently_compares_equal_on_macos() {
        // The bug this guards: both spellings were registered as separate taps,
        // so one press fired the handler twice.
        assert_eq!(
            normalize_with("Cmd+Alt+S", "META"),
            normalize_with("CommandOrControl+Alt+S", "META")
        );
        assert_eq!(
            normalize_with("Ctrl+Alt+S", "CTRL"),
            normalize_with("CommandOrControl+Alt+S", "CTRL")
        );
    }

    #[test]
    fn command_or_control_follows_the_platform() {
        assert_eq!(normalize_with("CommandOrControl+P", "META"), "META+P");
        assert_eq!(normalize_with("CommandOrControl+P", "CTRL"), "CTRL+P");
    }

    #[test]
    fn ordering_and_spacing_do_not_change_identity() {
        assert_eq!(
            normalize_with("Alt+Ctrl+Space", "CTRL"),
            normalize_with("Ctrl+Alt+Space", "CTRL")
        );
        assert_eq!(
            normalize_with(" ctrl + alt + space ", "CTRL"),
            "ALT+CTRL+SPACE"
        );
    }

    #[test]
    fn window_spellings_all_mean_meta() {
        for spelling in ["Ctrl+Window", "Ctrl+Windows", "Ctrl+Win", "Ctrl+Cmd"] {
            assert_eq!(normalize_with(spelling, "CTRL"), "CTRL+META", "{spelling}");
        }
    }

    #[test]
    fn modifier_only_shortcuts_parse_without_a_key() {
        let parsed = parse_with("Ctrl+Window", "CTRL").unwrap();
        assert!(parsed.is_modifier_only());
        assert!(parsed.ctrl && parsed.meta);
        assert!(!parsed.alt && !parsed.shift);
    }

    #[test]
    fn a_lone_modifier_is_rejected() {
        assert!(parse_with("Ctrl", "CTRL").is_err());
    }

    #[test]
    fn keys_the_listener_cannot_register_are_rejected_up_front() {
        // Exactly what bricked the listener in the field: a backtick shortcut
        // reached config, then threw at every listener start.
        for bad in ["CommandOrControl+`", "Ctrl+Alt+~", "CommandOrControl+F30"] {
            assert!(
                validate_with(bad, "macos").is_err(),
                "{bad} must be rejected"
            );
        }
        assert!(validate_with("CommandOrControl+F30", "windows").is_err());
    }

    #[test]
    fn supported_keys_are_accepted() {
        for good in [
            "F13",
            "CommandOrControl+Shift+P",
            "Ctrl+Alt+Space",
            "Ctrl+Window",
        ] {
            assert!(
                validate_with(good, "macos").is_ok(),
                "{good} must be accepted"
            );
        }
        for good in ["F24", "Ctrl+Alt+Space", "Ctrl+Window"] {
            assert!(
                validate_with(good, "windows").is_ok(),
                "{good} must be accepted"
            );
        }
    }

    #[test]
    fn modifier_only_is_rejected_on_linux() {
        assert!(validate_with("Ctrl+Window", "linux").is_err());
    }

    #[test]
    fn mac_and_windows_tables_agree_on_the_documented_ranges() {
        assert!(mac_key_code("F20").is_ok());
        assert!(mac_key_code("F21").is_err());
        assert!(windows_virtual_key("F24").is_ok());
        assert!(windows_virtual_key("F25").is_err());
    }
}
