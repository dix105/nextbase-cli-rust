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
