//! The four files a finished meeting leaves behind.
//!
//! Files, not a chat summary — they go in `~/.nextbase/meetings/<id>/` so they can be
//! opened, kept and shared.
//!
//! Every claim about quality here is derived from something measured: job elapsed
//! time, timestamp coverage against the audio's own duration, segment and label
//! counts. Nothing invents an accuracy percentage, because nothing here can know one —
//! language detection says which language was heard, not whether the words are right.

use anyhow::{Context, Result};
use nextbase_core::sarvam_batch::{clock, Mode, Transcription};
use nextbase_core::wav::WavInfo;
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::state::{ActiveMeeting, SourceLevel};
use crate::summary::{Analysis, Confidence};

/// Everything needed to write the deliverables.
pub struct Deliverable<'a> {
    pub meeting: &'a ActiveMeeting,
    pub audio: WavInfo,
    pub transcription: &'a Transcription,
    pub analysis: Option<&'a Analysis>,
    pub mode: Mode,
    pub batch_elapsed_seconds: f64,
    pub job_id: String,
    pub partial: bool,
    pub failed_inputs: Vec<String>,
}

/// How much of the audio the transcript's timestamps actually span.
///
/// A transcript that stops 40 minutes into a 60-minute recording is the clearest sign
/// something went wrong, and it is invisible unless measured.
pub fn coverage_fraction(transcription: &Transcription, audio: &WavInfo) -> Option<f64> {
    let (_, last) = transcription.coverage()?;
    let duration = audio.duration_seconds();
    if duration <= 0.0 {
        return None;
    }
    Some((last / duration).clamp(0.0, 1.0))
}

/// How far the output can be trusted, based only on measured properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Themes only. Something measurable is off.
    HighLevelOnly,
    /// Timestamped discussion points, once spot-checked.
    Medium,
}

impl Trust {
    pub fn label(&self) -> &'static str {
        match self {
            Trust::HighLevelOnly => "high-level only",
            Trust::Medium => "medium",
        }
    }
}

/// Classify without inventing precision.
///
/// Deliberately only two levels, and never a number: the skill's point is that a
/// readable transcript can still corrupt names, amounts and commitments, and no
/// measurement available here distinguishes that.
pub fn trust(
    transcription: &Transcription,
    audio: &WavInfo,
    partial: bool,
) -> (Trust, Vec<String>) {
    let mut reasons = Vec::new();

    if partial {
        reasons.push("the transcription job only partly completed".to_string());
    }
    if transcription.segments.is_empty() {
        reasons.push("no diarized segments came back, so nothing is attributable".to_string());
    }
    match coverage_fraction(transcription, audio) {
        Some(fraction) if fraction < 0.9 => reasons.push(format!(
            "timestamps cover only {:.0}% of the recording",
            fraction * 100.0
        )),
        None if !transcription.segments.is_empty() => {
            reasons.push("segments carry no timestamps, so coverage is unknown".to_string())
        }
        _ => {}
    }

    let overlaps = transcription.overlapping_segments();
    if !transcription.segments.is_empty() && overlaps * 4 > transcription.segments.len() {
        reasons.push(format!(
            "{overlaps} of {} segments overlap, which lowers attribution confidence",
            transcription.segments.len()
        ));
    }

    let level = if reasons.is_empty() {
        Trust::Medium
    } else {
        Trust::HighLevelOnly
    };
    (level, reasons)
}

/// The caveat that goes on every note.
pub fn caveat(transcription: &Transcription, audio: &WavInfo, partial: bool) -> String {
    let (level, reasons) = trust(transcription, audio, partial);
    let mut text = format!(
        "**Quality: {}.** Speaker labels are generic and were not matched to people. \
         Timestamps are phrase-level, not word-level. Language detection reports which \
         language was heard, not whether the words are correct. Names, numbers, amounts, \
         deadlines and ownership need checking against the audio before being relied on.",
        level.label()
    );
    if !reasons.is_empty() {
        text.push_str("\n\nMeasured concerns: ");
        text.push_str(&reasons.join("; "));
        text.push('.');
    }
    text
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    meeting_id: String,
    started_at: String,
    audio_seconds: f64,
    audio_sample_rate: u32,
    audio_channels: u16,
    recorded_seconds: Option<f64>,
    source_levels: Vec<SourceLevel>,
    transcription_mode: String,
    batch_job_id: String,
    batch_elapsed_seconds: f64,
    partially_completed: bool,
    failed_inputs: Vec<String>,
    segment_count: usize,
    speaker_label_count: usize,
    overlapping_segments: usize,
    /// Provider language detection. Explicitly not an accuracy measure.
    detected_language: Option<String>,
    first_timestamp_seconds: Option<f64>,
    last_timestamp_seconds: Option<f64>,
    timestamp_coverage_fraction: Option<f64>,
    trust: String,
    trust_reasons: Vec<String>,
}

fn prepared_directory(deliverable: &Deliverable<'_>) -> Result<PathBuf> {
    let directory = deliverable.meeting.directory();
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("Could not create {}", directory.display()))?;
    Ok(directory)
}

/// Write only the files that depend on the transcription, not on the summary.
///
/// Split out so the pipeline can put the transcript on disk *before* it calls Groq. A
/// crash, a Ctrl+C or a machine going to sleep during summarising would otherwise cost
/// the user a transcription they have already paid Sarvam for, and the only way back is
/// to submit and pay for the same audio again. Neither file reads `analysis`, so `write`
/// re-rendering them afterwards produces byte-identical content.
pub fn write_transcript(deliverable: &Deliverable<'_>) -> Result<Vec<PathBuf>> {
    let directory = prepared_directory(deliverable)?;

    let diarized = directory.join("full-diarized-transcript.md");
    let plain = directory.join("full-transcript.txt");

    write_file(&diarized, &render_diarized(deliverable))?;
    write_file(&plain, &render_plain(deliverable))?;

    Ok(vec![diarized, plain])
}

/// Write all four files. Returns their paths in a stable order.
pub fn write(deliverable: &Deliverable<'_>) -> Result<Vec<PathBuf>> {
    let directory = prepared_directory(deliverable)?;

    let note = directory.join("meeting-note.md");
    let diarized = directory.join("full-diarized-transcript.md");
    let plain = directory.join("full-transcript.txt");
    let metadata = directory.join("processing-metadata.json");

    write_file(&diarized, &render_diarized(deliverable))?;
    write_file(&plain, &render_plain(deliverable))?;
    write_file(&metadata, &render_metadata(deliverable)?)?;
    // Last: `resumable` treats a directory without a note as unfinished, so the note
    // appearing is what marks the meeting done. Writing it first would hide a meeting
    // that still had files to write.
    write_file(&note, &render_note(deliverable))?;

    Ok(vec![note, diarized, plain, metadata])
}

fn write_file(path: &Path, body: &str) -> Result<()> {
    std::fs::write(path, body).with_context(|| format!("Could not write {}", path.display()))?;
    Ok(())
}

fn render_note(deliverable: &Deliverable<'_>) -> String {
    let analysis = deliverable.analysis;
    let title = analysis
        .map(|a| a.title.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "Meeting notes".to_string());

    let mut out = format!("# {title}\n\n");
    out.push_str(&format!(
        "- Meeting: `{}`\n- Started: {}\n- Audio: {} ({:.0} Hz mono)\n- Transcription: Sarvam Batch, mode `{}`\n\n",
        deliverable.meeting.id,
        deliverable.meeting.started_at,
        clock(deliverable.audio.duration_seconds()),
        deliverable.audio.sample_rate,
        deliverable.mode
    ));

    match analysis {
        Some(analysis) => {
            if !analysis.summary.is_empty() {
                out.push_str("## Summary\n\n");
                out.push_str(&analysis.summary);
                out.push_str("\n\n");
            }

            // Confirmed directions kept apart from anything merely raised.
            out.push_str("## Decisions\n\n");
            if analysis.decisions.is_empty() {
                out.push_str("_No decisions were stated outright._\n\n");
            } else {
                for decision in &analysis.decisions {
                    out.push_str(&format!("- {decision}\n"));
                }
                out.push('\n');
            }

            out.push_str("## Action items\n\n");
            if analysis.action_items.is_empty() {
                out.push_str("_No action items were identified._\n\n");
            } else {
                for item in &analysis.action_items {
                    let owner = match (&item.owner, item.confidence) {
                        (Some(owner), Confidence::Explicit) => format!(" — **{owner}**"),
                        // Never render a name for anything short of an explicit
                        // assignment; that is the difference between a note and a
                        // fabricated commitment.
                        _ => " — _unassigned_".to_string(),
                    };
                    let due = item
                        .due_date
                        .as_ref()
                        .map(|d| format!(" (due {d})"))
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "- `{}` {}{owner}{due}\n",
                        item.confidence.as_str(),
                        item.task
                    ));
                }
                out.push('\n');
            }

            if !analysis.blockers.is_empty() {
                out.push_str("## Risks and blockers\n\n");
                for blocker in &analysis.blockers {
                    out.push_str(&format!("- {blocker}\n"));
                }
                out.push('\n');
            }
            if !analysis.open_questions.is_empty() {
                out.push_str("## Open questions\n\n");
                for question in &analysis.open_questions {
                    out.push_str(&format!("- {question}\n"));
                }
                out.push('\n');
            }
        }
        None => {
            out.push_str("## Summary\n\n_No summary was produced. The transcript below is the full record._\n\n");
        }
    }

    out.push_str("## Discussion\n\n");
    let discussion = timestamped_points(deliverable.transcription, 25);
    if discussion.is_empty() {
        out.push_str("_No timestamped segments were returned._\n\n");
    } else {
        for line in discussion {
            out.push_str(&format!("- {line}\n"));
        }
        out.push('\n');
    }

    out.push_str("## Quality\n\n");
    out.push_str(&caveat(
        deliverable.transcription,
        &deliverable.audio,
        deliverable.partial,
    ));
    out.push_str("\n\nThe original audio remains the source of truth.\n");

    if deliverable.partial || !deliverable.failed_inputs.is_empty() {
        out.push_str(&format!(
            "\n> The transcription job did not fully complete. Unprocessed inputs: {}.\n",
            if deliverable.failed_inputs.is_empty() {
                "unknown".to_string()
            } else {
                deliverable.failed_inputs.join(", ")
            }
        ));
    }

    for level in &deliverable.meeting.source_levels {
        if level.silent {
            out.push_str(&format!(
                "\n> **{}** was silent for the whole recording, so anything from that side is missing from this transcript.\n",
                level.source
            ));
        }
    }

    out
}

/// Evenly spaced timestamped lines, so a long meeting yields a readable outline
/// rather than the whole transcript pasted twice.
fn timestamped_points(transcription: &Transcription, limit: usize) -> Vec<String> {
    let with_time: Vec<&nextbase_core::sarvam_batch::Segment> = transcription
        .segments
        .iter()
        .filter(|segment| segment.start_seconds.is_some())
        .collect();
    if with_time.is_empty() || limit == 0 {
        return Vec::new();
    }

    let step = (with_time.len() as f64 / limit as f64).ceil().max(1.0) as usize;
    with_time
        .iter()
        .step_by(step)
        .map(|segment| {
            format!(
                "**{}** {}: {}",
                clock(segment.start_seconds.unwrap_or(0.0)),
                segment.speaker.clone().unwrap_or_else(|| "UNKNOWN".into()),
                segment.text
            )
        })
        .collect()
}

fn render_diarized(deliverable: &Deliverable<'_>) -> String {
    let transcription = deliverable.transcription;
    let mut out = format!(
        "# Diarized transcript — {}\n\n\
         - Source duration: {}\n- Transcription mode: `{}`\n- Detected language: {}\n\
         - Segments: {} | Distinct speaker labels: {} | Overlapping: {}\n\n\
         > Speaker labels are generated by the provider and are **not** identities. \
         `SPEAKER_00` is one voice the model separated out, not a named person. \
         Timestamps are phrase-level, not word-level.\n\n---\n\n",
        deliverable.meeting.id,
        clock(deliverable.audio.duration_seconds()),
        deliverable.mode,
        transcription
            .language_code
            .clone()
            .unwrap_or_else(|| "not reported".into()),
        transcription.segments.len(),
        transcription.speaker_labels().len(),
        transcription.overlapping_segments(),
    );

    if transcription.segments.is_empty() {
        out.push_str("_No diarized segments were returned. The plain transcript has the text._\n");
        return out;
    }

    for segment in &transcription.segments {
        let stamp = segment
            .start_seconds
            .map(|s| format!("`{}` ", clock(s)))
            .unwrap_or_default();
        out.push_str(&format!(
            "{stamp}**{}**: {}\n\n",
            segment.speaker.clone().unwrap_or_else(|| "UNKNOWN".into()),
            segment.text
        ));
    }
    out
}

fn render_plain(deliverable: &Deliverable<'_>) -> String {
    let transcription = deliverable.transcription;
    if !transcription.text.trim().is_empty() {
        return format!("{}\n", transcription.text.trim());
    }
    // No plain field came back, so rebuild it from the segments rather than shipping
    // an empty file.
    let joined = transcription
        .segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    format!("{}\n", joined.trim())
}

fn render_metadata(deliverable: &Deliverable<'_>) -> Result<String> {
    let transcription = deliverable.transcription;
    let coverage = transcription.coverage();
    let (level, reasons) = trust(transcription, &deliverable.audio, deliverable.partial);

    let metadata = Metadata {
        meeting_id: deliverable.meeting.id.clone(),
        started_at: deliverable.meeting.started_at.clone(),
        audio_seconds: deliverable.audio.duration_seconds(),
        audio_sample_rate: deliverable.audio.sample_rate,
        audio_channels: deliverable.audio.channels,
        recorded_seconds: deliverable.meeting.duration_seconds,
        source_levels: deliverable.meeting.source_levels.clone(),
        transcription_mode: deliverable.mode.to_string(),
        batch_job_id: deliverable.job_id.clone(),
        batch_elapsed_seconds: deliverable.batch_elapsed_seconds,
        partially_completed: deliverable.partial,
        failed_inputs: deliverable.failed_inputs.clone(),
        segment_count: transcription.segments.len(),
        speaker_label_count: transcription.speaker_labels().len(),
        overlapping_segments: transcription.overlapping_segments(),
        detected_language: transcription.language_code.clone(),
        first_timestamp_seconds: coverage.map(|(first, _)| first),
        last_timestamp_seconds: coverage.map(|(_, last)| last),
        timestamp_coverage_fraction: coverage_fraction(transcription, &deliverable.audio),
        trust: level.label().to_string(),
        trust_reasons: reasons,
    };

    Ok(format!("{}\n", serde_json::to_string_pretty(&metadata)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nextbase_core::sarvam_batch::Segment;

    fn audio(seconds: f64) -> WavInfo {
        WavInfo {
            sample_rate: 16_000,
            channels: 1,
            bits_per_sample: 16,
            frames: (seconds * 16_000.0) as u64,
        }
    }

    fn transcription(segments: Vec<(f64, f64, &str)>) -> Transcription {
        Transcription {
            text: segments
                .iter()
                .map(|(_, _, t)| *t)
                .collect::<Vec<_>>()
                .join(" "),
            segments: segments
                .into_iter()
                .enumerate()
                .map(|(index, (start, end, text))| Segment {
                    speaker: Some(format!("SPEAKER_{:02}", index % 2)),
                    text: text.to_string(),
                    start_seconds: Some(start),
                    end_seconds: Some(end),
                })
                .collect(),
            language_code: Some("gu-IN".into()),
        }
    }

    #[test]
    fn full_coverage_with_clean_segments_reaches_medium_trust() {
        let details = transcription(vec![(0.0, 30.0, "one"), (31.0, 60.0, "two")]);
        let (level, reasons) = trust(&details, &audio(60.0), false);
        assert_eq!(level, Trust::Medium);
        assert!(reasons.is_empty(), "{reasons:?}");
    }

    #[test]
    fn a_transcript_that_stops_early_is_downgraded_and_says_why() {
        // The clearest sign of a broken run, and invisible unless measured.
        let details = transcription(vec![(0.0, 30.0, "one")]);
        let (level, reasons) = trust(&details, &audio(600.0), false);
        assert_eq!(level, Trust::HighLevelOnly);
        assert!(
            reasons.iter().any(|r| r.contains("cover only 5%")),
            "{reasons:?}"
        );
    }

    #[test]
    fn a_partial_job_is_always_downgraded() {
        let details = transcription(vec![(0.0, 60.0, "all of it")]);
        let (level, reasons) = trust(&details, &audio(60.0), true);
        assert_eq!(level, Trust::HighLevelOnly);
        assert!(reasons.iter().any(|r| r.contains("partly completed")));
    }

    #[test]
    fn no_segments_means_nothing_is_attributable() {
        let empty = Transcription {
            text: "words".into(),
            segments: Vec::new(),
            language_code: None,
        };
        let (level, reasons) = trust(&empty, &audio(60.0), false);
        assert_eq!(level, Trust::HighLevelOnly);
        assert!(reasons
            .iter()
            .any(|r| r.contains("nothing is attributable")));
    }

    #[test]
    fn the_caveat_never_claims_an_accuracy_figure() {
        let details = transcription(vec![(0.0, 60.0, "hello")]);
        let text = caveat(&details, &audio(60.0), false);

        assert!(text.contains("Speaker labels are generic"));
        assert!(text.contains("not word-level"));
        assert!(text.contains("not whether the words are correct"));
        // Nothing here can measure word accuracy, so no percentage may appear.
        assert!(!text.contains("% accurate"));
        assert!(!text.to_lowercase().contains("accuracy of"));
    }

    #[test]
    fn a_suggested_owner_is_never_printed_as_an_assignment() {
        let mut meeting = ActiveMeeting::new("meeting-x");
        meeting.started_at = "2026-07-25T10:00:00Z".into();
        let details = transcription(vec![(0.0, 60.0, "discussion")]);
        let analysis = Analysis {
            title: "Test".into(),
            summary: "A summary".into(),
            decisions: vec![],
            action_items: vec![
                crate::summary::ActionItem {
                    task: "Explicit thing".into(),
                    owner: Some("Dixit".into()),
                    due_date: None,
                    confidence: Confidence::Explicit,
                },
                crate::summary::ActionItem {
                    task: "Merely suggested".into(),
                    owner: Some("Priya".into()),
                    due_date: None,
                    confidence: Confidence::Suggested,
                },
            ],
            blockers: vec![],
            open_questions: vec![],
            language: "mixed".into(),
        };

        let note = render_note(&Deliverable {
            meeting: &meeting,
            audio: audio(60.0),
            transcription: &details,
            analysis: Some(&analysis),
            mode: Mode::Codemix,
            batch_elapsed_seconds: 10.0,
            job_id: "job-1".into(),
            partial: false,
            failed_inputs: vec![],
        });

        assert!(note.contains("**Dixit**"));
        // A name attached to a suggestion must not reach the note.
        assert!(!note.contains("Priya"), "{note}");
        assert!(note.contains("_unassigned_"));
    }

    #[test]
    fn a_silent_source_is_called_out_in_the_note() {
        let mut meeting = ActiveMeeting::new("meeting-y");
        meeting.source_levels = vec![SourceLevel {
            source: "system audio".into(),
            peak: 0.0,
            rms: 0.0,
            silent: true,
        }];
        let details = transcription(vec![(0.0, 60.0, "only me talking")]);

        let note = render_note(&Deliverable {
            meeting: &meeting,
            audio: audio(60.0),
            transcription: &details,
            analysis: None,
            mode: Mode::Transcribe,
            batch_elapsed_seconds: 5.0,
            job_id: "job-2".into(),
            partial: false,
            failed_inputs: vec![],
        });

        assert!(note.contains("**system audio** was silent"), "{note}");
        assert!(note.contains("missing from this transcript"));
    }

    #[test]
    fn the_diarized_file_states_labels_are_not_identities() {
        let meeting = ActiveMeeting::new("meeting-z");
        let details = transcription(vec![(0.0, 10.0, "hello"), (11.0, 20.0, "hi")]);
        let body = render_diarized(&Deliverable {
            meeting: &meeting,
            audio: audio(20.0),
            transcription: &details,
            analysis: None,
            mode: Mode::Transcribe,
            batch_elapsed_seconds: 1.0,
            job_id: "job-3".into(),
            partial: false,
            failed_inputs: vec![],
        });

        assert!(body.contains("**not** identities"));
        assert!(body.contains("not a named person"));
        assert!(body.contains("SPEAKER_00"));
        assert!(body.contains("`0:11`"));
    }

    #[test]
    fn the_plain_transcript_falls_back_to_segments_when_no_text_field_came_back() {
        let details = Transcription {
            text: String::new(),
            segments: transcription(vec![(0.0, 5.0, "first"), (6.0, 9.0, "second")]).segments,
            language_code: None,
        };
        let meeting = ActiveMeeting::new("meeting-w");
        let body = render_plain(&Deliverable {
            meeting: &meeting,
            audio: audio(10.0),
            transcription: &details,
            analysis: None,
            mode: Mode::Transcribe,
            batch_elapsed_seconds: 1.0,
            job_id: "j".into(),
            partial: false,
            failed_inputs: vec![],
        });
        assert_eq!(body, "first second\n");
    }

    #[test]
    fn metadata_records_measurements_and_not_an_accuracy_claim() {
        let meeting = ActiveMeeting::new("meeting-m");
        let details = transcription(vec![(0.0, 30.0, "a"), (31.0, 59.0, "b")]);
        let json = render_metadata(&Deliverable {
            meeting: &meeting,
            audio: audio(60.0),
            transcription: &details,
            analysis: None,
            mode: Mode::Codemix,
            batch_elapsed_seconds: 42.5,
            job_id: "job-9".into(),
            partial: false,
            failed_inputs: vec![],
        })
        .unwrap();

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["transcriptionMode"], "codemix");
        assert_eq!(value["batchElapsedSeconds"], 42.5);
        assert_eq!(value["segmentCount"], 2);
        assert_eq!(value["speakerLabelCount"], 2);
        assert_eq!(value["detectedLanguage"], "gu-IN");
        assert!(value["timestampCoverageFraction"].as_f64().unwrap() > 0.98);
        assert_eq!(value["trust"], "medium");
        // The field is named as detection, and there is no accuracy field at all.
        assert!(value.get("accuracy").is_none());
    }

    #[test]
    fn the_transcript_files_do_not_depend_on_the_summary() {
        // `write_transcript` runs before Groq is called, and `write` re-renders the same
        // two files afterwards with the analysis attached. If either file ever read
        // `analysis`, that second write would silently change a file the user may
        // already have open — and the early write would stop being the safety net it
        // exists to be.
        let meeting = ActiveMeeting::new("meeting-early");
        let details = transcription(vec![(0.0, 30.0, "one"), (31.0, 60.0, "two")]);
        let analysis = Analysis {
            title: "Summarised".into(),
            summary: "A summary that must not reach the transcript".into(),
            decisions: vec!["A decision".into()],
            action_items: vec![],
            blockers: vec![],
            open_questions: vec![],
            language: "mixed".into(),
        };
        fn describe<'a>(
            meeting: &'a ActiveMeeting,
            details: &'a Transcription,
            analysis: Option<&'a Analysis>,
        ) -> Deliverable<'a> {
            Deliverable {
                meeting,
                audio: audio(60.0),
                transcription: details,
                analysis,
                mode: Mode::Codemix,
                batch_elapsed_seconds: 12.0,
                job_id: "job-early".into(),
                partial: false,
                failed_inputs: vec![],
            }
        }

        let without = describe(&meeting, &details, None);
        let with = describe(&meeting, &details, Some(&analysis));

        assert_eq!(render_diarized(&without), render_diarized(&with));
        assert_eq!(render_plain(&without), render_plain(&with));
        // And the note is the file that does carry it, so the two writes are not
        // interchangeable.
        assert!(render_note(&with).contains("A decision"));
    }

    #[test]
    fn a_long_meeting_yields_a_readable_outline_not_the_whole_transcript() {
        let segments: Vec<(f64, f64, &str)> = (0..400)
            .map(|i| (i as f64 * 10.0, i as f64 * 10.0 + 9.0, "line"))
            .collect();
        let details = transcription(segments);
        let points = timestamped_points(&details, 25);
        assert!(points.len() <= 25, "{}", points.len());
        assert!(points.len() > 10);
    }
}
