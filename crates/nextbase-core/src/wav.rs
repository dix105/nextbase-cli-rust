//! Reading, slicing and splitting the 16-bit PCM WAV files the recorder writes.
//!
//! No ffmpeg. Every operation here is a sample-exact copy of our own output, so a
//! sample cut for the quality gate tests the original recording rather than a
//! re-encode — and a bad transcript can never be blamed on a conversion step.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// What the recorder produces and what Sarvam accepts natively.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WavInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub frames: u64,
}

impl WavInfo {
    pub fn duration_seconds(&self) -> f64 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        self.frames as f64 / self.sample_rate as f64
    }
}

/// Read the header and frame count without loading the audio.
///
/// This is the `ffprobe` step the skill asks for: we own the format, so the header
/// is the source of truth and there is no external binary to depend on.
pub fn info(path: &Path) -> Result<WavInfo> {
    let reader = hound::WavReader::open(path)
        .with_context(|| format!("Could not read audio file: {}", path.display()))?;
    let spec = reader.spec();
    let samples = reader.len() as u64;
    let channels = spec.channels.max(1);

    Ok(WavInfo {
        sample_rate: spec.sample_rate,
        channels,
        bits_per_sample: spec.bits_per_sample,
        frames: samples / channels as u64,
    })
}

fn read_frames(path: &Path) -> Result<(hound::WavSpec, Vec<i16>)> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("Could not read audio file: {}", path.display()))?;
    let spec = reader.spec();
    if spec.bits_per_sample != 16 || spec.sample_format != hound::SampleFormat::Int {
        bail!(
            "{} is not 16-bit PCM, so it cannot be sliced here.",
            path.display()
        );
    }
    let samples: Vec<i16> = reader.samples::<i16>().collect::<Result<_, _>>()?;
    Ok((spec, samples))
}

fn write_frames(path: &Path, spec: hound::WavSpec, samples: &[i16]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut writer = hound::WavWriter::create(path, spec)
        .with_context(|| format!("Could not write {}", path.display()))?;
    for sample in samples {
        writer.write_sample(*sample)?;
    }
    writer.finalize()?;
    Ok(())
}

/// Copy `[start, start + length)` of `source` into `destination`.
///
/// A request past the end is clamped rather than rejected: the caller picked a
/// window from measured audio, and losing a second at the tail is not worth failing
/// a meeting over.
pub fn slice(
    source: &Path,
    destination: &Path,
    start: std::time::Duration,
    length: std::time::Duration,
) -> Result<WavInfo> {
    let (spec, samples) = read_frames(source)?;
    let channels = spec.channels.max(1) as usize;
    let per_second = spec.sample_rate as usize * channels;
    if per_second == 0 {
        bail!("{} reports a zero sample rate.", source.display());
    }

    let start_sample = (start.as_secs_f64() * per_second as f64) as usize;
    let start_sample = start_sample.min(samples.len()) / channels * channels;
    let wanted = (length.as_secs_f64() * per_second as f64) as usize;
    let end_sample = start_sample.saturating_add(wanted).min(samples.len()) / channels * channels;

    if end_sample <= start_sample {
        bail!(
            "The requested slice of {} contains no audio.",
            source.display()
        );
    }

    write_frames(destination, spec, &samples[start_sample..end_sample])?;
    info(destination)
}

/// Split into parts of at most `max` each, named `<stem>-part01.wav` and so on.
///
/// Returns the original path unsplit when it already fits, so callers do not need a
/// special case for the common meeting.
pub fn split(source: &Path, directory: &Path, max: std::time::Duration) -> Result<Vec<PathBuf>> {
    let details = info(source)?;
    if details.duration_seconds() <= max.as_secs_f64() {
        return Ok(vec![source.to_path_buf()]);
    }

    let (spec, samples) = read_frames(source)?;
    let channels = spec.channels.max(1) as usize;
    let per_part = (max.as_secs_f64() * spec.sample_rate as f64) as usize * channels;
    if per_part == 0 {
        bail!("A zero-length split was requested.");
    }

    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "audio".to_string());

    let mut parts = Vec::new();
    for (index, chunk) in samples.chunks(per_part).enumerate() {
        let path = directory.join(format!("{stem}-part{:02}.wav", index + 1));
        write_frames(&path, spec, chunk)?;
        parts.push(path);
    }
    Ok(parts)
}

/// The window a sample should be cut from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    pub start: std::time::Duration,
    pub length: std::time::Duration,
    /// RMS of the chosen window, 0.0-1.0. Reported so a near-silent recording is
    /// visible before a sample is sent anywhere.
    pub rms: f32,
}

/// Bucket RMS energy and return the loudest contiguous window of `length`.
///
/// The skill warns against sampling the first two minutes, which is usually silence,
/// joining noises and "can you hear me". Picking by energy finds real conversation
/// wherever it happens to be, and `skip` drops a lead-in outright.
pub fn pick_energy_window(
    path: &Path,
    length: std::time::Duration,
    skip: std::time::Duration,
) -> Result<Window> {
    const BUCKET: std::time::Duration = std::time::Duration::from_secs(10);

    let (spec, samples) = read_frames(path)?;
    let channels = spec.channels.max(1) as usize;
    let per_second = spec.sample_rate as usize * channels;
    if per_second == 0 || samples.is_empty() {
        bail!("{} contains no audio to sample.", path.display());
    }

    let total = std::time::Duration::from_secs_f64(samples.len() as f64 / per_second as f64);
    // A recording shorter than the window (or barely longer than the lead-in) has no
    // choice to make: take it from the start and say what the energy was.
    if total <= length {
        return Ok(Window {
            start: std::time::Duration::ZERO,
            length: total,
            rms: rms_of(&samples),
        });
    }

    let bucket_samples = (BUCKET.as_secs_f64() * per_second as f64) as usize;
    let bucket_samples = bucket_samples.max(channels);
    let window_buckets = ((length.as_secs_f64() / BUCKET.as_secs_f64()).ceil() as usize).max(1);

    let energies: Vec<f64> = samples
        .chunks(bucket_samples)
        .map(|bucket| {
            bucket
                .iter()
                .map(|s| {
                    let v = *s as f64 / i16::MAX as f64;
                    v * v
                })
                .sum::<f64>()
                / bucket.len().max(1) as f64
        })
        .collect();

    let first_bucket = (skip.as_secs_f64() / BUCKET.as_secs_f64()) as usize;
    let last_start = energies.len().saturating_sub(window_buckets);
    // Honour `skip` only while it leaves a full window; a short meeting keeps its
    // lead-in rather than losing the sample entirely.
    let first_bucket = first_bucket.min(last_start);

    let mut best_start = first_bucket;
    let mut best_energy = f64::NEG_INFINITY;
    for start in first_bucket..=last_start {
        let energy: f64 = energies[start..(start + window_buckets).min(energies.len())]
            .iter()
            .sum();
        if energy > best_energy {
            best_energy = energy;
            best_start = start;
        }
    }

    let start = std::time::Duration::from_secs_f64(best_start as f64 * BUCKET.as_secs_f64());
    let start_sample = (start.as_secs_f64() * per_second as f64) as usize;
    let window_len = (length.as_secs_f64() * per_second as f64) as usize;
    let end_sample = (start_sample + window_len).min(samples.len());

    Ok(Window {
        start,
        length: length.min(total.saturating_sub(start)),
        rms: rms_of(&samples[start_sample..end_sample]),
    })
}

fn rms_of(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples
        .iter()
        .map(|s| {
            let v = *s as f64 / i16::MAX as f64;
            v * v
        })
        .sum();
    (sum / samples.len() as f64).sqrt() as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nextbase-wav-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn spec() -> hound::WavSpec {
        hound::WavSpec {
            channels: 1,
            sample_rate: TARGET_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        }
    }

    /// `sections` is (seconds, amplitude) so a test can place loud speech after a
    /// silent lead-in.
    fn write_test_wav(path: &Path, sections: &[(u32, i16)]) {
        let mut writer = hound::WavWriter::create(path, spec()).unwrap();
        for (seconds, amplitude) in sections {
            for index in 0..(*seconds * TARGET_SAMPLE_RATE) {
                // Alternate sign so RMS reflects the amplitude rather than a DC offset.
                let sample = if index % 2 == 0 {
                    *amplitude
                } else {
                    -*amplitude
                };
                writer.write_sample(sample).unwrap();
            }
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn info_reports_duration_from_the_header() {
        let dir = scratch("info");
        let path = dir.join("a.wav");
        write_test_wav(&path, &[(3, 1000)]);

        let details = info(&path).unwrap();
        assert_eq!(details.sample_rate, TARGET_SAMPLE_RATE);
        assert_eq!(details.channels, 1);
        assert_eq!(details.frames, 3 * TARGET_SAMPLE_RATE as u64);
        assert!((details.duration_seconds() - 3.0).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_slice_has_the_requested_length() {
        let dir = scratch("slice");
        let source = dir.join("full.wav");
        let cut = dir.join("cut.wav");
        write_test_wav(&source, &[(30, 800)]);

        let details = slice(
            &source,
            &cut,
            Duration::from_secs(10),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(details.frames, 5 * TARGET_SAMPLE_RATE as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_slice_past_the_end_is_clamped_not_rejected() {
        let dir = scratch("clamp");
        let source = dir.join("full.wav");
        let cut = dir.join("cut.wav");
        write_test_wav(&source, &[(10, 800)]);

        let details = slice(
            &source,
            &cut,
            Duration::from_secs(8),
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(details.frames, 2 * TARGET_SAMPLE_RATE as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audio_within_the_limit_is_not_split() {
        let dir = scratch("nosplit");
        let source = dir.join("full.wav");
        write_test_wav(&source, &[(4, 500)]);

        let parts = split(&source, &dir, Duration::from_secs(10)).unwrap();
        assert_eq!(parts, vec![source]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_recording_splits_into_parts_that_add_up() {
        let dir = scratch("split");
        let source = dir.join("full.wav");
        write_test_wav(&source, &[(25, 500)]);

        let parts = split(&source, &dir, Duration::from_secs(10)).unwrap();
        assert_eq!(parts.len(), 3);

        let total: u64 = parts.iter().map(|p| info(p).unwrap().frames).sum();
        assert_eq!(total, 25 * TARGET_SAMPLE_RATE as u64);
        // Every part must be within the limit, or Sarvam rejects the job.
        for part in &parts {
            assert!(info(part).unwrap().duration_seconds() <= 10.0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sample_window_skips_a_silent_lead_in() {
        let dir = scratch("energy");
        let source = dir.join("full.wav");
        // 60s of near-silence, then 40s of speech: the naive first-two-minutes
        // sample would be entirely silence.
        write_test_wav(&source, &[(60, 2), (40, 6000)]);

        let window =
            pick_energy_window(&source, Duration::from_secs(20), Duration::from_secs(30)).unwrap();
        assert!(
            window.start.as_secs() >= 60,
            "picked {}s, expected the loud section at 60s+",
            window.start.as_secs()
        );
        assert!(window.rms > 0.1, "rms was {}", window.rms);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_recording_shorter_than_the_window_uses_all_of_it() {
        let dir = scratch("short");
        let source = dir.join("full.wav");
        write_test_wav(&source, &[(8, 4000)]);

        let window =
            pick_energy_window(&source, Duration::from_secs(180), Duration::from_secs(30)).unwrap();
        assert_eq!(window.start, Duration::ZERO);
        assert!((window.length.as_secs_f64() - 8.0).abs() < 0.01);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_lead_in_is_kept_when_skipping_would_leave_no_window() {
        let dir = scratch("skipwide");
        let source = dir.join("full.wav");
        write_test_wav(&source, &[(40, 3000)]);

        // Skip 30s of a 40s recording while asking for a 20s window: obeying the
        // skip exactly would leave only 10s.
        let window =
            pick_energy_window(&source, Duration::from_secs(20), Duration::from_secs(30)).unwrap();
        assert!(window.start.as_secs() <= 20);
        assert!((window.length.as_secs_f64() - 20.0).abs() < 0.01);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
