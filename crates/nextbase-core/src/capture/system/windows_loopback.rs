//! Windows system audio via WASAPI loopback.
//!
//! cpal has no loopback capture, so this goes straight to the COM API. The pattern
//! (CoInitialize, device enumerator, activate an interface) is the same one
//! `media.rs` already uses for volume control.
//!
//! Loopback runs at the render endpoint's mix format, which cannot be negotiated —
//! usually 48 kHz float stereo. The rate is read from `GetMixFormat` and handed to the
//! mixer's resampler rather than assumed.

use anyhow::{anyhow, bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use super::super::{Sink, SourceHandle, SourceKind};

use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

/// 100-nanosecond units, which is what WASAPI buffer durations are measured in.
const REFTIMES_PER_SEC: i64 = 10_000_000;

// Format tags from mmreg.h. Spelled out rather than imported because windows-rs moves
// them between modules and gates them behind different features between releases —
// these are fixed ABI values, so a literal is the stable way to name them.
const WAVE_FORMAT_PCM: u32 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u32 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u32 = 0xFFFE;

fn initialise_com() {
    unsafe {
        // RPC_E_CHANGED_MODE only means COM is already up on this thread with another
        // model, which none of these calls care about.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

fn default_render_device() -> Result<windows::Win32::Media::Audio::IMMDevice> {
    initialise_com();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("Could not open the Windows audio device enumerator")?;
        enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("No default playback device found, so system audio cannot be captured")
    }
}

/// The playback device system audio would be captured from, for `doctor`.
///
/// Confirming the device exists is the part that matters; its human-readable name is
/// a nicety, so a failure to read the name degrades to a generic label rather than
/// making the whole check look broken.
pub(crate) fn render_device_name() -> Result<String> {
    let device = default_render_device()?;
    Ok(friendly_name(&device).unwrap_or_else(|| "Default playback device".to_string()))
}

fn friendly_name(device: &windows::Win32::Media::Audio::IMMDevice) -> Option<String> {
    use windows::core::BSTR;
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::System::Com::STGM_READ;

    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        // PROPVARIANT is an owning wrapper in windows-rs, so it frees itself and the
        // documented conversion replaces poking at the raw union.
        let name = BSTR::try_from(&value).ok()?.to_string();
        Some(name).filter(|name| !name.trim().is_empty())
    }
}

/// How the mix format encodes samples. Loopback hands back raw bytes, so this decides
/// how to read them.
#[derive(Debug, Clone, Copy)]
enum SampleEncoding {
    Float32,
    Int16,
    Int32,
}

fn encoding_of(format: &WAVEFORMATEX) -> Result<SampleEncoding> {
    // WAVE_FORMAT_EXTENSIBLE hides the real tag in a sub-format GUID, but the bit
    // depth is enough to tell the three cases apart in practice.
    let tag = format.wFormatTag as u32;
    match (tag, format.wBitsPerSample) {
        (WAVE_FORMAT_IEEE_FLOAT, _) => Ok(SampleEncoding::Float32),
        (WAVE_FORMAT_EXTENSIBLE, 32) => Ok(SampleEncoding::Float32),
        (WAVE_FORMAT_PCM, 16) | (WAVE_FORMAT_EXTENSIBLE, 16) => Ok(SampleEncoding::Int16),
        (WAVE_FORMAT_PCM, 32) => Ok(SampleEncoding::Int32),
        (tag, bits) => bail!(
            "Unsupported playback mix format (tag {tag}, {bits}-bit), so system audio cannot be captured."
        ),
    }
}

fn frame_to_mono(bytes: &[u8], channels: usize, encoding: SampleEncoding) -> f32 {
    let mut sum = 0.0f32;
    let width = match encoding {
        SampleEncoding::Float32 | SampleEncoding::Int32 => 4,
        SampleEncoding::Int16 => 2,
    };
    let mut counted = 0usize;

    for channel in 0..channels {
        let start = channel * width;
        let Some(slice) = bytes.get(start..start + width) else {
            break;
        };
        sum += match encoding {
            SampleEncoding::Float32 => f32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]),
            SampleEncoding::Int16 => {
                i16::from_le_bytes([slice[0], slice[1]]) as f32 / i16::MAX as f32
            }
            SampleEncoding::Int32 => {
                i32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]) as f32
                    / i32::MAX as f32
            }
        };
        counted += 1;
    }

    if counted == 0 {
        0.0
    } else {
        sum / counted as f32
    }
}

pub(crate) fn start() -> Result<SourceHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel::<Result<Sink>>();

    let thread_stop = Arc::clone(&stop);
    // COM interfaces are apartment-bound, so the client is created and used entirely
    // on this one thread.
    std::thread::spawn(move || {
        let result = (|| -> Result<(Sink, IAudioClient, IAudioCaptureClient, usize, SampleEncoding)> {
            let device = default_render_device()?;
            unsafe {
                let client: IAudioClient = device
                    .Activate(CLSCTX_ALL, None)
                    .context("Could not open the playback device for loopback capture")?;
                let format = client
                    .GetMixFormat()
                    .context("Could not read the playback mix format")?;
                if format.is_null() {
                    bail!("Windows returned no playback mix format.");
                }

                let channels = (*format).nChannels.max(1) as usize;
                let rate = (*format).nSamplesPerSec;
                let encoding = encoding_of(&*format)?;

                client
                    .Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_LOOPBACK,
                        REFTIMES_PER_SEC,
                        0,
                        format,
                        None,
                    )
                    .context("Could not start loopback capture on the playback device")?;
                CoTaskMemFree(Some(format as *const _));

                let capture: IAudioCaptureClient = client
                    .GetService()
                    .context("Could not obtain the loopback capture client")?;
                client.Start().context("Could not start the loopback stream")?;

                let sink = Sink::new(rate)?;
                Ok((sink, client, capture, channels, encoding))
            }
        })();

        let (sink, client, capture, channels, encoding) = match result {
            Ok(parts) => {
                let _ = ready_tx.send(Ok(parts.0.clone()));
                parts
            }
            Err(error) => {
                let _ = ready_tx.send(Err(error));
                return;
            }
        };

        let mut mono: Vec<f32> = Vec::new();
        while !thread_stop.load(Ordering::SeqCst) {
            unsafe {
                let Ok(available) = capture.GetNextPacketSize() else {
                    break;
                };
                if available == 0 {
                    // Nothing playing: loopback simply goes quiet, so poll rather
                    // than block, and let the mixer pad with silence.
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }

                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .is_err()
                {
                    break;
                }

                mono.clear();
                if !data.is_null() && frames > 0 {
                    let width = match encoding {
                        SampleEncoding::Int16 => 2,
                        _ => 4,
                    };
                    let stride = width * channels;
                    let bytes = std::slice::from_raw_parts(data, frames as usize * stride);
                    for frame in bytes.chunks_exact(stride) {
                        mono.push(frame_to_mono(frame, channels, encoding));
                    }
                }
                let _ = capture.ReleaseBuffer(frames);

                if !mono.is_empty() {
                    sink.push(&mono);
                }
            }
        }

        unsafe {
            let _ = client.Stop();
        }
    });

    let sink = match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(sink)) => sink,
        Ok(Err(error)) => {
            stop.store(true, Ordering::SeqCst);
            return Err(error);
        }
        Err(_) => {
            stop.store(true, Ordering::SeqCst);
            return Err(anyhow!("Loopback capture did not start within 5s."));
        }
    };

    let stop_flag = Arc::clone(&stop);
    Ok(SourceHandle {
        kind: SourceKind::System,
        buffer: sink.buffer_handle(),
        live: sink.live_handle(),
        stop: Box::new(move || stop_flag.store(true, Ordering::SeqCst)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_frames_are_averaged_across_channels() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0.5f32.to_le_bytes());
        bytes.extend_from_slice(&(-0.1f32).to_le_bytes());
        let mono = frame_to_mono(&bytes, 2, SampleEncoding::Float32);
        assert!((mono - 0.2).abs() < 1e-6, "{mono}");
    }

    #[test]
    fn sixteen_bit_frames_are_scaled_to_unit_range() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&i16::MAX.to_le_bytes());
        bytes.extend_from_slice(&i16::MAX.to_le_bytes());
        assert!((frame_to_mono(&bytes, 2, SampleEncoding::Int16) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_truncated_frame_does_not_panic() {
        // A short read must go quiet rather than index out of bounds mid-meeting.
        assert_eq!(frame_to_mono(&[0, 0], 2, SampleEncoding::Float32), 0.0);
    }
}
