//! Microphone source for meeting capture.
//!
//! Separate from `audio.rs` because the requirements differ: dictation writes the
//! device's native rate straight to a file, while a meeting needs mono 16 kHz frames
//! handed to a mixer. Device selection is shared — `audio::list_input_devices` and
//! the virtual-device ranking still apply.

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use super::{Sink, SourceHandle, SourceKind};
use crate::audio::DEFAULT_DEVICE;
use crate::wav::TARGET_SAMPLE_RATE;

fn device(name: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    match name {
        None | Some(DEFAULT_DEVICE) | Some("") => host
            .default_input_device()
            .context("No default microphone found. Check your system sound input settings."),
        Some(wanted) => host
            .input_devices()
            .context("Could not enumerate microphones")?
            .find(|device| device.name().map(|n| n == wanted).unwrap_or(false))
            .with_context(|| {
                format!("Audio device \"{wanted}\" is not available. Run: nbmeet doctor")
            }),
    }
}

/// Prefer a native 16 kHz config so the resampler can be skipped entirely.
///
/// Most built-in microphones and headsets offer 16 kHz; when one does not, the
/// default config is used and the mixer resamples.
fn pick_config(device: &cpal::Device) -> Result<(cpal::StreamConfig, cpal::SampleFormat)> {
    if let Ok(ranges) = device.supported_input_configs() {
        for range in ranges {
            if range.min_sample_rate().0 <= TARGET_SAMPLE_RATE
                && range.max_sample_rate().0 >= TARGET_SAMPLE_RATE
            {
                let supported = range.with_sample_rate(cpal::SampleRate(TARGET_SAMPLE_RATE));
                let format = supported.sample_format();
                return Ok((supported.into(), format));
            }
        }
    }

    let default = device
        .default_input_config()
        .context("Microphone did not report a usable input format")?;
    let format = default.sample_format();
    Ok((default.into(), format))
}

pub(crate) fn start(name: Option<&str>) -> Result<SourceHandle> {
    open(SourceKind::Mic, name)
}

/// Open any cpal input device as a source. Linux system audio is a "monitor"
/// input, so it comes through this same path rather than a platform backend.
pub(crate) fn open(kind: SourceKind, name: Option<&str>) -> Result<SourceHandle> {
    let device = device(name)?;
    let (config, sample_format) = pick_config(&device)?;
    let channels = config.channels.max(1) as usize;
    let sink = Sink::new(config.sample_rate.0)?;

    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

    // cpal streams are not `Send` on every platform, so the stream lives entirely on
    // its own thread and is controlled with a flag — the same shape `audio.rs` uses.
    let thread_stop = Arc::clone(&stop);
    let thread_sink = sink.clone();
    std::thread::spawn(move || {
        let error_callback = |error| crate::log::log(&format!("Microphone stream error: {error}"));

        let mono = move |interleaved: &[f32], out: &mut Vec<f32>| {
            out.clear();
            if channels <= 1 {
                out.extend_from_slice(interleaved);
                return;
            }
            for frame in interleaved.chunks(channels) {
                out.push(frame.iter().sum::<f32>() / frame.len() as f32);
            }
        };

        let build = || -> Result<cpal::Stream, cpal::BuildStreamError> {
            match sample_format {
                cpal::SampleFormat::F32 => {
                    let sink = thread_sink.clone();
                    let mut scratch = Vec::new();
                    device.build_input_stream(
                        &config,
                        move |data: &[f32], _| {
                            mono(data, &mut scratch);
                            sink.push(&scratch);
                        },
                        error_callback,
                        None,
                    )
                }
                cpal::SampleFormat::I16 => {
                    let sink = thread_sink.clone();
                    let mut scratch = Vec::new();
                    let mut floats = Vec::new();
                    device.build_input_stream(
                        &config,
                        move |data: &[i16], _| {
                            floats.clear();
                            floats.extend(data.iter().map(|s| *s as f32 / i16::MAX as f32));
                            mono(&floats, &mut scratch);
                            sink.push(&scratch);
                        },
                        error_callback,
                        None,
                    )
                }
                cpal::SampleFormat::U16 => {
                    let sink = thread_sink.clone();
                    let mut scratch = Vec::new();
                    let mut floats = Vec::new();
                    device.build_input_stream(
                        &config,
                        move |data: &[u16], _| {
                            floats.clear();
                            floats.extend(
                                data.iter().map(|s| (*s as f32 - 32768.0) / i16::MAX as f32),
                            );
                            mono(&floats, &mut scratch);
                            sink.push(&scratch);
                        },
                        error_callback,
                        None,
                    )
                }
                other => {
                    crate::log::log(&format!("Unsupported microphone sample format: {other:?}"));
                    Err(cpal::BuildStreamError::StreamConfigNotSupported)
                }
            }
        };

        let stream = match build() {
            Ok(stream) => stream,
            Err(error) => {
                let _ = ready_tx.send(Err(anyhow!("Could not open the microphone: {error}")));
                return;
            }
        };
        if let Err(error) = stream.play() {
            let _ = ready_tx.send(Err(anyhow!("Could not start the microphone: {error}")));
            return;
        }
        let _ = ready_tx.send(Ok(()));

        while !thread_stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(stream);
    });

    // Surface "busy" or "no such device" now, rather than as a silent hour of
    // nothing.
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            stop.store(true, Ordering::SeqCst);
            return Err(error);
        }
        Err(_) => {
            stop.store(true, Ordering::SeqCst);
            bail!("Microphone did not start within 5s. Check microphone permission for your terminal.");
        }
    }

    let stop_flag = Arc::clone(&stop);
    Ok(SourceHandle {
        kind,
        buffer: sink.buffer_handle(),
        live: sink.live_handle(),
        stop: Box::new(move || stop_flag.store(true, Ordering::SeqCst)),
    })
}
