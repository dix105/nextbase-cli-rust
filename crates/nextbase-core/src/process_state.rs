//! Listener process bookkeeping.
//!
//! The PID file only ever names the last listener that wrote it. An autostart
//! launcher can revive its own copy while a manually started one is still alive,
//! and both then register the same shortcuts — every press fires once per
//! listener. So stopping means finding listeners by command line, not just
//! trusting the file.

use anyhow::Result;
use sysinfo::{Signal, System};

use crate::paths;

/// Marks the in-process listener subcommand. Also matches the TypeScript
/// listener (`cli.js _listen`) so the two builds cannot double-register during
/// the migration.
const LISTENER_MARKER: &str = "_listen";

pub fn write_pid() -> Result<()> {
    std::fs::create_dir_all(paths::wisper_dir())?;
    std::fs::write(paths::pid_file(), std::process::id().to_string())?;
    Ok(())
}

pub fn read_pid() -> Option<u32> {
    std::fs::read_to_string(paths::pid_file())
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0)
}

pub fn clear_pid() {
    let _ = std::fs::remove_file(paths::pid_file());
}

/// Every live listener except this process.
pub fn other_listener_pids() -> Vec<u32> {
    let system = System::new_all();
    let own = std::process::id();

    let mut pids: Vec<u32> = system
        .processes()
        .iter()
        .filter(|(pid, process)| {
            if pid.as_u32() == own {
                return false;
            }
            let command: Vec<String> = process
                .cmd()
                .iter()
                .map(|part| part.to_string_lossy().to_lowercase())
                .collect();
            if !command.iter().any(|part| part == LISTENER_MARKER) {
                return false;
            }
            // Only our own listeners: the marker alone is too generic.
            command
                .iter()
                .any(|part| part.contains("wisper") || part.contains("nextbase") || part.contains("cli.js"))
        })
        .map(|(pid, _)| pid.as_u32())
        .collect();

    if let Some(recorded) = read_pid() {
        if recorded != own && !pids.contains(&recorded) {
            pids.push(recorded);
        }
    }

    pids.sort_unstable();
    pids.dedup();
    pids
}

/// Stop every other listener. Returns how many were signalled.
///
/// SIGTERM, not SIGKILL: the listener releases the microphone and clears its PID
/// file on the way out.
pub fn stop_other_listeners() -> usize {
    let system = System::new_all();
    let mut stopped = 0;

    for pid in other_listener_pids() {
        if let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) {
            if process.kill_with(Signal::Term).unwrap_or(false) {
                stopped += 1;
                continue;
            }
        }
        // Recorded in the PID file but already gone, or owned by another user.
    }

    if read_pid() != Some(std::process::id()) {
        clear_pid();
    }
    stopped
}

pub fn listener_is_running() -> bool {
    !other_listener_pids().is_empty()
}
