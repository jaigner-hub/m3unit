//! Audio tap + spectrum analyser for the little bouncing bars.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rodio::source::SeekError;
use rodio::{ChannelCount, Sample, SampleRate, Source};

pub const BANDS: usize = 19;
pub const WINDOW: usize = 1024;
const KEEP: usize = WINDOW * 4;
const BATCH: usize = 256;

/// Shared ring buffer of mono samples, written by the audio decoder and read
/// by the UI thread.
#[derive(Clone, Default)]
pub struct SampleTap(Arc<Mutex<VecDeque<f32>>>);

impl SampleTap {
    fn push(&self, mono: &[f32]) {
        let mut buf = self.0.lock().unwrap();
        buf.extend(mono.iter().copied());
        let overflow = buf.len().saturating_sub(KEEP);
        if overflow > 0 {
            buf.drain(..overflow);
        }
    }

    /// Most recent `n` samples (fewer if not enough are buffered yet).
    pub fn latest(&self, n: usize) -> Vec<f32> {
        let buf = self.0.lock().unwrap();
        let start = buf.len().saturating_sub(n);
        buf.range(start..).copied().collect()
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// A `Source` wrapper that copies a mono downmix of everything it yields into
/// a [`SampleTap`].
pub struct Tapped<S> {
    inner: S,
    tap: SampleTap,
    acc: f32,
    ch: u16,
    batch: Vec<f32>,
}

impl<S: Source> Tapped<S> {
    pub fn new(inner: S, tap: SampleTap) -> Self {
        Self { inner, tap, acc: 0.0, ch: 0, batch: Vec::with_capacity(BATCH) }
    }
}

impl<S: Source> Iterator for Tapped<S> {
    type Item = Sample;

    #[inline]
    fn next(&mut self) -> Option<Sample> {
        let s = self.inner.next()?;
        let channels = self.inner.channels().get();
        self.acc += s;
        self.ch += 1;
        if self.ch >= channels {
            self.batch.push(self.acc / channels as f32);
            self.acc = 0.0;
            self.ch = 0;
            if self.batch.len() >= BATCH {
                self.tap.push(&self.batch);
                self.batch.clear();
            }
        }
        Some(s)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: Source> Source for Tapped<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }
    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }
    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.acc = 0.0;
        self.ch = 0;
        self.batch.clear();
        self.inner.try_seek(pos)
    }
}

/// Smoothed, log-spaced band levels in 0..=1 with slowly falling peak caps.
pub struct Spectrum {
    pub bars: [f32; BANDS],
    pub peaks: [f32; BANDS],
    hold: [f32; BANDS],
    window: Vec<f32>,
}

impl Default for Spectrum {
    fn default() -> Self {
        let window = (0..WINDOW)
            .map(|i| {
                let x = i as f32 / (WINDOW - 1) as f32;
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * x).cos()
            })
            .collect();
        Self { bars: [0.0; BANDS], peaks: [0.0; BANDS], hold: [0.0; BANDS], window }
    }
}

impl Spectrum {
    const BAR_FALL: f32 = 2.2; // units per second
    const PEAK_HOLD: f32 = 0.35; // seconds
    const PEAK_FALL: f32 = 0.9;

    /// Let every bar and peak sink toward zero (used while paused/stopped).
    pub fn decay(&mut self, dt: f32) {
        self.apply([0.0; BANDS], dt);
    }

    pub fn update(&mut self, samples: &[f32], sample_rate: f32, dt: f32) {
        if samples.len() < WINDOW {
            self.decay(dt);
            return;
        }
        let tail = &samples[samples.len() - WINDOW..];
        let mut re: Vec<f32> = tail.iter().zip(&self.window).map(|(s, w)| s * w).collect();
        let mut im = vec![0.0f32; WINDOW];
        fft(&mut re, &mut im);

        // Amplitude normalised so a full-scale sine lands near 1.0 (Hann gain 0.5).
        let norm = 4.0 / WINDOW as f32;
        let nyquist = sample_rate / 2.0;
        let f_lo = 45.0f32;
        let f_hi = nyquist.min(16_000.0);
        let mut levels = [0.0f32; BANDS];
        let mut lo_bin = ((f_lo / nyquist) * (WINDOW / 2) as f32).floor().max(1.0) as usize;
        for (b, level) in levels.iter_mut().enumerate() {
            let t = (b + 1) as f32 / BANDS as f32;
            let f_edge = f_lo * (f_hi / f_lo).powf(t);
            let hi_bin = (((f_edge / nyquist) * (WINDOW / 2) as f32).floor() as usize).max(lo_bin + 1);
            let hi_bin = hi_bin.min(WINDOW / 2);
            let mut energy = 0.0f32;
            for k in lo_bin..hi_bin {
                let a = (re[k] * re[k] + im[k] * im[k]).sqrt() * norm;
                energy += a * a;
            }
            let amp = energy.sqrt();
            let db = 20.0 * (amp + 1e-9).log10();
            // Music rolls off with frequency; tilt the high bands up a touch.
            let db = db + b as f32 * 1.1;
            *level = ((db + 52.0) / 50.0).clamp(0.0, 1.0);
            lo_bin = hi_bin;
        }
        self.apply(levels, dt);
    }

    fn apply(&mut self, levels: [f32; BANDS], dt: f32) {
        for (b, &target) in levels.iter().enumerate() {
            self.bars[b] = if target >= self.bars[b] {
                target
            } else {
                (self.bars[b] - Self::BAR_FALL * dt).max(target)
            };
            if self.bars[b] >= self.peaks[b] {
                self.peaks[b] = self.bars[b];
                self.hold[b] = Self::PEAK_HOLD;
            } else {
                self.hold[b] -= dt;
                if self.hold[b] < 0.0 {
                    self.peaks[b] = (self.peaks[b] - Self::PEAK_FALL * dt).max(0.0);
                }
            }
        }
    }
}

/// In-place iterative radix-2 FFT. `re.len()` must be a power of two.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two());
    // bit-reversal permutation
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f32::consts::PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (ar, ai) = (re[i + k], im[i + k]);
                let (br, bi) = (re[i + k + len / 2], im[i + k + len / 2]);
                let (tr, ti) = (br * cr - bi * ci, br * ci + bi * cr);
                re[i + k] = ar + tr;
                im[i + k] = ai + ti;
                re[i + k + len / 2] = ar - tr;
                im[i + k + len / 2] = ai - ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_finds_a_sine() {
        let n = 1024;
        let k = 37;
        let mut re: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * k as f32 * i as f32 / n as f32).sin())
            .collect();
        let mut im = vec![0.0; n];
        fft(&mut re, &mut im);
        let mags: Vec<f32> = (0..n / 2).map(|i| (re[i] * re[i] + im[i] * im[i]).sqrt()).collect();
        let peak = mags.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        assert_eq!(peak, k);
        assert!((mags[k] - n as f32 / 2.0).abs() < 1.0);
    }

    #[test]
    fn spectrum_reacts_to_tone() {
        let mut spec = Spectrum::default();
        let sr = 44_100.0;
        let samples: Vec<f32> = (0..WINDOW)
            .map(|i| 0.8 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin())
            .collect();
        spec.update(&samples, sr, 0.016);
        let max = spec.bars.iter().cloned().fold(0.0f32, f32::max);
        assert!(max > 0.5, "bars = {:?}", spec.bars);
    }
}
