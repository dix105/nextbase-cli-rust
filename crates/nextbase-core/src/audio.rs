//! Microphone capture.
//!
//! This replaces the SoX subprocess entirely: `cpal` opens the input device in
//! process and `hound` writes the WAV, so there is no external binary to install,
//! no `q`-to-stdin stop protocol, and no `taskkill` fallback.
//!
//! Levels are accumulated while recording rather than by re-reading the file with
//! `sox stat` afterwards, which also removes a second subprocess per dictation.

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::paths;

pub const DEFAULT_DEVICE: &str = "default";

/// Peak and RMS amplitude, both 0.0-1.0, matching the numbers the old `sox stat`
/// parsing produced so the existing silence thresholds still apply.
#[derive(Debug, Clone, Copy, Default)]
pub struct Levels {
    pub peak: f32,
    pub rms: f32,
}

impl Levels {
    /// Field-tuned thresholds carried over from the TypeScript listener: a real
    /// microphone that heard nothing still reports a tiny non-zero floor.
    pub fn is_silent(&self) -> bool {
        self.peak < 0.0001 && self.rms < 0.00005
    }

    /// Ranking score for device probing. RMS is weighted up because a probe is
    /// short and peaks are noisy.
    pub fn score(&self) -> f32 {
        self.peak.max(self.rms * 10.0)
    }
}

/// Most recent chunk levels, published from the audio callback so a UI can draw a
/// meter without waiting for the recording to finish. `f32` bits in atomics keeps
/// the callback lock-free — it must never block.
#[derive(Debug, Default)]
pub struct LiveLevel {
    peak: std::sync::atomic::AtomicU32,
    rms: std::sync::atomic::AtomicU32,
}

impl LiveLevel {
    fn publish(&self, levels: Levels) {
        self.peak.store(levels.peak.to_bits(), Ordering::Relaxed);
        self.rms.store(levels.rms.to_bits(), Ordering::Relaxed);
    }

    pub fn get(&self) -> Levels {
        Levels {
            peak: f32::from_bits(self.peak.load(Ordering::Relaxed)),
            rms: f32::from_bits(self.rms.load(Ordering::Relaxed)),
        }
    }
}

#[derive(Debug, Default)]
struct LevelAccumulator {
    peak: f32,
    sum_squares: f64,
    samples: u64,
}

impl LevelAccumulator {
    fn push(&mut self, sample: f32) {
        let magnitude = sample.abs();
        if magnitude > self.peak {
            self.peak = magnitude;
        }
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

pub struct Finished {
    pub path: PathBuf,
    pub duration: Duration,
    pub levels: Levels,
}

/// A recording in progress. The `cpal` stream is not `Send` on every platform, so
/// it is owned by a dedicated thread and controlled with a channel.
pub struct Recording {
    path: PathBuf,
    started: Instant,
    stop: Arc<AtomicBool>,
    live: Arc<LiveLevel>,
    worker: Option<std::thread::JoinHandle<Result<Levels>>>,
}

impl Recording {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Levels from the most recent audio callback, for a live meter.
    pub fn live_levels(&self) -> Levels {
        self.live.get()
    }

    pub fn stop(mut self) -> Result<Finished> {
        let duration = self.started.elapsed();
        self.stop.store(true, Ordering::SeqCst);
        let levels = match self.worker.take() {
            Some(worker) => worker
                .join()
                .map_err(|_| anyhow!("Recording thread panicked"))??,
            None => Levels::default(),
        };
        Ok(Finished {
            path: self.path.clone(),
            duration,
            levels,
        })
    }

    /// Abandon the recording and delete the partial file.
    pub fn cancel(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        // Never leave the input device open if a caller drops mid-recording.
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn host_input_device(name: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();

    match name {
        None | Some(DEFAULT_DEVICE) | Some("") => host
            .default_input_device()
            .context("No default microphone found. Check your system sound input settings."),
        Some(wanted) => {
            let mut devices = host
                .input_devices()
                .context("Could not enumerate microphones")?;
            devices
                .find(|device| device.name().map(|n| n == wanted).unwrap_or(false))
                .with_context(|| {
                    format!("Microphone \"{wanted}\" is not available. Run: wisper mic --auto")
                })
        }
    }
}

pub fn new_recording_path() -> Result<PathBuf> {
    let dir = paths::tmp_dir();
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f");
    Ok(dir.join(format!("recording-{stamp}.wav")))
}

/// Start capturing to `path`.
///
/// Recording happens at the device's native sample rate, downmixed to mono 16-bit.
/// Providers resample server side, so no client-side resampler is pulled in; the
/// tradeoff is a larger upload than SoX's fixed 16 kHz.
pub fn start(device_name: Option<&str>, path: PathBuf) -> Result<Recording> {
    let device = host_input_device(device_name)?;
    let supported = device
        .default_input_config()
        .context("Microphone did not report a usable input format")?;

    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let channels = config.channels as usize;
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: config.sample_rate.0,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let live = Arc::new(LiveLevel::default());
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

    let thread_stop = Arc::clone(&stop);
    let thread_live = Arc::clone(&live);
    let thread_path = path.clone();
    let worker = std::thread::spawn(move || -> Result<Levels> {
        let writer = match hound::WavWriter::create(&thread_path, spec) {
            Ok(writer) => Arc::new(Mutex::new(Some(writer))),
            Err(error) => {
                let _ = ready_tx.send(Err(anyhow!(
                    "Could not open {} for writing: {error}",
                    thread_path.display()
                )));
                return Err(anyhow!(error));
            }
        };
        let levels = Arc::new(Mutex::new(LevelAccumulator::default()));

        let write_frames = {
            let writer = Arc::clone(&writer);
            let levels = Arc::clone(&levels);
            let live = Arc::clone(&thread_live);
            move |mono: &[f32]| {
                let mut guard = match writer.lock() {
                    Ok(guard) => guard,
                    Err(_) => return,
                };
                let Some(writer) = guard.as_mut() else { return };
                let mut accumulator = match levels.lock() {
                    Ok(accumulator) => accumulator,
                    Err(_) => return,
                };
                let mut chunk = LevelAccumulator::default();
                for sample in mono {
                    accumulator.push(*sample);
                    chunk.push(*sample);
                    let clamped = sample.clamp(-1.0, 1.0);
                    let _ = writer.write_sample((clamped * i16::MAX as f32) as i16);
                }
                live.publish(chunk.levels());
            }
        };

        // Downmix to mono by averaging each frame's channels.
        let to_mono = move |interleaved: &[f32], out: &mut Vec<f32>| {
            out.clear();
            if channels <= 1 {
                out.extend_from_slice(interleaved);
                return;
            }
            for frame in interleaved.chunks(channels) {
                out.push(frame.iter().sum::<f32>() / frame.len() as f32);
            }
        };

        let error_callback = |error| eprintln!("Microphone stream error: {error}");

        let stream = {
            let build = |device: &cpal::Device| -> Result<cpal::Stream, cpal::BuildStreamError> {
                match sample_format {
                    cpal::SampleFormat::F32 => {
                        let mut scratch = Vec::new();
                        device.build_input_stream(
                            &config,
                            move |data: &[f32], _| {
                                to_mono(data, &mut scratch);
                                write_frames(&scratch);
                            },
                            error_callback,
                            None,
                        )
                    }
                    cpal::SampleFormat::I16 => {
                        let mut scratch = Vec::new();
                        let mut floats = Vec::new();
                        device.build_input_stream(
                            &config,
                            move |data: &[i16], _| {
                                floats.clear();
                                floats.extend(data.iter().map(|s| *s as f32 / i16::MAX as f32));
                                to_mono(&floats, &mut scratch);
                                write_frames(&scratch);
                            },
                            error_callback,
                            None,
                        )
                    }
                    cpal::SampleFormat::U16 => {
                        let mut scratch = Vec::new();
                        let mut floats = Vec::new();
                        device.build_input_stream(
                            &config,
                            move |data: &[u16], _| {
                                floats.clear();
                                floats.extend(
                                    data.iter().map(|s| (*s as f32 - 32768.0) / i16::MAX as f32),
                                );
                                to_mono(&floats, &mut scratch);
                                write_frames(&scratch);
                            },
                            error_callback,
                            None,
                        )
                    }
                    other => {
                        eprintln!("Unsupported microphone sample format: {other:?}");
                        Err(cpal::BuildStreamError::StreamConfigNotSupported)
                    }
                }
            };

            match build(&device) {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = ready_tx.send(Err(anyhow!("Could not open microphone: {error}")));
                    return Err(anyhow!(error));
                }
            }
        };

        if let Err(error) = stream.play() {
            let _ = ready_tx.send(Err(anyhow!("Could not start the microphone: {error}")));
            return Err(anyhow!(error));
        }

        let _ = ready_tx.send(Ok(()));

        while !thread_stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
        }

        drop(stream);
        if let Ok(mut guard) = writer.lock() {
            if let Some(writer) = guard.take() {
                writer.finalize().ok();
            }
        }

        Ok(levels.lock().map(|l| l.levels()).unwrap_or_default())
    });

    // Surface "device is busy / does not exist" as a normal error instead of a
    // silent zero-length recording.
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = worker.join();
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        Err(_) => {
            stop.store(true, Ordering::SeqCst);
            let _ = worker.join();
            let _ = std::fs::remove_file(&path);
            bail!("Microphone did not start within 5s. Check microphone permission for your terminal.");
        }
    }

    Ok(Recording {
        path,
        started: Instant::now(),
        stop,
        live,
        worker: Some(worker),
    })
}

// ------------------------------------------------------------------ devices

#[derive(Debug, Clone)]
pub struct DeviceProbe {
    pub device: String,
    pub score: f32,
    pub ok: bool,
    pub has_signal: bool,
    pub error: Option<String>,
}

/// Loopback and virtual cables enumerate like microphones but usually carry no
/// live input, so they are ranked last rather than hidden.
pub fn is_likely_virtual(device: &str) -> bool {
    let name = device.to_lowercase();
    [
        "virtual",
        "relay",
        "cable",
        "vb-audio",
        "voicemeeter",
        "loopback",
        "aggregate",
    ]
    .iter()
    .any(|marker| name.contains(marker))
}

pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut names: Vec<String> = host
        .input_devices()
        .map(|devices| devices.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default();
    names.dedup();
    names
}

/// Record a short sample to judge whether a device works.
///
/// `ok` only means the device opened. A user can be silent during the probe, so a
/// zero level must never make a real microphone look broken — signal is a ranking
/// preference, not a pass/fail.
pub fn probe_input_device(name: &str) -> DeviceProbe {
    let path = match new_recording_path() {
        Ok(path) => path.with_file_name(format!("probe-{}.wav", std::process::id())),
        Err(error) => {
            return DeviceProbe {
                device: name.to_string(),
                score: 0.0,
                ok: false,
                has_signal: false,
                error: Some(error.to_string()),
            }
        }
    };

    let result = start(Some(name), path.clone()).and_then(|recording| {
        std::thread::sleep(Duration::from_millis(600));
        recording.stop()
    });
    let _ = std::fs::remove_file(&path);

    match result {
        Ok(finished) => DeviceProbe {
            device: name.to_string(),
            score: finished.levels.score(),
            ok: true,
            has_signal: !finished.levels.is_silent(),
            error: None,
        },
        Err(error) => DeviceProbe {
            device: name.to_string(),
            score: 0.0,
            ok: false,
            has_signal: false,
            error: Some(error.to_string()),
        },
    }
}

pub struct AutoDetect {
    pub device: String,
    pub probes: Vec<DeviceProbe>,
}

/// Try the configured device first, then real microphones, then virtual ones.
/// Prefer a device that actually heard something, then the strongest signal.
pub fn auto_detect_input_device(configured: Option<&str>) -> AutoDetect {
    let devices = list_input_devices();
    let mut ordered: Vec<String> = Vec::new();

    for candidate in configured
        .filter(|c| !c.is_empty() && *c != DEFAULT_DEVICE)
        .map(|c| c.to_string())
        .into_iter()
        .chain(devices.iter().filter(|d| !is_likely_virtual(d)).cloned())
        .chain(devices.iter().filter(|d| is_likely_virtual(d)).cloned())
        .chain(std::iter::once(DEFAULT_DEVICE.to_string()))
    {
        if !ordered.contains(&candidate) {
            ordered.push(candidate);
        }
    }

    let probes: Vec<DeviceProbe> = ordered
        .iter()
        .map(|name| probe_input_device(name))
        .collect();

    let mut usable: Vec<&DeviceProbe> = probes.iter().filter(|probe| probe.ok).collect();
    usable.sort_by(|a, b| {
        let real = is_likely_virtual(&a.device).cmp(&is_likely_virtual(&b.device));
        let signal = b.has_signal.cmp(&a.has_signal);
        let score = b
            .score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal);
        real.then(signal).then(score)
    });

    let device = usable
        .first()
        .map(|probe| probe.device.clone())
        .or_else(|| devices.iter().find(|d| !is_likely_virtual(d)).cloned())
        .unwrap_or_else(|| DEFAULT_DEVICE.to_string());

    AutoDetect { device, probes }
}

/// Delete stale recordings: keep the newest `max_files`, drop anything older than
/// `max_age`.
pub fn cleanup_old_recordings(max_files: usize, max_age: Duration) -> Result<()> {
    let dir = paths::tmp_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(());
    };

    let mut files: Vec<(PathBuf, std::time::SystemTime)> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .map(|ext| ext.eq_ignore_ascii_case("wav"))
                .unwrap_or(false)
        })
        .filter_map(|entry| {
            let modified = entry.metadata().and_then(|m| m.modified()).ok()?;
            Some((entry.path(), modified))
        })
        .collect();

    files.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));

    let now = std::time::SystemTime::now();
    for (index, (path, modified)) in files.iter().enumerate() {
        let too_old = now
            .duration_since(*modified)
            .map(|age| age > max_age)
            .unwrap_or(false);
        if index >= max_files || too_old {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_uses_the_field_tuned_floor() {
        assert!(Levels {
            peak: 0.00005,
            rms: 0.00001
        }
        .is_silent());
        assert!(!Levels {
            peak: 0.02,
            rms: 0.005
        }
        .is_silent());
    }

    #[test]
    fn rms_is_weighted_up_when_scoring_devices() {
        let quiet_but_steady = Levels {
            peak: 0.05,
            rms: 0.02,
        };
        assert!((quiet_but_steady.score() - 0.2).abs() < 1e-6);
    }

    #[test]
    fn virtual_devices_are_recognised() {
        for name in [
            "VB-Audio Virtual Cable",
            "BlackHole 2ch Loopback",
            "VoiceMeeter Output",
            "Aggregate Device",
        ] {
            assert!(is_likely_virtual(name), "{name}");
        }
        assert!(!is_likely_virtual("MacBook Pro Microphone"));
        assert!(!is_likely_virtual("Shure MV7"));
    }

    #[test]
    fn accumulator_tracks_peak_and_rms() {
        let mut accumulator = LevelAccumulator::default();
        for sample in [0.0, 0.5, -0.8, 0.3] {
            accumulator.push(sample);
        }
        let levels = accumulator.levels();
        assert!((levels.peak - 0.8).abs() < 1e-6);
        assert!(levels.rms > 0.0 && levels.rms < 0.8);
    }

    #[test]
    fn empty_accumulator_is_silent_not_nan() {
        let levels = LevelAccumulator::default().levels();
        assert_eq!(levels.rms, 0.0);
        assert!(levels.is_silent());
    }
}
