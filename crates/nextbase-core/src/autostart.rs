//! Start the listener at login, and start it detached from the terminal.
//!
//! Two lessons from the TypeScript build are baked in here:
//!
//! 1. A KeepAlive launcher owns the listener. Killing its child just makes it
//!    respawn, so restarting must go through the launcher, never around it.
//! 2. The old LaunchAgent captured no output, so a listener that crashed on every
//!    start looked like silence. This one logs stdout and stderr to a file.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::paths;

/// Deliberately not `com.wisper.cli`: that label belongs to the TypeScript build,
/// and overwriting its plist would hijack a working install.
pub const LAUNCH_AGENT_LABEL: &str = "com.nextbase.wisper";
pub const LEGACY_LAUNCH_AGENT_LABEL: &str = "com.wisper.cli";

pub struct AutostartResult {
    pub enabled: bool,
    pub message: String,
}

fn launch_agents_dir() -> PathBuf {
    paths::home_dir().join("Library").join("LaunchAgents")
}

pub fn launch_agent_file() -> PathBuf {
    launch_agents_dir().join(format!("{LAUNCH_AGENT_LABEL}.plist"))
}

pub fn legacy_launch_agent_file() -> PathBuf {
    launch_agents_dir().join(format!("{LEGACY_LAUNCH_AGENT_LABEL}.plist"))
}

/// The TypeScript autostart entry. While it exists, both builds would register the
/// same shortcuts and every press would fire twice.
pub fn legacy_autostart_present() -> bool {
    cfg!(target_os = "macos") && legacy_launch_agent_file().exists()
}

pub fn launchd_log_file() -> PathBuf {
    paths::wisper_dir().join("launchd.log")
}

fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("Could not determine the path of this binary")
}

/// Spawn the listener in a new session so it outlives the terminal that started it.
pub fn spawn_detached() -> Result<u32> {
    let exe = current_exe()?;
    let mut command = Command::new(exe);
    command
        .arg("_listen")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        // Without setsid the child stays in the terminal's process group and dies
        // of SIGHUP when the window closes.
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command.spawn().context("Could not start the listener")?;
    Ok(child.id())
}

// -------------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    fn launchd_target() -> String {
        format!("gui/{}/{LAUNCH_AGENT_LABEL}", unsafe { libc::getuid() })
    }

    fn plist(exe: &std::path::Path) -> String {
        let log = launchd_log_file();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key><string>{LAUNCH_AGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
      <string>{}</string>
      <string>_listen</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ThrottleInterval</key><integer>10</integer>
    <key>StandardOutPath</key><string>{}</string>
    <key>StandardErrorPath</key><string>{}</string>
  </dict>
</plist>
"#,
            exe.display(),
            log.display(),
            log.display()
        )
    }

    pub fn enable() -> Result<AutostartResult> {
        let exe = current_exe()?;
        let file = launch_agent_file();
        std::fs::create_dir_all(launch_agents_dir())?;
        std::fs::write(&file, plist(&exe))?;

        // bootout first so a changed binary path takes effect.
        let _ = Command::new("launchctl")
            .args(["bootout", &launchd_target()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        let uid = unsafe { libc::getuid() };
        let status = Command::new("launchctl")
            .args(["bootstrap", &format!("gui/{uid}"), &file.to_string_lossy()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        let ok = status.map(|s| s.success()).unwrap_or(false);
        if ok {
            return Ok(AutostartResult {
                enabled: true,
                message: "Autostart enabled with a LaunchAgent.".into(),
            });
        }

        // Older macOS releases only understand load/unload.
        let legacy = Command::new("launchctl")
            .args(["load", &file.to_string_lossy()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        Ok(AutostartResult {
            enabled: legacy.map(|s| s.success()).unwrap_or(false),
            message: format!("Autostart file written to {}.", file.display()),
        })
    }

    pub fn disable() -> Result<AutostartResult> {
        let _ = Command::new("launchctl")
            .args(["bootout", &launchd_target()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let file = launch_agent_file();
        let _ = Command::new("launchctl")
            .args(["unload", &file.to_string_lossy()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_file(&file);

        Ok(AutostartResult {
            enabled: false,
            message: "Autostart disabled. LaunchAgent removed.".into(),
        })
    }

    pub fn status() -> Result<AutostartResult> {
        let file = launch_agent_file();
        Ok(if file.exists() {
            AutostartResult {
                enabled: true,
                message: format!("Autostart enabled: {}", file.display()),
            }
        } else {
            AutostartResult {
                enabled: false,
                message: "Autostart disabled: no LaunchAgent found.".into(),
            }
        })
    }

    /// launchd owns the process, so a restart has to go through it.
    pub fn restart() -> bool {
        Command::new("launchctl")
            .args(["kickstart", "-k", &launchd_target()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub fn managed() -> bool {
        launch_agent_file().exists()
    }
}

// ------------------------------------------------------------------ Windows

#[cfg(windows)]
mod platform {
    use super::*;

    const TASK_NAME: &str = "NextbaseWisper";

    fn startup_dir() -> PathBuf {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| paths::home_dir().join("AppData").join("Roaming"));
        base.join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
    }

    fn startup_file() -> PathBuf {
        startup_dir().join(format!("{TASK_NAME}.vbs"))
    }

    /// A VBScript shim keeps the console window hidden. Launching the exe from the
    /// Startup folder directly would flash a window at every login.
    fn shim(exe: &std::path::Path) -> String {
        format!(
            "Set WshShell = CreateObject(\"WScript.Shell\")\r\nWshShell.Run \"\"\"{}\"\" _listen\", 0, False\r\n",
            exe.display()
        )
    }

    pub fn enable() -> Result<AutostartResult> {
        let exe = current_exe()?;
        std::fs::create_dir_all(startup_dir())?;
        std::fs::write(startup_file(), shim(&exe))?;

        // A logon Scheduled Task is preferred, but some policies reject it with
        // 0x80070005; the Startup folder shim above needs no privileges.
        let script = format!(
            "$A = New-ScheduledTaskAction -Execute 'wscript.exe' -Argument '//B \"{}\"'; \
             $T = New-ScheduledTaskTrigger -AtLogOn; \
             $S = New-ScheduledTaskSettingsSet -Hidden -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries; \
             Register-ScheduledTask -TaskName '{TASK_NAME}' -Action $A -Trigger $T -Settings $S -Force | Out-Null",
            startup_file().display()
        );
        let status = Command::new("powershell.exe")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        Ok(AutostartResult {
            enabled: true,
            message: if status.map(|s| s.success()).unwrap_or(false) {
                "Autostart enabled as a hidden logon task.".into()
            } else {
                "Autostart enabled via the Startup folder (scheduled task was blocked).".into()
            },
        })
    }

    pub fn disable() -> Result<AutostartResult> {
        let _ = Command::new("schtasks.exe")
            .args(["/Delete", "/TN", TASK_NAME, "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_file(startup_file());
        Ok(AutostartResult {
            enabled: false,
            message: "Autostart disabled. Startup entries removed.".into(),
        })
    }

    pub fn status() -> Result<AutostartResult> {
        let task = Command::new("schtasks.exe")
            .args(["/Query", "/TN", TASK_NAME])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let shim_exists = startup_file().exists();

        Ok(AutostartResult {
            enabled: task || shim_exists,
            message: match (task, shim_exists) {
                (true, _) => "Autostart enabled: logon task registered.".into(),
                (false, true) => "Autostart enabled: Startup folder entry present.".into(),
                _ => "Autostart disabled: no logon task or Startup entry.".into(),
            },
        })
    }

    /// Nothing revives the listener on Windows, so a plain respawn is enough.
    pub fn restart() -> bool {
        super::spawn_detached().is_ok()
    }

    pub fn managed() -> bool {
        false
    }
}

// -------------------------------------------------------------------- Linux

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use super::*;

    fn service_file() -> PathBuf {
        paths::home_dir()
            .join(".config")
            .join("systemd")
            .join("user")
            .join("nextbase-wisper.service")
    }

    pub fn enable() -> Result<AutostartResult> {
        let exe = current_exe()?;
        let file = service_file();
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(
            &file,
            format!(
                "[Unit]\nDescription=Wisper background listener\nAfter=default.target\n\n\
                 [Service]\nExecStart={} _listen\nRestart=always\nRestartSec=3\n\n\
                 [Install]\nWantedBy=default.target\n",
                exe.display()
            ),
        )?;

        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
        let status = Command::new("systemctl")
            .args(["--user", "enable", "nextbase-wisper.service"])
            .status();

        Ok(AutostartResult {
            enabled: status.map(|s| s.success()).unwrap_or(false),
            message: format!("Autostart service written to {}.", file.display()),
        })
    }

    pub fn disable() -> Result<AutostartResult> {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "nextbase-wisper.service"])
            .status();
        let _ = std::fs::remove_file(service_file());
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
        Ok(AutostartResult {
            enabled: false,
            message: "Autostart disabled. systemd user service removed.".into(),
        })
    }

    pub fn status() -> Result<AutostartResult> {
        Ok(if service_file().exists() {
            AutostartResult {
                enabled: true,
                message: format!("Autostart enabled: {}", service_file().display()),
            }
        } else {
            AutostartResult {
                enabled: false,
                message: "Autostart disabled: no systemd user service found.".into(),
            }
        })
    }

    pub fn restart() -> bool {
        Command::new("systemctl")
            .args(["--user", "restart", "nextbase-wisper.service"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub fn managed() -> bool {
        service_file().exists()
    }
}

pub use platform::{disable, enable, managed, restart, status};
