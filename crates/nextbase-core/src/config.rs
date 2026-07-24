use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use crate::paths;

pub const DEFAULT_SHORTCUT: &str = "Ctrl+Alt+Space";
pub const DEFAULT_POLISH_SHORTCUT: &str = "CommandOrControl+Shift+P";
pub const DEFAULT_SPELL_SHORTCUT: &str = "CommandOrControl+Alt+S";
pub const DEFAULT_POLISH_MODEL: &str = "llama-3.3-70b-versatile";
pub const DEFAULT_DUCKING_VOLUME: u8 = 35;
pub const DEFAULT_UPDATE_INTERVAL_MINUTES: u64 = 180;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Provider {
    #[serde(rename = "groq")]
    Groq,
    #[serde(rename = "elevenlabs")]
    ElevenLabs,
    #[serde(rename = "sarvam")]
    Sarvam,
    #[serde(rename = "nextbase-codex")]
    NextbaseCodex,
}

impl Provider {
    pub const ALL: [Provider; 4] = [
        Provider::Groq,
        Provider::ElevenLabs,
        Provider::Sarvam,
        Provider::NextbaseCodex,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Groq => "groq",
            Provider::ElevenLabs => "elevenlabs",
            Provider::Sarvam => "sarvam",
            Provider::NextbaseCodex => "nextbase-codex",
        }
    }

    /// Prompt wording differs for the Nextbase gateway, which issues `nbmg_` keys
    /// rather than a vendor API key.
    pub fn key_prompt(&self) -> String {
        match self {
            Provider::NextbaseCodex => "Paste Nextbase gateway key (nbmg_...)".to_string(),
            other => format!("Paste {} API key", other.as_str()),
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Provider {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "groq" => Ok(Provider::Groq),
            "elevenlabs" => Ok(Provider::ElevenLabs),
            "sarvam" => Ok(Provider::Sarvam),
            "nextbase-codex" => Ok(Provider::NextbaseCodex),
            other => anyhow::bail!("Unknown provider: {other}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModelOption {
    pub label: &'static str,
    pub provider: Provider,
    pub model: &'static str,
}

pub const MODEL_OPTIONS: [ModelOption; 4] = [
    ModelOption {
        label: "Groq Whisper Large v3 Turbo",
        provider: Provider::Groq,
        model: "whisper-large-v3-turbo",
    },
    ModelOption {
        label: "ElevenLabs Scribe v2",
        provider: Provider::ElevenLabs,
        model: "scribe_v2",
    },
    ModelOption {
        label: "Sarvam Saaras v3",
        provider: Provider::Sarvam,
        model: "saaras:v3",
    },
    ModelOption {
        label: "Nextbase Codex Transcribe",
        provider: Provider::NextbaseCodex,
        model: "codex-transcribe",
    },
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shortcut: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub polish_shortcut: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spell_shortcut: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autostart: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_device: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_polish: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub polish_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_ducking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_ducking_volume: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_update: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_update_interval_minutes: Option<u64>,

    /// Anything this build does not know about is carried through untouched, so
    /// writing config from the Rust CLI never drops a setting the TypeScript CLI
    /// still owns.
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Config {
    pub fn key_for(&self, provider: Provider) -> Option<&str> {
        self.keys.get(provider.as_str()).map(String::as_str)
    }

    pub fn set_key(&mut self, provider: Provider, key: impl Into<String>) {
        self.keys.insert(provider.as_str().to_string(), key.into());
    }

    pub fn shortcut_or_default(&self) -> &str {
        self.shortcut.as_deref().unwrap_or(DEFAULT_SHORTCUT)
    }

    pub fn polish_shortcut_or_default(&self) -> &str {
        self.polish_shortcut
            .as_deref()
            .unwrap_or(DEFAULT_POLISH_SHORTCUT)
    }

    pub fn spell_shortcut_or_default(&self) -> &str {
        self.spell_shortcut
            .as_deref()
            .unwrap_or(DEFAULT_SPELL_SHORTCUT)
    }

    pub fn polish_model_or_default(&self) -> &str {
        self.polish_model.as_deref().unwrap_or(DEFAULT_POLISH_MODEL)
    }

    pub fn is_configured(&self) -> bool {
        match self.provider {
            Some(provider) => self.key_for(provider).is_some_and(|key| !key.is_empty()),
            None => false,
        }
    }
}

/// A missing or unreadable config is an empty config, matching the TypeScript
/// behaviour: setup should start clean rather than fail.
pub fn load() -> Config {
    let Ok(raw) = std::fs::read_to_string(paths::config_file()) else {
        return Config::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn save(config: &Config) -> Result<()> {
    let dir = paths::wisper_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Could not create {}", dir.display()))?;

    let body = serde_json::to_string_pretty(config)?;
    let target = paths::config_file();
    let temp = dir.join(format!("config.json.{}.tmp", std::process::id()));
    std::fs::write(&temp, body.as_bytes())?;
    std::fs::rename(&temp, &target)
        .with_context(|| format!("Could not write {}", target.display()))?;
    Ok(())
}

/// Read, mutate, write. Mirrors `updateConfig` in the TypeScript CLI.
pub fn update(edit: impl FnOnce(&mut Config)) -> Result<Config> {
    let mut config = load();
    edit(&mut config);
    save(&config)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_fields_survive_a_round_trip() {
        let raw = r#"{"provider":"groq","futureSetting":{"nested":true},"keys":{"groq":"k"}}"#;
        let config: Config = serde_json::from_str(raw).unwrap();
        let written = serde_json::to_string(&config).unwrap();
        assert!(
            written.contains("futureSetting"),
            "unknown settings must be preserved, got {written}"
        );
    }

    #[test]
    fn camel_case_matches_the_typescript_file() {
        let mut config = Config::default();
        config.polish_shortcut = Some("CommandOrControl+Shift+P".into());
        config.auto_update_interval_minutes = Some(180);
        let written = serde_json::to_string(&config).unwrap();
        assert!(written.contains("polishShortcut"));
        assert!(written.contains("autoUpdateIntervalMinutes"));
    }

    #[test]
    fn absent_settings_are_not_written_as_null() {
        let written = serde_json::to_string(&Config::default()).unwrap();
        assert_eq!(written, "{}");
    }
}
