use anyhow::Result;
use std::io::Write;

use crate::paths;

/// `2026-07-23T12:14:23.794Z` — byte-identical to `new Date().toISOString()`, so
/// the log file stays readable by the existing tooling.
pub fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Append one line to `wisper.log`.
///
/// The file must stay plain text: `startListenerAndReport` greps it for literal
/// markers like `Shortcut registered:`, and ANSI colour codes would break that.
/// Style terminal output at the call site instead, never here.
pub fn write_line(message: &str) -> Result<()> {
    std::fs::create_dir_all(paths::wisper_dir())?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::log_file())?;
    writeln!(file, "[{}] {}", timestamp(), message)?;
    Ok(())
}

/// Log to file and echo to the terminal — the listener runs detached, so the log
/// is the only channel that survives.
pub fn log(message: &str) {
    let _ = write_line(message);
    println!("{message}");
}

pub fn read_logs() -> String {
    std::fs::read_to_string(paths::log_file()).unwrap_or_else(|_| "No logs yet.".to_string())
}
