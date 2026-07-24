//! Text rewriting through Groq chat completions: auto-polish before paste, the
//! selected-text polish shortcut, and the focused-input spell fix.

use anyhow::{bail, Context, Result};

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteMode {
    Clean,
    Spell,
    Polish,
    Professional,
    Shorter,
    Friendly,
}

impl RewriteMode {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "clean" => Some(Self::Clean),
            "spell" => Some(Self::Spell),
            "polish" => Some(Self::Polish),
            "professional" => Some(Self::Professional),
            "shorter" => Some(Self::Shorter),
            "friendly" => Some(Self::Friendly),
            _ => None,
        }
    }

    fn instruction(&self) -> &'static str {
        match self {
            Self::Clean => "Clean up dictation artifacts, punctuation, grammar, and structure. Preserve the speaker's meaning.",
            // Deliberately narrow: the spell shortcut replaces the user's whole
            // input field, so it must not reword anything.
            Self::Spell => "Correct spelling mistakes, capitalization, and obvious punctuation only. Do not rewrite, summarize, translate, change wording, or alter the tone.",
            Self::Polish => "Polish this written text. Fix grammar, punctuation, spelling, clarity, and sentence flow while preserving the original voice, tone, and meaning. Do not make it more formal unless needed.",
            Self::Professional => "Rewrite the text to sound clear, polished, and professional. Preserve the meaning.",
            Self::Shorter => "Make the text shorter and punchier. Preserve the core meaning.",
            Self::Friendly => "Rewrite the text to sound warm, friendly, and natural. Preserve the meaning.",
        }
    }
}

/// Tidy dictation output before it is pasted or sent for rewriting: drop spaces
/// before punctuation, collapse runs of whitespace, trim stray edge punctuation.
pub fn normalize_transcript_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());

    for ch in text.chars() {
        if matches!(ch, ',' | '.' | ':' | ';' | '!' | '?') {
            while out.ends_with(char::is_whitespace) {
                out.pop();
            }
            out.push(ch);
            continue;
        }

        if ch.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
            continue;
        }

        out.push(ch);
    }

    out.trim_matches(|c: char| c.is_whitespace() || matches!(c, ',' | '.' | ':' | ';' | '!' | '?' | '-'))
        .to_string()
}

pub async fn rewrite_text(text: &str, config: &Config, mode: RewriteMode) -> Result<String> {
    let input = normalize_transcript_text(text);
    if input.is_empty() {
        bail!("Nothing to rewrite yet.");
    }

    let key = config
        .key_for(crate::config::Provider::Groq)
        .filter(|key| !key.is_empty())
        .context("Rewriting needs a Groq API key. Run: wisper polish on")?;

    let body = serde_json::json!({
        "model": config.polish_model_or_default(),
        "temperature": 0.2,
        "messages": [
            {
                "role": "system",
                "content": "You are a dictation cleanup engine. Return only the rewritten text, with no explanation."
            },
            {
                "role": "user",
                "content": format!("{}\n\nText:\n{}", mode.instruction(), input)
            }
        ]
    });

    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?
        .post("https://api.groq.com/openai/v1/chat/completions")
        .bearer_auth(key)
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    let payload: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);

    if !status.is_success() {
        let message = payload
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .map(|m| m.to_string())
            .unwrap_or_else(|| format!("Groq rewrite failed: HTTP {}", status.as_u16()));
        bail!("{message}");
    }

    let rewritten = payload
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();

    if rewritten.is_empty() {
        bail!("Groq returned an empty rewrite.");
    }
    Ok(rewritten)
}

/// Used by the listener before pasting. Falls back to the cleaned transcript so a
/// polish failure never costs the user their dictation.
pub async fn polish_dictation_if_enabled(text: &str, config: &Config) -> String {
    let cleaned = normalize_transcript_text(text);
    if config.auto_polish != Some(true) {
        return cleaned;
    }

    match rewrite_text(&cleaned, config, RewriteMode::Polish).await {
        Ok(polished) if !polished.trim().is_empty() => polished,
        _ => cleaned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spaces_before_punctuation_are_removed() {
        assert_eq!(normalize_transcript_text("hello , world ."), "hello, world");
    }

    #[test]
    fn whitespace_runs_collapse() {
        assert_eq!(normalize_transcript_text("too   many    spaces"), "too many spaces");
        assert_eq!(normalize_transcript_text("line\n\nbreak"), "line break");
    }

    #[test]
    fn edge_punctuation_and_whitespace_are_trimmed() {
        assert_eq!(normalize_transcript_text("  ...hello world!  "), "hello world");
        assert_eq!(normalize_transcript_text("- dash lead"), "dash lead");
    }

    #[test]
    fn inner_punctuation_survives() {
        assert_eq!(
            normalize_transcript_text("first. second, third"),
            "first. second, third"
        );
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(normalize_transcript_text("   ,,, "), "");
    }

    #[test]
    fn mode_names_round_trip() {
        assert_eq!(RewriteMode::from_name("spell"), Some(RewriteMode::Spell));
        assert_eq!(RewriteMode::from_name("nonsense"), None);
    }
}
