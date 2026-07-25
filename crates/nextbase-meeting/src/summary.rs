//! Meeting analysis through Groq chat completions.
//!
//! Ported from the TypeScript `notebot-ai.ts`, keeping the two things that build got
//! right: the system prompt forbids inventing owners, and every field is re-validated
//! after parsing rather than trusted. A model that returns a plausible owner for an
//! unassigned task is the failure that matters here — it turns a suggestion into an
//! apparent commitment.

use anyhow::{bail, Context, Result};
use nextbase_core::config::{Config, Provider};
use serde::{Deserialize, Serialize};

/// How firmly a task was actually assigned in the meeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// Stated outright, with an owner.
    Explicit,
    /// Implied, or an owner inferred from context but never assigned.
    Suggested,
    /// A task with nobody attached.
    Unassigned,
}

impl Confidence {
    fn parse(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_lowercase()).as_deref() {
            Some("explicit") => Self::Explicit,
            Some("suggested") => Self::Suggested,
            // Anything unrecognised falls to the weakest claim rather than the
            // strongest.
            _ => Self::Unassigned,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Suggested => "suggested",
            Self::Unassigned => "unassigned",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionItem {
    pub task: String,
    /// Only ever set when the transcript assigned it outright.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_date: Option<String>,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Analysis {
    pub title: String,
    pub summary: String,
    pub decisions: Vec<String>,
    pub action_items: Vec<ActionItem>,
    pub blockers: Vec<String>,
    pub open_questions: Vec<String>,
    /// What the model thought was spoken. Not a provider detection.
    pub language: String,
}

const SYSTEM_PROMPT: &str = "You extract structured meeting notes from multilingual Gujarati, Hindi, English, Hinglish and code-mixed transcripts. Return valid JSON only. Never invent facts, decisions, people, deadlines or responsibilities. Only set actionItems[].owner when the transcript explicitly assigns that task to that person. If context merely suggests someone, omit owner and set confidence to \"suggested\". If nobody is attached, omit owner and set confidence to \"unassigned\". Speaker labels like SPEAKER_00 are not names: never present them as people, and never guess who they are. Write title and summary in English.";

const SHAPE: &str = r#"{"title":"string","summary":"string","decisions":["string"],"actionItems":[{"task":"string","owner":"optional string","dueDate":"optional string","confidence":"explicit|suggested|unassigned"}],"blockers":["string"],"openQuestions":["string"],"language":"string"}"#;

pub async fn analyse(transcript: &str, config: &Config) -> Result<Analysis> {
    if transcript.trim().is_empty() {
        bail!("There is no transcript to summarise.");
    }

    let key = config
        .key_for(Provider::Groq)
        .filter(|key| !key.is_empty())
        .context("Meeting summaries need a Groq API key. Run: nbmeet setup")?;

    let body = serde_json::json!({
        "model": config.polish_model_or_default(),
        // Low but not zero: summarising needs to paraphrase, not invent.
        "temperature": 0.1,
        "response_format": {"type": "json_object"},
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": format!("Transcript:\n{transcript}\n\nReturn exactly this JSON shape:\n{SHAPE}")}
        ]
    });

    let response = reqwest::Client::builder()
        // A long meeting transcript is a large prompt.
        .timeout(std::time::Duration::from_secs(300))
        .build()?
        .post("https://api.groq.com/openai/v1/chat/completions")
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .context("Could not reach Groq to summarise the meeting")?;

    let status = response.status();
    let payload: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        let message = payload
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .map(|m| m.to_string())
            .unwrap_or_else(|| format!("Meeting analysis failed: HTTP {}", status.as_u16()));
        bail!("{message}");
    }

    let content = payload
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    parse_analysis(content)
}

/// Turn the model's reply into an `Analysis`, re-validating every field.
///
/// `json_object` mode still occasionally wraps the reply in a code fence, and nothing
/// stops a model returning a number where a string belongs.
pub fn parse_analysis(content: &str) -> Result<Analysis> {
    let cleaned = strip_code_fence(content);
    let parsed: serde_json::Value =
        serde_json::from_str(&cleaned).context("Meeting analysis returned invalid JSON.")?;

    Ok(Analysis {
        title: text(&parsed, "title").unwrap_or_else(|| "Untitled meeting".to_string()),
        summary: text(&parsed, "summary").unwrap_or_default(),
        decisions: strings(&parsed, "decisions"),
        action_items: action_items(&parsed),
        blockers: strings(&parsed, "blockers"),
        open_questions: strings(&parsed, "openQuestions"),
        language: text(&parsed, "language").unwrap_or_else(|| "mixed".to_string()),
    })
}

fn strip_code_fence(content: &str) -> String {
    let trimmed = content.trim();
    let without_open = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```JSON"))
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    without_open
        .strip_suffix("```")
        .unwrap_or(without_open)
        .trim()
        .to_string()
}

fn text(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn strings(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn action_items(value: &serde_json::Value) -> Vec<ActionItem> {
    value
        .get("actionItems")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let task = text(item, "task")?;
                    let owner = text(item, "owner");
                    let confidence =
                        Confidence::parse(item.get("confidence").and_then(|v| v.as_str()));

                    // The rule the prompt states, enforced in code as well: an owner
                    // only stands when the task was explicitly assigned. A model that
                    // pairs a name with "suggested" would otherwise turn an inference
                    // into an apparent commitment.
                    let (owner, confidence) = match (owner, confidence) {
                        (Some(owner), Confidence::Explicit) => (Some(owner), Confidence::Explicit),
                        (Some(_), _) => (None, Confidence::Suggested),
                        (None, Confidence::Explicit) => (None, Confidence::Suggested),
                        (None, other) => (None, other),
                    };

                    Some(ActionItem {
                        task,
                        owner,
                        due_date: text(item, "dueDate"),
                        confidence,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_reply_parses_completely() {
        let analysis = parse_analysis(
            r#"{"title":"Sprint planning","summary":"Discussed the release.",
                "decisions":["Ship on Friday"],
                "actionItems":[{"task":"Write the migration","owner":"Dixit","confidence":"explicit","dueDate":"2026-08-01"}],
                "blockers":["Staging is down"],"openQuestions":["Who signs off?"],"language":"Gujarati/English"}"#,
        )
        .unwrap();

        assert_eq!(analysis.title, "Sprint planning");
        assert_eq!(analysis.decisions, vec!["Ship on Friday"]);
        assert_eq!(analysis.action_items.len(), 1);
        assert_eq!(analysis.action_items[0].owner.as_deref(), Some("Dixit"));
        assert_eq!(analysis.action_items[0].confidence, Confidence::Explicit);
        assert_eq!(
            analysis.action_items[0].due_date.as_deref(),
            Some("2026-08-01")
        );
        assert_eq!(analysis.blockers, vec!["Staging is down"]);
    }

    #[test]
    fn a_code_fenced_reply_still_parses() {
        // json_object mode mostly prevents this, but not always.
        let analysis = parse_analysis("```json\n{\"title\":\"Fenced\"}\n```").unwrap();
        assert_eq!(analysis.title, "Fenced");
    }

    #[test]
    fn an_owner_on_a_suggested_task_is_dropped() {
        // The important guarantee: an inferred name must never read as an assignment.
        let analysis = parse_analysis(
            r#"{"actionItems":[{"task":"Review the PR","owner":"Priya","confidence":"suggested"}]}"#,
        )
        .unwrap();

        assert_eq!(analysis.action_items[0].owner, None);
        assert_eq!(analysis.action_items[0].confidence, Confidence::Suggested);
    }

    #[test]
    fn explicit_without_an_owner_is_downgraded_not_trusted() {
        let analysis =
            parse_analysis(r#"{"actionItems":[{"task":"Fix the build","confidence":"explicit"}]}"#)
                .unwrap();
        assert_eq!(analysis.action_items[0].confidence, Confidence::Suggested);
        assert_eq!(analysis.action_items[0].owner, None);
    }

    #[test]
    fn an_unrecognised_confidence_falls_to_the_weakest_claim() {
        let analysis = parse_analysis(
            r#"{"actionItems":[{"task":"Something","confidence":"very sure indeed"}]}"#,
        )
        .unwrap();
        assert_eq!(analysis.action_items[0].confidence, Confidence::Unassigned);
    }

    #[test]
    fn missing_fields_get_honest_defaults_rather_than_failing() {
        let analysis = parse_analysis("{}").unwrap();
        assert_eq!(analysis.title, "Untitled meeting");
        assert_eq!(analysis.language, "mixed");
        assert!(analysis.summary.is_empty());
        assert!(analysis.decisions.is_empty());
        assert!(analysis.action_items.is_empty());
    }

    #[test]
    fn wrongly_typed_fields_are_skipped_not_stringified() {
        let analysis = parse_analysis(
            r#"{"title":42,"decisions":["real",7,""],"actionItems":[{"task":""},{"task":"kept"}]}"#,
        )
        .unwrap();

        assert_eq!(analysis.title, "Untitled meeting");
        assert_eq!(analysis.decisions, vec!["real"]);
        // The empty task is dropped, not carried as a blank action item.
        assert_eq!(analysis.action_items.len(), 1);
        assert_eq!(analysis.action_items[0].task, "kept");
    }

    #[test]
    fn invalid_json_is_an_error_with_a_readable_message() {
        let error = parse_analysis("not json at all").unwrap_err();
        assert!(error.to_string().contains("invalid JSON"), "{error}");
    }

    #[test]
    fn the_prompt_forbids_inventing_owners_and_naming_speakers() {
        // These two clauses are the whole reason the summary is trustworthy; a
        // reword that drops them should break a test, not slip through.
        assert!(SYSTEM_PROMPT.contains("Never invent"));
        assert!(SYSTEM_PROMPT.contains("explicitly assigns"));
        assert!(SYSTEM_PROMPT.contains("SPEAKER_00 are not names"));
    }
}
