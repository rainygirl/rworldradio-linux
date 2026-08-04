//! The decoded-audio side of playback: a ring buffer between the decoder thread
//! and cpal's audio callback, plus the rate/channel conversion needed to feed
//! whatever the ALSA device's default config happens to be.
//!
//! On Haiku, `BSoundPlayer` was constructed from the decoder's negotiated
//! `media_raw_audio_format` and the Media Kit did any conversion. cpal has no
//! equivalent, so the app opens the device's default output config once and
//! converts into it here.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Condvar, Mutex};

/// Roughly 400ms of stereo audio at 48kHz. Big enough to ride out scheduler
/// jitter and an HLS segment boundary, small enough that stopping is audibly
/// immediate.
const RING_CAPACITY_FRAMES: usize = 48_000 * 2 / 5;

/// Shared between the decoder thread (producer), the cpal callback (consumer)
/// and the UI thread (level meter).
pub struct AudioSink {
    ring: Mutex<VecDeque<f32>>,
    space_available: Condvar,
    capacity: usize,
    /// Set by `stop` - the callback outputs silence at once instead of draining
    /// whatever is still buffered.
    muted: AtomicBool,
    /// Peak amplitude of the most recent callback, as IEEE-754 bits (the same
    /// trick the Haiku build used with `atomic_set`/`atomic_get`).
    level_bits: AtomicU32,
}

impl AudioSink {
    pub fn new(channels: usize) -> Self {
        AudioSink {
            ring: Mutex::new(VecDeque::new()),
            space_available: Condvar::new(),
            capacity: RING_CAPACITY_FRAMES * channels.max(1),
            muted: AtomicBool::new(false),
            level_bits: AtomicU32::new(0),
        }
    }

    /// Appends interleaved samples, blocking while the ring is full. This is
    /// what paces the decoder to real time. Returns false once muted, which is
    /// the decoder's signal to stop.
    pub fn push(&self, samples: &[f32]) -> bool {
        let mut ring = self.ring.lock().unwrap();
        for &sample in samples {
            while ring.len() >= self.capacity {
                if self.muted.load(Ordering::Relaxed) {
                    return false;
                }
                let (guard, _) = self
                    .space_available
                    .wait_timeout(ring, std::time::Duration::from_millis(100))
                    .unwrap();
                ring = guard;
            }
            ring.push_back(sample);
        }
        true
    }

    /// Fills `out` from the ring, padding with silence on underrun, and updates
    /// the level meter. Called from the audio callback - no allocation, short
    /// critical section, never blocks.
    pub fn fill(&self, out: &mut [f32]) {
        if self.muted.load(Ordering::Relaxed) {
            out.fill(0.0);
            self.level_bits.store(0f32.to_bits(), Ordering::Relaxed);
            return;
        }

        let produced = {
            let mut ring = self.ring.lock().unwrap();
            let count = out.len().min(ring.len());
            for slot in out.iter_mut().take(count) {
                *slot = ring.pop_front().unwrap();
            }
            count
        };
        self.space_available.notify_all();

        if produced < out.len() {
            out[produced..].fill(0.0);
        }

        let mut peak = 0.0f32;
        for &sample in out[..produced].iter() {
            let magnitude = sample.abs();
            if magnitude > peak {
                peak = magnitude;
            }
        }
        self.level_bits
            .store(peak.min(1.0).to_bits(), Ordering::Relaxed);
    }

    /// Silences output immediately and releases any blocked producer.
    pub fn mute(&self) {
        self.muted.store(true, Ordering::Relaxed);
        self.ring.lock().unwrap().clear();
        self.space_available.notify_all();
        self.level_bits.store(0f32.to_bits(), Ordering::Relaxed);
    }

    /// Peak amplitude (0.0-1.0) of the most recently played buffer. Cheap and
    /// thread-safe to poll from the UI thread - it's just an atomic read.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level_bits.load(Ordering::Relaxed))
    }

}

/// Linear-interpolating resampler and channel mapper: decoded stream format in,
/// device format out.
///
/// Linear interpolation is a deliberate choice over a windowed/polyphase
/// resampler: the common cases are 44.1k -> 48k and 48k -> 48k for lossy
/// 64-320kbps radio audio, where the interpolation artifacts sit far below the
/// codec's own, and it needs no extra dependency.
pub struct Converter {
    in_channels: usize,
    out_channels: usize,
    /// Input frames consumed per output frame.
    step: f64,
    /// Fractional read position within the current input buffer.
    position: f64,
    /// Last frame of the previous buffer, so interpolation is continuous across
    /// buffer boundaries.
    previous: Vec<f32>,
    have_previous: bool,
    passthrough: bool,
}

impl Converter {
    pub fn new(in_channels: usize, in_rate: u32, out_channels: usize, out_rate: u32) -> Self {
        let step = f64::from(in_rate.max(1)) / f64::from(out_rate.max(1));
        Converter {
            in_channels: in_channels.max(1),
            out_channels: out_channels.max(1),
            step,
            position: 0.0,
            previous: vec![0.0; in_channels.max(1)],
            have_previous: false,
            passthrough: in_rate == out_rate && in_channels == out_channels,
        }
    }

    /// Converts one interleaved input buffer, appending to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }

        let frames = input.len() / self.in_channels;
        if frames == 0 {
            return;
        }

        let mut frame_in = vec![0.0f32; self.in_channels];
        while self.position < frames as f64 {
            let base = self.position.floor();
            let fraction = (self.position - base) as f32;
            let index = base as isize;

            for channel in 0..self.in_channels {
                let a = self.sample_at(input, frames, index, channel);
                let b = self.sample_at(input, frames, index + 1, channel);
                frame_in[channel] = a + (b - a) * fraction;
            }
            self.write_frame(&frame_in, out);
            self.position += self.step;
        }
        self.position -= frames as f64;

        let last = (frames - 1) * self.in_channels;
        self.previous
            .copy_from_slice(&input[last..last + self.in_channels]);
        self.have_previous = true;
    }

    /// Input sample lookup on a timeline where index -1 is the previous buffer's
    /// last frame. Indices past the end clamp to the last frame - the error is
    /// one interpolation step at each buffer boundary, which is inaudible and
    /// avoids having to hold a frame back.
    fn sample_at(&self, input: &[f32], frames: usize, index: isize, channel: usize) -> f32 {
        if index < 0 {
            return if self.have_previous {
                self.previous[channel]
            } else {
                input[channel]
            };
        }
        let clamped = (index as usize).min(frames - 1);
        input[clamped * self.in_channels + channel]
    }

    fn write_frame(&self, frame: &[f32], out: &mut Vec<f32>) {
        match (self.in_channels, self.out_channels) {
            (i, o) if i == o => out.extend_from_slice(frame),
            (1, o) => {
                // Mono source: send it to every output channel rather than only
                // the left one.
                for _ in 0..o {
                    out.push(frame[0]);
                }
            }
            (_, 1) => out.push((frame[0] + frame[1]) * 0.5),
            (i, o) => {
                // Anything else: copy what lines up, then repeat the last
                // channel to fill (e.g. 2 -> 4 duplicates right into the rears).
                for channel in 0..o {
                    out.push(frame[channel.min(i - 1)]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_when_formats_match() {
        let mut converter = Converter::new(2, 48_000, 2, 48_000);
        let mut out = Vec::new();
        converter.process(&[0.1, 0.2, 0.3, 0.4], &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn upsamples_to_the_expected_frame_count() {
        let mut converter = Converter::new(1, 22_050, 1, 44_100);
        let input: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let mut out = Vec::new();
        converter.process(&input, &mut out);
        // 100 input frames at a 0.5 step yields ~200 output frames.
        assert!((out.len() as i64 - 200).abs() <= 1, "got {}", out.len());
    }

    #[test]
    fn downsample_phase_is_continuous_across_buffers() {
        let mut converter = Converter::new(1, 48_000, 1, 24_000);
        let mut total = 0usize;
        for _ in 0..10 {
            let mut out = Vec::new();
            converter.process(&vec![0.5f32; 101], &mut out);
            total += out.len();
        }
        // 1010 input frames at a step of 2 -> ~505 output frames. Without the
        // carried fractional position this would drift to 510 (51 per buffer).
        assert!((total as i64 - 505).abs() <= 1, "got {total}");
    }

    #[test]
    fn mono_source_feeds_all_output_channels() {
        let mut converter = Converter::new(1, 48_000, 2, 48_000);
        let mut out = Vec::new();
        converter.process(&[0.7, 0.9], &mut out);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], out[1]);
    }

    #[test]
    fn stereo_downmixes_to_mono() {
        let mut converter = Converter::new(2, 48_000, 1, 48_000);
        let mut out = Vec::new();
        converter.process(&[1.0, 0.0], &mut out);
        assert_eq!(out, vec![0.5]);
    }

    #[test]
    fn sink_pads_underrun_with_silence_and_tracks_peak() {
        let sink = AudioSink::new(2);
        assert!(sink.push(&[0.5, -0.75]));
        let mut out = [9.0f32; 6];
        sink.fill(&mut out);
        assert_eq!(&out[..2], &[0.5, -0.75]);
        assert_eq!(&out[2..], &[0.0; 4]);
        assert!((sink.level() - 0.75).abs() < 1e-6);
    }

    #[test]
    fn mute_silences_output_and_releases_producer() {
        let sink = AudioSink::new(2);
        sink.push(&[1.0, 1.0]);
        sink.mute();
        let mut out = [1.0f32; 4];
        sink.fill(&mut out);
        assert_eq!(out, [0.0; 4]);
        assert_eq!(sink.level(), 0.0);
        assert!(!sink.push(&[1.0; 1_000_000]));
    }
}
