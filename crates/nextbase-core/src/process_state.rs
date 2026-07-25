//! Listener process bookkeeping.
//!
//! The PID file only ever names the last listener that wrote it. An autostart
//! launcher can revive its own copy while a manually started one is still alive,
//! and both then register the same shortcuts — every press fires once per
//! listener. So stopping means finding listeners by command line, not just
//! trusting the file.

use anyhow::Result;
// Signals do not exist on Windows, so the import would be unused there.
#[cfg(not(windows))]
use sysinfo::Signal;
use sysinfo::System;

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

/// Is this process one of our listeners?
///
/// Matching is deliberately narrow: the executable itself must be one of ours and
/// `_listen` must appear in its arguments. An earlier version matched any process
/// whose command line merely mentioned both, which meant a shell script or a grep
/// referring to `wisper _listen` could be terminated.
pub(crate) fn is_listener_command(name: &str, cmd: &[String]) -> bool {
    let stem = name.to_lowercase();
    let stem = stem.trim_end_matches(".exe");

    let joined = cmd.join(" ").to_lowercase();
    // Skip argv[0]: macOS reports argv as separate entries and Windows as a single
    // string, so drop the leading executable path either way.
    let arguments = joined
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest)
        .unwrap_or_default();

    let has_marker = arguments
        .split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
        .any(|token| token == LISTENER_MARKER);
    if !has_marker {
        return false;
    }

    match stem {
        "wisper" | "nextbase" => true,
        // The TypeScript build runs as `node .../dist/cli.js _listen`.
        "node" => arguments.contains("cli.js"),
        _ => false,
    }
}

fn is_listener(process: &sysinfo::Process) -> bool {
    let cmd: Vec<String> = process
        .cmd()
        .iter()
        .map(|part| part.to_string_lossy().to_string())
        .collect();
    is_listener_command(&process.name().to_string_lossy(), &cmd)
}

/// Every live listener except this process.
pub fn other_listener_pids() -> Vec<u32> {
    let system = System::new_all();
    let own = std::process::id();

    let mut pids: Vec<u32> = system
        .processes()
        .iter()
        .filter(|(pid, process)| pid.as_u32() != own && is_listener(process))
        .map(|(pid, _)| pid.as_u32())
        .collect();

    // The PID file is the fallback for when the command line cannot be read, which
    // Windows can refuse for some processes.
    if let Some(recorded) = read_pid() {
        if recorded != own
            && !pids.contains(&recorded)
            && system.process(sysinfo::Pid::from_u32(recorded)).is_some()
        {
            pids.push(recorded);
        }
    }

    pids.sort_unstable();
    pids.dedup();
    pids
}

/// End one listener.
///
/// Windows has no signals for sysinfo to send — `kill_with` returns `None` there,
/// which used to be read as "kill failed", so nothing was terminated and the caller
/// reported no listener at all while listeners piled up.
fn terminate(process: &sysinfo::Process) -> bool {
    #[cfg(windows)]
    {
        // TerminateProcess. No graceful unwind is available on Windows.
        process.kill()
    }
    #[cfg(not(windows))]
    {
        // SIGTERM first, so the listener releases the microphone and clears its
        // PID file on the way out.
        process
            .kill_with(Signal::Term)
            .unwrap_or_else(|| process.kill())
    }
}

/// Stop every other listener, and confirm they are gone.
///
/// Returns how many actually exited, not how many kills were attempted.
pub fn stop_other_listeners() -> usize {
    let targets = other_listener_pids();
    if targets.is_empty() {
        if read_pid() != Some(std::process::id()) {
            clear_pid();
        }
        return 0;
    }

    let system = System::new_all();
    for pid in &targets {
        if let Some(process) = system.process(sysinfo::Pid::from_u32(*pid)) {
            terminate(process);
        }
    }

    // Verify rather than trust: a silently failed kill is what let listeners stack
    // up, each one pasting the same dictation.
    let mut remaining = targets.clone();
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let alive = System::new_all();
        remaining.retain(|pid| alive.process(sysinfo::Pid::from_u32(*pid)).is_some());
        if remaining.is_empty() {
            break;
        }
    }

    if read_pid() != Some(std::process::id()) {
        clear_pid();
    }
    targets.len() - remaining.len()
}

/// Listeners that survived a stop attempt.
pub fn stubborn_listeners() -> Vec<u32> {
    other_listener_pids()
}

pub fn listener_is_running() -> bool {
    !other_listener_pids().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn our_listeners_are_recognised() {
        assert!(is_listener_command(
            "wisper",
            &cmd(&["/Users/me/.local/bin/wisper", "_listen"])
        ));
        assert!(is_listener_command(
            "nextbase",
            &cmd(&["nextbase", "_listen"])
        ));
        // Windows reports the whole command line as one string.
        assert!(is_listener_command(
            "wisper.exe",
            &cmd(&["C:\\Users\\pc\\wisper.exe _listen"])
        ));
        // The TypeScript build.
        assert!(is_listener_command(
            "node",
            &cmd(&["node", "/home/me/.wisper-cli/app/dist/cli.js", "_listen"])
        ));
    }

    #[test]
    fn unrelated_processes_are_never_matched() {
        // This is the case that mattered: a shell running a script that merely
        // mentions the listener used to be killed by the sweep.
        assert!(!is_listener_command(
            "bash",
            &cmd(&["bash", "-c", "wisper _listen; echo done"])
        ));
        assert!(!is_listener_command(
            "pgrep",
            &cmd(&["pgrep", "-f", "wisper _listen"])
        ));
        assert!(!is_listener_command(
            "grep",
            &cmd(&["grep", "_listen", "notes.txt"])
        ));
        // An unrelated node process must not be mistaken for the old build.
        assert!(!is_listener_command(
            "node",
            &cmd(&["node", "server.js", "_listen"])
        ));
    }

    #[test]
    fn other_wisper_commands_are_left_alone() {
        for arguments in [
            cmd(&["wisper", "stop"]),
            cmd(&["wisper", "listen"]),
            cmd(&["wisper", "open"]),
            cmd(&["wisper"]),
        ] {
            assert!(
                !is_listener_command("wisper", &arguments),
                "{arguments:?} is not the in-process listener"
            );
        }
    }
}
