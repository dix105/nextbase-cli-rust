use clap::{Args, Parser, Subcommand};

/// `nextbase` is the umbrella CLI. Wisper is its first tool; more will sit
/// alongside it, so tool-level commands stay namespaced under `nextbase <tool>`.
#[derive(Debug, Parser)]
#[command(
    name = "nextbase",
    version,
    about = "Nextbase CLI — command-line tools for Nextbase",
    long_about = None,
    subcommand_negates_reqs = true
)]
pub struct Nextbase {
    #[command(subcommand)]
    pub tool: Option<Tool>,
}

#[derive(Debug, Subcommand)]
pub enum Tool {
    /// Wisper — hold-to-record dictation, paste, polish, spell fix
    Wisper(WisperArgs),
    /// Meeting Agent — record a meeting, transcribe it, get notes
    #[command(alias = "nbmeet")]
    Meeting(crate::meeting_cli::MeetingArgs),
}

#[derive(Debug, Parser)]
#[command(
    name = "wisper",
    version,
    about = "Wisper — hold-to-record dictation for your whole desktop"
)]
pub struct WisperCli {
    #[command(subcommand)]
    pub command: Option<WisperCommand>,
}

#[derive(Debug, Args)]
pub struct WisperArgs {
    #[command(subcommand)]
    pub command: Option<WisperCommand>,
}

#[derive(Debug, Subcommand)]
pub enum WisperCommand {
    /// First-time setup: model, API key, shortcut, and preferences
    Setup {
        /// Only ask for settings that are still missing
        #[arg(long)]
        update: bool,
    },
    /// Choose the dictation model: `model [name]`, e.g. `model saaras:v3`
    #[command(alias = "provider")]
    Model {
        /// Model or provider name. Omit to choose from a list.
        name: Option<String>,
    },
    /// Show the current setup
    Status,
    /// Show configured shortcuts and the keys each platform supports
    Shortcuts,
    /// Set the dictation shortcut, e.g. `wisper shortcut F15`
    Shortcut {
        /// Shortcut to set. Omit to be prompted.
        keys: Vec<String>,
    },
    /// Selected-text polish: `polish on|off|status|shortcut [key]` or `polish "text"`
    Polish {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Focused-input spell fix: `spell status|shortcut [key]` or `spell "text"`
    Spell {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Print transcript history
    History {
        /// How many entries to show
        limit: Option<usize>,
    },
    /// Save a transcript by hand
    Add {
        #[arg(trailing_var_arg = true)]
        text: Vec<String>,
    },
    /// Show listener logs
    Logs,
    /// Transcribe an audio file
    Transcribe { file: String },
    /// Lower system volume while recording: `media on|off|status|volume <n>|test`
    Media {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Control the startup listener: `autostart on|off|status`
    Autostart {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Install the latest release over this one
    #[command(alias = "upgrade")]
    Update {
        /// Report whether an update exists without installing it
        #[arg(long)]
        check: bool,
    },
    /// Control background update checks: `autoupdate on|off|status|check`
    #[command(alias = "auto-update")]
    Autoupdate {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Check permissions, microphone, and configuration
    Doctor,
    /// Record a test clip without the hotkey, for debugging capture
    Record {
        /// Seconds to record
        seconds: Option<u64>,
    },
    /// Pick the microphone
    Mic {
        /// Test microphones and pick the working one
        #[arg(long)]
        auto: bool,
    },
    /// Start the background listener
    Listen {
        /// Run in this terminal instead of detaching, for debugging
        #[arg(long)]
        foreground: bool,
    },
    /// Internal: the in-process listener that autostart launches
    #[command(hide = true, name = "_listen")]
    ListenInternal,
    /// Stop the background listener
    Stop,
    /// Restart the background listener
    Restart,
    /// Open the local web dashboard
    #[command(alias = "app")]
    Open { port: Option<u16> },
}
