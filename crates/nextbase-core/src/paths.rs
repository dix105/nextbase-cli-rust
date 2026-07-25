use std::path::PathBuf;

/// Resolved the same way Node's `os.homedir()` does on each platform, so an
/// overridden `HOME` in tests points both CLIs at the same sandbox.
pub fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let key = "USERPROFILE";
    #[cfg(not(windows))]
    let key = "HOME";

    std::env::var_os(key)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn wisper_dir() -> PathBuf {
    home_dir().join(".wisper-cli")
}

pub fn config_file() -> PathBuf {
    wisper_dir().join("config.json")
}

pub fn history_file() -> PathBuf {
    wisper_dir().join("history.json")
}

pub fn log_file() -> PathBuf {
    wisper_dir().join("wisper.log")
}

pub fn pid_file() -> PathBuf {
    wisper_dir().join("listener.pid")
}

pub fn tmp_dir() -> PathBuf {
    wisper_dir().join("tmp")
}

pub fn installed_sha_file() -> PathBuf {
    wisper_dir().join("installed-sha")
}

/// Meeting Agent state and deliverables.
///
/// Separate from `~/.wisper-cli` because these are files a person opens — notes,
/// transcripts, recordings — not a tool's internal state. Config and API keys stay
/// shared in `~/.wisper-cli/config.json`, so a key saved by `wisper setup` works here.
pub fn nextbase_dir() -> PathBuf {
    home_dir().join(".nextbase")
}

pub fn meetings_dir() -> PathBuf {
    nextbase_dir().join("meetings")
}

pub fn meeting_dir(id: &str) -> PathBuf {
    meetings_dir().join(id)
}

pub fn active_meeting_file() -> PathBuf {
    nextbase_dir().join("active-meeting.json")
}

pub fn meeting_log_file() -> PathBuf {
    nextbase_dir().join("meeting.log")
}
