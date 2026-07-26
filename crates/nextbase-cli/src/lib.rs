pub mod cli;
pub mod commands;
pub mod dashboard;
pub mod listener;
pub mod meeting_cli;
pub mod tui;
pub mod ui;

use anyhow::Result;
use clap::Parser;
use cli::{Nextbase, Tool, WisperCli, WisperCommand};
use meeting_cli::MeetingCli;

pub async fn dispatch(command: Option<WisperCommand>) -> Result<()> {
    let Some(command) = command else {
        return wisper_overview();
    };

    match command {
        WisperCommand::Setup { update } => commands::setup(update).await,
        WisperCommand::Model { name } => commands::provider(name.as_deref()).await,
        WisperCommand::Status => commands::status(),
        WisperCommand::Shortcuts => commands::shortcuts(),
        WisperCommand::Shortcut { keys } => commands::set_shortcut(&keys),
        WisperCommand::Polish { args } => commands::polish(&args).await,
        WisperCommand::Spell { args } => commands::spell(&args).await,
        WisperCommand::History { limit } => commands::history(limit),
        WisperCommand::Add { text } => commands::add(&text),
        WisperCommand::Logs => commands::logs(),
        WisperCommand::Transcribe { file } => commands::transcribe(&file).await,
        WisperCommand::Media { args } => commands::media(&args),
        WisperCommand::Autostart { args } => commands::autostart(&args),
        WisperCommand::Update { check } => commands::update(check).await,
        WisperCommand::Autoupdate { args } => commands::autoupdate(&args).await,
        WisperCommand::Doctor => commands::doctor(),
        WisperCommand::Record { seconds } => commands::record(seconds),
        WisperCommand::Mic { auto } => commands::mic(auto),
        WisperCommand::Listen { foreground } => commands::listen(foreground).await,
        WisperCommand::ListenInternal => crate::listener::run().await,
        WisperCommand::Stop => commands::stop(),
        WisperCommand::Restart => commands::restart().await,
        WisperCommand::Open { port } => commands::open(port).await,
    }
}

/// Bare `wisper`: show state plus the few commands people actually reach for,
/// rather than a wall of every subcommand.
fn wisper_overview() -> Result<()> {
    commands::status()?;
    println!();
    ui::heading("Common commands");
    ui::info("wisper setup       First-time setup");
    ui::info("wisper listen      Start the background listener");
    ui::info("wisper shortcuts   Show configured shortcuts");
    ui::info("wisper history     Recent transcripts");
    println!();
    ui::hint("Full list: wisper --help");
    Ok(())
}

/// Bare `nextbase`: name the tools and how to reach them.
fn nextbase_overview() -> Result<()> {
    ui::heading("Nextbase CLI");
    println!();
    ui::info("wisper    Hold-to-record dictation, paste, polish, spell fix");
    ui::info("meeting   Record a meeting, transcribe it, get notes");
    println!();
    ui::heading("Usage");
    ui::info("nextbase wisper <command>    Run a Wisper command");
    ui::info("nextbase meeting <command>   Run a Meeting Agent command");
    ui::info("wisper <command>             Same thing, direct");
    ui::info("nbmeet <command>             Same thing, direct");
    println!();
    ui::hint("Start here: nextbase wisper setup — or nextbase meeting setup");
    Ok(())
}

pub async fn run_nextbase() -> Result<()> {
    match Nextbase::parse().tool {
        Some(Tool::Wisper(args)) => dispatch(args.command).await,
        Some(Tool::Meeting(args)) => meeting_cli::dispatch(args.command).await,
        None => nextbase_overview(),
    }
}

pub async fn run_meeting() -> Result<()> {
    meeting_cli::dispatch(MeetingCli::parse().command).await
}

pub async fn run_wisper() -> Result<()> {
    dispatch(WisperCli::parse().command).await
}
