//! Meeting capture: the microphone and the system output, mixed into one 16 kHz
//! mono WAV.
//!
//! Wisper records dictation — one voice, one device, a few seconds — so `audio.rs`
//! opens the microphone and writes it straight out. A meeting is different: the
//! other participants are not in the room, they are coming out of the speakers, and
//! recording only the microphone produces a transcript of half a conversation.
//!
//! Three decisions shape this module:
//!
//! 1. **16 kHz mono output.** At the 48 kHz `audio.rs` records natively, two hours is
//!    ~690 MB to upload; at 16 kHz it is ~115 MB, and Sarvam accepts 16 kHz PCM
//!    natively. Sources are asked for 16 kHz directly where the platform allows it,
//!    so resampling is skipped rather than merely cheap.
//! 2. **One source is the clock.** Two devices have two independent clocks, which
//!    drift measurably over a two-hour meeting. The microphone (or whichever source
//!    is enabled) drives the write loop; the other is padded with silence when it
//!    falls short and trimmed when it runs ahead.
//! 3. **The header is checkpointed.** `WavWriter::flush` rewrites the RIFF sizes, so
//!    a recorder that is killed two hours in still leaves a playable file. Without it
//!    the header claims zero samples and the whole meeting is unreadable.

use anyhow::{bail, Result};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::audio::{Levels, LiveLevel};
use crate::wav::TARGET_SAMPLE_RATE;

mod mic;
mod resample;
mod system;

pub use resample::Resampler;
pub use system::{system_source_name, SystemAudioStatus};

/// How often the WAV header is rewritten so a killed recorder leaves a usable file.
const CHECKPOINT: Duration = Duration::from_secs(5);
/// Drift guard: a non-clock source is never allowed to bank more than this.
const MAX_BACKLOG: Duration = Duration::from_secs(2);

/// Which inputs a meeting should record.
#[derive(Debug, Clone)]
pub struct CaptureOptions {
    pub mic: bool,
    pub system: bool,
    /// Microphone name, or `None` for the system default.
    pub device: Option<String>,
    /// Also keep an unmixed WAV per source, for working out which side went wrong.
    pub keep_tracks: bool,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            mic: true,
            system: true,
            device: None,
            keep_tracks: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    Mic,
    System,
}

impl SourceKind {
    pub fn label(&self) -> &'static str {
        match self {
            SourceKind::Mic => "microphone",
            SourceKind::System => "system audio",
        }
    }
}

/// A running source, handing mono 16 kHz samples to the mixer.
pub(crate) struct SourceHandle {
    pub(crate) kind: SourceKind,
    pub(crate) buffer: Arc<Mutex<VecDeque<f32>>>,
    pub(crate) live: Arc<LiveLevel>,
    /// Kept so the platform stream is torn down when the recording ends.
    pub(crate) stop: Box<dyn FnOnce() + Send>,
}

/// A shared sample sink a platform callback can push into without blocking long.
#[derive(Clone)]
pub(crate) struct Sink {
    buffer: Arc<Mutex<VecDeque<f32>>>,
    live: Arc<LiveLevel>,
    resampler: Arc<Mutex<Option<Resampler>>>,
}

impl Sink {
    pub(crate) fn new(source_rate: u32) -> Result<Self> {
        Ok(Self {
            buffer: Arc::new(Mutex::new(VecDeque::new())),
            live: Arc::new(LiveLevel::default()),
            resampler: Arc::new(Mutex::new(Resampler::new(source_rate, TARGET_SAMPLE_RATE)?)),
        })
    }

    /// Push mono samples at the source's native rate.
    pub(crate) fn push(&self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }

        let mut peak = 0.0f32;
        let mut sum_squares = 0.0f64;
        for sample in samples {
            peak = peak.max(sample.abs());
            sum_squares += (*sample as f64) * (*sample as f64);
        }
        self.live.publish(Levels {
            peak,
            rms: (sum_squares / samples.len() as f64).sqrt() as f32,
        });

        let resampled = match self.resampler.lock() {
            Ok(mut guard) => match guard.as_mut() {
                Some(resampler) => resampler.push(samples),
                None => samples.to_vec(),
            },
            Err(_) => return,
        };

        if let Ok(mut buffer) = self.buffer.lock() {
            buffer.extend(resampled);
        }
    }

    pub(crate) fn buffer_handle(&self) -> Arc<Mutex<VecDeque<f32>>> {
        Arc::clone(&self.buffer)
    }

    pub(crate) fn live_handle(&self) -> Arc<LiveLevel> {
        Arc::clone(&self.live)
    }
}

/// What the mixer thread hands back: overall levels plus one entry per source.
type MixOutcome = (Levels, Vec<(SourceKind, Levels)>);

pub struct Finished {
    pub path: PathBuf,
    pub duration: Duration,
    pub levels: Levels,
    /// Per-source levels over the whole recording, so a source that stayed silent
    /// the entire meeting is visible afterwards rather than merely suspected.
    pub per_source: Vec<(SourceKind, Levels)>,
}

/// A meeting recording in progress.
pub struct MixedRecording {
    path: PathBuf,
    started: Instant,
    stop: Arc<AtomicBool>,
    live: Vec<(SourceKind, Arc<LiveLevel>)>,
    worker: Option<std::thread::JoinHandle<Result<MixOutcome>>>,
}

impl MixedRecording {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Live level per source, for a meter that shows both sides separately — the
    /// point being that a dead system source is obvious while recording.
    pub fn live_levels(&self) -> Vec<(SourceKind, Levels)> {
        self.live
            .iter()
            .map(|(kind, live)| (*kind, live.get()))
            .collect()
    }

    pub fn stop(mut self) -> Result<Finished> {
        let duration = self.started.elapsed();
        self.stop.store(true, Ordering::SeqCst);
        let (levels, per_source) = match self.worker.take() {
            Some(worker) => worker
                .join()
                .map_err(|_| anyhow::anyhow!("Recording thread panicked"))??,
            None => (Levels::default(), Vec::new()),
        };
        Ok(Finished {
            path: self.path.clone(),
            duration,
            levels,
            per_source,
        })
    }
}

impl Drop for MixedRecording {
    fn drop(&mut self) {
        // Never leave a device or a capture stream open.
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Open the requested sources and start writing the mix to `path`.
pub fn start(options: &CaptureOptions, path: PathBuf) -> Result<MixedRecording> {
    if !options.mic && !options.system {
        bail!("Both the microphone and system audio are disabled, so there is nothing to record.");
    }

    let mut sources: Vec<SourceHandle> = Vec::new();
    let mut problems: Vec<String> = Vec::new();

    if options.mic {
        match mic::start(options.device.as_deref()) {
            Ok(handle) => sources.push(handle),
            Err(error) => problems.push(format!("microphone: {error}")),
        }
    }
    if options.system {
        match system::start() {
            Ok(handle) => sources.push(handle),
            Err(error) => problems.push(format!("system audio: {error}")),
        }
    }

    // One working source still records a useful meeting, so a single failure is a
    // warning rather than a dead stop — but it must be said out loud, because a
    // half-recorded meeting is only discovered when the transcript is missing
    // everyone else.
    if sources.is_empty() {
        bail!("No audio source could be opened. {}", problems.join("; "));
    }
    for problem in &problems {
        crate::log::log(&format!("Meeting capture warning — {problem}"));
    }

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let track_paths: Vec<(SourceKind, PathBuf)> = if options.keep_tracks {
        sources
            .iter()
            .map(|source| {
                let name = match source.kind {
                    SourceKind::Mic => "track-mic.wav",
                    SourceKind::System => "track-system.wav",
                };
                (source.kind, path.with_file_name(name))
            })
            .collect()
    } else {
        Vec::new()
    };

    let stop = Arc::new(AtomicBool::new(false));
    let live: Vec<(SourceKind, Arc<LiveLevel>)> = sources
        .iter()
        .map(|source| (source.kind, Arc::clone(&source.live)))
        .collect();

    let thread_stop = Arc::clone(&stop);
    let thread_path = path.clone();
    let worker =
        std::thread::spawn(move || write_mix(sources, thread_path, spec, track_paths, thread_stop));

    Ok(MixedRecording {
        path,
        started: Instant::now(),
        stop,
        live,
        worker: Some(worker),
    })
}

/// Pull from every source and write the sum until stopped.
fn write_mix(
    sources: Vec<SourceHandle>,
    path: PathBuf,
    spec: hound::WavSpec,
    track_paths: Vec<(SourceKind, PathBuf)>,
    stop: Arc<AtomicBool>,
) -> Result<MixOutcome> {
    let mut writer = hound::WavWriter::create(&path, spec)?;
    let mut tracks: Vec<(
        SourceKind,
        hound::WavWriter<std::io::BufWriter<std::fs::File>>,
    )> = track_paths
        .into_iter()
        .filter_map(|(kind, track_path)| {
            hound::WavWriter::create(&track_path, spec)
                .ok()
                .map(|writer| (kind, writer))
        })
        .collect();

    // The first source is the clock: everything is written at its pace.
    let (clock, others) = sources.split_first().expect("at least one source");
    let mut totals: Vec<(SourceKind, Accumulator)> = sources
        .iter()
        .map(|source| (source.kind, Accumulator::default()))
        .collect();
    let mut mixed = Accumulator::default();
    let mut last_checkpoint = Instant::now();
    let max_backlog = (MAX_BACKLOG.as_secs_f64() * TARGET_SAMPLE_RATE as f64) as usize;

    let mut scratch: Vec<f32> = Vec::new();
    let mut other_scratch: Vec<f32> = Vec::new();

    loop {
        let stopping = stop.load(Ordering::SeqCst);

        take_available(&clock.buffer, usize::MAX, &mut scratch);
        if scratch.is_empty() {
            if stopping {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }

        record_track(&mut tracks, clock.kind, &scratch);
        accumulate(&mut totals, clock.kind, &scratch);

        for source in others {
            // Exactly as many frames as the clock produced: that is what keeps the
            // two streams from sliding apart over hours.
            take_available(&source.buffer, scratch.len(), &mut other_scratch);
            trim_backlog(&source.buffer, max_backlog);
            record_track(&mut tracks, source.kind, &other_scratch);
            accumulate(&mut totals, source.kind, &other_scratch);

            for (index, sample) in other_scratch.iter().enumerate() {
                scratch[index] += *sample;
            }
        }

        for sample in scratch.iter_mut() {
            // Two sources summing to more than full scale would clip hard; a soft
            // knee keeps loud passages intelligible instead of crunching them.
            *sample = soft_clip(*sample);
            mixed.push(*sample);
            writer.write_sample((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }

        if last_checkpoint.elapsed() >= CHECKPOINT {
            // Cheap insurance: a crash now still leaves everything up to here.
            let _ = writer.flush();
            for (_, track) in tracks.iter_mut() {
                let _ = track.flush();
            }
            last_checkpoint = Instant::now();
        }
    }

    writer.finalize()?;
    for (_, track) in tracks {
        let _ = track.finalize();
    }
    for source in sources {
        (source.stop)();
    }

    Ok((
        mixed.levels(),
        totals
            .into_iter()
            .map(|(kind, accumulator)| (kind, accumulator.levels()))
            .collect(),
    ))
}

fn soft_clip(sample: f32) -> f32 {
    if sample.abs() <= 0.7 {
        sample
    } else {
        sample.signum() * (0.7 + (sample.abs() - 0.7).tanh() * 0.3)
    }
}

fn take_available(buffer: &Arc<Mutex<VecDeque<f32>>>, wanted: usize, out: &mut Vec<f32>) {
    out.clear();
    let Ok(mut guard) = buffer.lock() else { return };
    let take = guard.len().min(wanted);
    out.extend(guard.drain(..take));
    // A source that fell short is padded with silence rather than shortening the
    // mix, which would shift every later timestamp.
    if wanted != usize::MAX && out.len() < wanted {
        out.resize(wanted, 0.0);
    }
}

/// Drop the oldest samples when a source banks more than the drift guard allows.
fn trim_backlog(buffer: &Arc<Mutex<VecDeque<f32>>>, max: usize) {
    let Ok(mut guard) = buffer.lock() else { return };
    if guard.len() > max {
        let excess = guard.len() - max;
        drop(guard.drain(..excess));
    }
}

fn record_track(
    tracks: &mut [(
        SourceKind,
        hound::WavWriter<std::io::BufWriter<std::fs::File>>,
    )],
    kind: SourceKind,
    samples: &[f32],
) {
    for (track_kind, writer) in tracks.iter_mut() {
        if *track_kind != kind {
            continue;
        }
        for sample in samples {
            let _ = writer.write_sample((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
        }
    }
}

fn accumulate(totals: &mut [(SourceKind, Accumulator)], kind: SourceKind, samples: &[f32]) {
    for (total_kind, accumulator) in totals.iter_mut() {
        if *total_kind == kind {
            for sample in samples {
                accumulator.push(*sample);
            }
        }
    }
}

#[derive(Debug, Default)]
struct Accumulator {
    peak: f32,
    sum_squares: f64,
    samples: u64,
}

impl Accumulator {
    fn push(&mut self, sample: f32) {
        self.peak = self.peak.max(sample.abs());
        self.sum_squares += (sample as f64) * (sample as f64);
        self.samples += 1;
    }

    fn levels(&self) -> Levels {
        Levels {
            peak: self.peak,
            rms: if self.samples == 0 {
                0.0
            } else {
                (self.sum_squares / self.samples as f64).sqrt() as f32
            },
        }
    }
}

/// What a short test of one source found.
#[derive(Debug, Clone)]
pub struct SourceProbe {
    pub kind: SourceKind,
    /// Device or capture backend the samples would come from.
    pub source: Option<String>,
    /// The source opened. Not the same as "heard something".
    pub opened: bool,
    pub levels: Levels,
    pub error: Option<String>,
}

impl SourceProbe {
    /// Signal above the silence floor. Absence is not failure — nobody may have been
    /// speaking, and nothing may have been playing.
    pub fn heard_something(&self) -> bool {
        self.opened && !self.levels.is_silent()
    }
}

/// Open one source for `duration` and report what it heard.
///
/// A meeting recorded with a silently broken system source is the worst outcome this
/// tool has: the recording looks fine, and the far side is simply absent from the
/// transcript. That has to be visible *before* the meeting, which is what this is for.
pub fn probe(kind: SourceKind, device: Option<&str>, duration: Duration) -> SourceProbe {
    let source = match kind {
        SourceKind::Mic => device
            .map(|d| d.to_string())
            .or_else(|| crate::audio::list_input_devices().first().cloned()),
        SourceKind::System => system::system_source_name(),
    };

    let opened = match kind {
        SourceKind::Mic => mic::start(device),
        SourceKind::System => system::start(),
    };

    let handle = match opened {
        Ok(handle) => handle,
        Err(error) => {
            return SourceProbe {
                kind,
                source,
                opened: false,
                levels: Levels::default(),
                error: Some(error.to_string()),
            }
        }
    };

    std::thread::sleep(duration);
    let levels = handle.live.get();
    let mut accumulated = Accumulator::default();
    if let Ok(mut buffer) = handle.buffer.lock() {
        for sample in buffer.drain(..) {
            accumulated.push(sample);
        }
    }
    (handle.stop)();

    // Whole-probe levels are the honest number; the last callback's levels only cover
    // a few milliseconds and can miss speech that happened a moment earlier.
    let levels = if accumulated.samples > 0 {
        accumulated.levels()
    } else {
        levels
    };

    SourceProbe {
        kind,
        source,
        opened: true,
        levels,
        error: None,
    }
}

/// Whether system audio can be captured here, and what is in the way if not.
pub fn system_audio_status() -> SystemAudioStatus {
    system::status()
}

/// Ask the OS for whatever permission system capture needs.
pub fn request_system_permission() -> Result<bool> {
    system::request_permission()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_clipping_leaves_normal_levels_untouched() {
        for sample in [0.0, 0.25, -0.5, 0.7, -0.7] {
            assert!((soft_clip(sample) - sample).abs() < 1e-6, "{sample}");
        }
    }

    #[test]
    fn soft_clipping_keeps_a_sum_of_two_loud_sources_in_range() {
        // Two sources at 0.9 sum to 1.8, which would clip flat and crunch.
        let clipped = soft_clip(1.8);
        assert!(clipped > 0.7 && clipped <= 1.0, "{clipped}");
        assert_eq!(soft_clip(-1.8), -clipped);
        // Monotonic, so louder input still means louder output.
        assert!(soft_clip(1.2) < soft_clip(1.8));
    }

    #[test]
    fn a_short_source_is_padded_so_the_mix_never_shortens() {
        let buffer = Arc::new(Mutex::new(VecDeque::from(vec![0.5, 0.5])));
        let mut out = Vec::new();
        take_available(&buffer, 5, &mut out);
        assert_eq!(out, vec![0.5, 0.5, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn the_clock_source_takes_everything_available_without_padding() {
        let buffer = Arc::new(Mutex::new(VecDeque::from(vec![0.1, 0.2, 0.3])));
        let mut out = Vec::new();
        take_available(&buffer, usize::MAX, &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3]);
        assert!(buffer.lock().unwrap().is_empty());
    }

    #[test]
    fn a_source_that_runs_ahead_is_trimmed_to_bound_drift() {
        let buffer = Arc::new(Mutex::new(
            (0..100).map(|i| i as f32).collect::<VecDeque<_>>(),
        ));
        trim_backlog(&buffer, 10);
        let guard = buffer.lock().unwrap();
        assert_eq!(guard.len(), 10);
        // The oldest go, so the mix stays close to live rather than replaying a
        // two-hour-old backlog at the end.
        assert_eq!(guard.front().copied(), Some(90.0));
    }

    #[test]
    fn a_source_within_the_backlog_limit_is_left_alone() {
        let buffer = Arc::new(Mutex::new(VecDeque::from(vec![1.0, 2.0])));
        trim_backlog(&buffer, 10);
        assert_eq!(buffer.lock().unwrap().len(), 2);
    }

    #[test]
    fn recording_with_every_source_disabled_is_refused() {
        let options = CaptureOptions {
            mic: false,
            system: false,
            ..Default::default()
        };
        let error = match start(&options, std::env::temp_dir().join("never.wav")) {
            Ok(_) => panic!("a recording with no sources should be refused"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("nothing to record"), "{error}");
    }

    #[test]
    fn accumulated_levels_report_peak_and_rms() {
        let mut accumulator = Accumulator::default();
        for sample in [0.0, 0.5, -0.9, 0.2] {
            accumulator.push(sample);
        }
        let levels = accumulator.levels();
        assert!((levels.peak - 0.9).abs() < 1e-6);
        assert!(levels.rms > 0.0 && levels.rms < 0.9);
        assert!(!levels.is_silent());
    }
}
