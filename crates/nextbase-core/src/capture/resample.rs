//! Rate conversion to 16 kHz.
//!
//! Sources are asked for 16 kHz directly wherever the platform allows it, so this is
//! the fallback path — mainly Windows loopback, which is fixed at whatever the render
//! device runs at. Decimating by dropping samples would alias speech harmonics down
//! into the voice band and cost transcription accuracy, so it goes through a proper
//! polynomial resampler.

use anyhow::{Context, Result};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, PolynomialDegree, Resampler as _};

/// Input frames handed to rubato at a time. Audio callbacks arrive in bursts of a few
/// hundred frames, so this buffers a little before converting.
const CHUNK: usize = 1024;

pub struct Resampler {
    inner: Async<f32>,
    pending: Vec<f32>,
    output: Vec<f32>,
    ratio: f64,
}

impl Resampler {
    /// `None` when no conversion is needed — the common case, and it must cost
    /// nothing.
    pub fn new(from: u32, to: u32) -> Result<Option<Self>> {
        if from == 0 {
            anyhow::bail!("Audio source reported a sample rate of zero.");
        }
        if from == to {
            return Ok(None);
        }

        let ratio = to as f64 / from as f64;
        let inner = Async::new_poly(
            ratio,
            1.0,
            // Cubic is the quality/cost balance that matters here: this runs inside an
            // audio callback path for hours.
            PolynomialDegree::Cubic,
            CHUNK,
            1,
            FixedAsync::Input,
        )
        .context("Could not create the audio resampler")?;

        Ok(Some(Self {
            inner,
            pending: Vec::with_capacity(CHUNK * 2),
            output: Vec::new(),
            ratio,
        }))
    }

    /// Convert what it can and hold the remainder for the next call.
    pub fn push(&mut self, samples: &[f32]) -> Vec<f32> {
        self.pending.extend_from_slice(samples);

        let mut converted = Vec::with_capacity(
            ((self.pending.len() as f64 * self.ratio).ceil() as usize).saturating_add(CHUNK),
        );
        let capacity = self.inner.output_frames_max();
        if self.output.len() < capacity {
            self.output.resize(capacity, 0.0);
        }

        while self.pending.len() >= CHUNK {
            let input = match InterleavedSlice::new(&self.pending[..CHUNK], 1, CHUNK) {
                Ok(input) => input,
                Err(_) => break,
            };
            let frames = self.output.len();
            let mut output = match InterleavedSlice::new_mut(&mut self.output, 1, frames) {
                Ok(output) => output,
                Err(_) => break,
            };

            match self.inner.process_into_buffer(&input, &mut output, None) {
                Ok((consumed, produced)) => {
                    converted.extend_from_slice(&self.output[..produced]);
                    // A resampler that consumed nothing would spin forever.
                    let consumed = consumed.max(1).min(self.pending.len());
                    self.pending.drain(..consumed);
                }
                Err(_) => {
                    self.pending.clear();
                    break;
                }
            }
        }

        converted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_rates_need_no_resampler() {
        assert!(Resampler::new(16_000, 16_000).unwrap().is_none());
    }

    #[test]
    fn a_zero_rate_is_an_error_rather_than_a_division_by_zero() {
        assert!(Resampler::new(0, 16_000).is_err());
    }

    #[test]
    fn downsampling_produces_roughly_the_expected_frame_count() {
        let mut resampler = Resampler::new(48_000, 16_000).unwrap().expect("resampler");

        // One second of 48 kHz should come out near 16 kHz worth of frames. The
        // resampler holds a little for its interpolation window, so this checks the
        // ratio rather than an exact count.
        let input: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
        let output = resampler.push(&input);

        assert!(
            (15_000..=16_100).contains(&output.len()),
            "got {} frames",
            output.len()
        );
        assert!(output.iter().all(|s| s.abs() <= 1.0));
        // Real signal, not silence: aliasing or a broken path would flatten it.
        assert!(output.iter().any(|s| s.abs() > 0.1));
    }

    #[test]
    fn upsampling_also_works_for_an_unusual_device_rate() {
        let mut resampler = Resampler::new(8_000, 16_000).unwrap().expect("resampler");
        // Chunk-aligned, so the whole input is consumed rather than leaving a tail
        // held for the next call.
        let frames = CHUNK * 8;
        let input: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.05).sin() * 0.4).collect();
        let output = resampler.push(&input);

        let expected = frames * 2;
        assert!(
            output.len().abs_diff(expected) < 64,
            "got {}, expected near {expected}",
            output.len()
        );
    }

    #[test]
    fn samples_below_the_chunk_size_are_held_until_there_are_enough() {
        let mut resampler = Resampler::new(48_000, 16_000).unwrap().expect("resampler");
        // Audio callbacks routinely deliver fewer frames than the chunk size.
        assert!(resampler.push(&[0.1; 100]).is_empty());
        assert!(resampler.push(&[0.1; 100]).is_empty());
        // Once past the chunk size, output appears rather than being lost.
        assert!(!resampler.push(&[0.1; 1000]).is_empty());
    }

    #[test]
    fn a_long_stream_of_callbacks_keeps_converting() {
        let mut resampler = Resampler::new(44_100, 16_000).unwrap().expect("resampler");
        let mut total = 0usize;
        // 100 callbacks of 512 frames, the shape of a real capture session.
        for _ in 0..100 {
            total += resampler.push(&[0.2; 512]).len();
        }
        let expected = (100.0 * 512.0 * 16_000.0 / 44_100.0) as usize;
        assert!(
            total.abs_diff(expected) < 2_000,
            "got {total}, expected near {expected}"
        );
    }
}
