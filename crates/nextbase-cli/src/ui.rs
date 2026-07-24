//! Terminal styling.
//!
//! Everything here writes through `anstream`, which strips colour automatically
//! when output is piped or `NO_COLOR` is set. Never route log-file content
//! through these helpers — `wisper.log` is grepped for literal markers.

use owo_colors::OwoColorize;

pub fn heading(text: &str) {
    anstream::println!("{}", text.bold());
}

pub fn success(text: &str) {
    anstream::println!("{} {}", "✓".green().bold(), text);
}

pub fn warn(text: &str) {
    anstream::println!("{} {}", "!".yellow().bold(), text);
}

pub fn failure(text: &str) {
    anstream::println!("{} {}", "✗".red().bold(), text);
}

pub fn info(text: &str) {
    anstream::println!("  {}", text);
}

/// `  Label: value` with a dim label, used by `status` and `shortcuts`.
pub fn field(label: &str, value: &str) {
    anstream::println!("  {:<20} {}", format!("{label}:").dimmed(), value);
}

pub fn hint(text: &str) {
    anstream::println!("{}", text.dimmed());
}

pub fn spinner(message: &str) -> indicatif::ProgressBar {
    let bar = indicatif::ProgressBar::new_spinner();
    bar.enable_steady_tick(std::time::Duration::from_millis(90));
    bar.set_style(
        indicatif::ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| indicatif::ProgressStyle::default_spinner()),
    );
    bar.set_message(message.to_string());
    bar
}
