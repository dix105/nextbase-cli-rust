//! macOS system audio via ScreenCaptureKit.
//!
//! There is no loopback input device on macOS to open, so the only routes are
//! ScreenCaptureKit or making the user install BlackHole and re-point their output
//! device. This takes the first: it costs a Screen Recording permission but no
//! hardware setup.
//!
//! Two wrinkles, both verified on this machine rather than assumed:
//!
//! 1. **Audio-only streams need macOS 15.** Before that a stream must carry video, so
//!    a 2×2 frame is requested and every frame discarded.
//! 2. **Linking pulls in the Swift runtime.** The binary needs an rpath to
//!    `/usr/lib/swift` or it dies at startup with "no LC_RPATH's found" — handled in
//!    `nextbase-cli/build.rs`.

use anyhow::{anyhow, Result};
use screencapturekit::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::super::{Sink, SourceHandle, SourceKind};
use super::SystemAudioStatus;

extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// ScreenCaptureKit is asked for this rate explicitly rather than for 16 kHz.
///
/// Requesting an unusual rate and *assuming* it was honoured would silently pitch the
/// whole meeting if it were not; 48 kHz is the documented default, so the rate is
/// known and the mixer resamples from a fixed, verified number.
const STREAM_RATE: u32 = 48_000;

pub(crate) fn is_permitted() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Show the system Screen Recording prompt. Returns whether it is already granted.
pub(crate) fn request_screen_recording() -> bool {
    if is_permitted() {
        return true;
    }
    unsafe { CGRequestScreenCaptureAccess() }
}

pub(crate) fn status() -> SystemAudioStatus {
    if !is_permitted() {
        return SystemAudioStatus::PermissionRequired {
            hint: "Screen Recording permission is needed to capture what the other participants say. nbmeet doctor can ask for it.".to_string(),
        };
    }
    match SCShareableContent::get() {
        Ok(content) if !content.displays().is_empty() => SystemAudioStatus::Ready,
        Ok(_) => SystemAudioStatus::Unavailable {
            reason: "macOS reported no capturable display.".to_string(),
        },
        Err(error) => SystemAudioStatus::Unavailable {
            reason: format!("ScreenCaptureKit is unavailable: {error:?}"),
        },
    }
}

/// Receives audio callbacks on ScreenCaptureKit's queue and forwards mono frames.
struct AudioTap {
    sink: Sink,
    stopped: Arc<AtomicBool>,
}

impl SCStreamOutputTrait for AudioTap {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        // The 2x2 video frames exist only to make the stream legal; drop them.
        if of_type != SCStreamOutputType::Audio || self.stopped.load(Ordering::Relaxed) {
            return;
        }
        let Some(list) = sample.audio_buffer_list() else {
            return;
        };

        // ScreenCaptureKit delivers deinterleaved f32: one buffer per channel, each
        // holding that channel's samples. Downmix by averaging across buffers.
        let channels = list.num_buffers();
        if channels == 0 {
            return;
        }

        let mut mono: Vec<f32> = Vec::new();
        for index in 0..channels {
            let Some(buffer) = list.buffer(index) else {
                continue;
            };
            let bytes = buffer.data();
            let frames = bytes.len() / 4;
            if mono.len() < frames {
                mono.resize(frames, 0.0);
            }
            for (frame, chunk) in bytes.chunks_exact(4).enumerate() {
                let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                mono[frame] += value / channels as f32;
            }
        }

        if !mono.is_empty() {
            self.sink.push(&mono);
        }
    }
}

pub(crate) fn start() -> Result<SourceHandle> {
    if !is_permitted() {
        return Err(anyhow!(
            "Screen Recording permission is not granted, so the other participants cannot be recorded. Run: nbmeet doctor"
        ));
    }

    let content = SCShareableContent::get()
        .map_err(|error| anyhow!("ScreenCaptureKit could not list displays: {error:?}"))?;
    let displays = content.displays();
    let display = displays
        .first()
        .ok_or_else(|| anyhow!("macOS reported no capturable display."))?;

    let filter = SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build();

    let configuration = SCStreamConfiguration::new()
        .with_width(2)
        .with_height(2)
        .with_captures_audio(true)
        .with_sample_rate(STREAM_RATE as i32)
        .with_channel_count(2)
        // Without this, our own output would be recorded back into the meeting.
        .with_excludes_current_process_audio(true);

    let sink = Sink::new(STREAM_RATE)?;
    let stopped = Arc::new(AtomicBool::new(false));

    let mut stream = SCStream::new(&filter, &configuration);
    stream.add_output_handler(
        AudioTap {
            sink: sink.clone(),
            stopped: Arc::clone(&stopped),
        },
        SCStreamOutputType::Audio,
    );
    stream
        .start_capture()
        .map_err(|error| anyhow!("Could not start system audio capture: {error:?}"))?;

    let buffer = sink.buffer_handle();
    let live = sink.live_handle();
    let stop_flag = Arc::clone(&stopped);

    Ok(SourceHandle {
        kind: SourceKind::System,
        buffer,
        live,
        stop: Box::new(move || {
            // Flag first: the callback runs on ScreenCaptureKit's own queue and may
            // fire once more while the stream is being torn down.
            stop_flag.store(true, Ordering::SeqCst);
            let _ = stream.stop_capture();
        }),
    })
}
