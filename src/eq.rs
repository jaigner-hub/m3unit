//! 10-band graphic equalizer: a cascade of peaking biquads per channel whose
//! gains can be changed live from the UI thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rodio::source::SeekError;
use rodio::{ChannelCount, Sample, SampleRate, Source};

pub const BANDS: usize = 10;
/// Standard ISO octave centres.
pub const FREQS: [f32; BANDS] = [31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];
pub const LABELS: [&str; BANDS] = ["31", "62", "125", "250", "500", "1k", "2k", "4k", "8k", "16k"];
pub const MAX_DB: f32 = 12.0;
/// Roughly one-octave bandwidth so neighbouring bands overlap smoothly.
const Q: f32 = 1.2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EqParams {
    pub enabled: bool,
    pub preamp_db: f32,
    pub gains_db: [f32; BANDS],
}

impl Default for EqParams {
    fn default() -> Self {
        Self { enabled: true, preamp_db: 0.0, gains_db: [0.0; BANDS] }
    }
}

impl EqParams {
    /// Magnitude response in dB at `freq` for a given sample rate (for the
    /// little response graph). Includes the preamp.
    pub fn response_db(&self, freq: f32, sample_rate: f32) -> f32 {
        if !self.enabled {
            return 0.0;
        }
        let w = 2.0 * std::f32::consts::PI * freq / sample_rate;
        let (cw, sw) = (w.cos(), w.sin());
        let (c2w, s2w) = ((2.0 * w).cos(), (2.0 * w).sin());
        let mut mag = 1.0f32;
        for (i, &g) in self.gains_db.iter().enumerate() {
            let Some(c) = Biquad::peaking(FREQS[i], g, Q, sample_rate) else { continue };
            // |H(e^jw)| = |b0 + b1 e^-jw + b2 e^-2jw| / |1 + a1 e^-jw + a2 e^-2jw|
            let nr = c.b0 + c.b1 * cw + c.b2 * c2w;
            let ni = -(c.b1 * sw + c.b2 * s2w);
            let dr = 1.0 + c.a1 * cw + c.a2 * c2w;
            let di = -(c.a1 * sw + c.a2 * s2w);
            mag *= ((nr * nr + ni * ni) / (dr * dr + di * di).max(1e-12)).sqrt();
        }
        20.0 * mag.max(1e-6).log10() + self.preamp_db
    }
}

pub struct Preset {
    pub name: &'static str,
    pub gains_db: [f32; BANDS],
}

pub const PRESETS: &[Preset] = &[
    Preset { name: "Flat", gains_db: [0.0; BANDS] },
    Preset { name: "Rock", gains_db: [5.0, 4.0, 3.0, 1.0, -1.0, -1.0, 1.0, 3.0, 4.0, 5.0] },
    Preset { name: "Pop", gains_db: [-1.0, 0.0, 2.0, 4.0, 4.0, 2.0, 0.0, -1.0, -1.0, -2.0] },
    Preset { name: "Live", gains_db: [-3.0, 0.0, 2.0, 3.0, 4.0, 4.0, 3.0, 2.0, 2.0, 1.0] },
    Preset { name: "Dance", gains_db: [6.0, 5.0, 2.0, 0.0, 0.0, -2.0, -3.0, -3.0, 0.0, 0.0] },
    Preset { name: "Classical", gains_db: [4.0, 3.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 3.0, 3.0] },
    Preset { name: "Jazz", gains_db: [3.0, 2.0, 1.0, 2.0, -1.0, -1.0, 0.0, 1.0, 2.0, 3.0] },
    Preset { name: "Bluegrass", gains_db: [2.0, 1.0, 0.0, 1.0, 2.0, 3.0, 3.0, 3.0, 2.0, 1.0] },
    Preset { name: "Acoustic", gains_db: [4.0, 3.0, 2.0, 1.0, 1.0, 1.0, 2.0, 3.0, 3.0, 2.0] },
    Preset { name: "Vocal", gains_db: [-3.0, -2.0, 0.0, 2.0, 4.0, 4.0, 3.0, 1.0, 0.0, -1.0] },
    Preset { name: "Bass Boost", gains_db: [7.0, 6.0, 5.0, 3.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0] },
    Preset { name: "Treble Boost", gains_db: [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 3.0, 5.0, 6.0, 7.0] },
    Preset { name: "Loudness", gains_db: [6.0, 4.0, 0.0, -1.0, -2.0, -1.0, 0.0, 2.0, 4.0, 5.0] },
];

/// Parameters shared between the UI and the audio thread. The version
/// counter lets the DSP notice changes with a single atomic load.
#[derive(Default)]
pub struct EqShared {
    params: Mutex<EqParams>,
    version: AtomicU64,
}

impl EqShared {
    pub fn get(&self) -> EqParams {
        *self.params.lock().unwrap()
    }

    pub fn set(&self, p: EqParams) {
        *self.params.lock().unwrap() = p;
        self.version.fetch_add(1, Ordering::Release);
    }
}

#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Biquad {
    /// RBJ peaking EQ. `None` when the centre frequency is too close to
    /// Nyquist to be meaningful (the band is then bypassed).
    fn peaking(f0: f32, gain_db: f32, q: f32, fs: f32) -> Option<Self> {
        if f0 >= fs * 0.47 {
            return None;
        }
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * f0 / fs;
        let alpha = w0.sin() / (2.0 * q);
        let cw = w0.cos();
        let a0 = 1.0 + alpha / a;
        Some(Self {
            b0: (1.0 + alpha * a) / a0,
            b1: (-2.0 * cw) / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: (-2.0 * cw) / a0,
            a2: (1.0 - alpha / a) / a0,
        })
    }
}

/// Per-channel, per-band filter memory (transposed direct form II).
#[derive(Clone, Copy, Default)]
struct State {
    s1: f32,
    s2: f32,
}

pub struct Equalized<S> {
    inner: S,
    shared: Arc<EqShared>,
    seen_version: u64,
    sample_rate: u32,
    active: bool,
    preamp: f32,
    coeffs: [Option<Biquad>; BANDS],
    state: Vec<[State; BANDS]>, // one entry per channel
    ch: usize,
    counter: u32,
}

impl<S: Source> Equalized<S> {
    pub fn new(inner: S, shared: Arc<EqShared>) -> Self {
        let mut eq = Self {
            inner,
            shared,
            seen_version: u64::MAX,
            sample_rate: 0,
            active: false,
            preamp: 1.0,
            coeffs: [None; BANDS],
            state: Vec::new(),
            ch: 0,
            counter: 0,
        };
        eq.refresh();
        eq
    }

    fn refresh(&mut self) {
        let version = self.shared.version.load(Ordering::Acquire);
        let sr = self.inner.sample_rate().get();
        let channels = self.inner.channels().get() as usize;
        if version == self.seen_version && sr == self.sample_rate && self.state.len() == channels {
            return;
        }
        let p = self.shared.get();
        if sr != self.sample_rate || self.state.len() != channels {
            self.state = vec![[State::default(); BANDS]; channels];
            self.ch = 0;
        }
        self.seen_version = version;
        self.sample_rate = sr;
        self.preamp = 10f32.powf(p.preamp_db / 20.0);
        let all_flat = p.gains_db.iter().all(|g| g.abs() < 0.01) && p.preamp_db.abs() < 0.01;
        self.active = p.enabled && !all_flat;
        for (i, c) in self.coeffs.iter_mut().enumerate() {
            *c = if p.gains_db[i].abs() < 0.01 {
                None
            } else {
                Biquad::peaking(FREQS[i], p.gains_db[i], Q, sr as f32)
            };
        }
    }
}

impl<S: Source> Iterator for Equalized<S> {
    type Item = Sample;

    #[inline]
    fn next(&mut self) -> Option<Sample> {
        let x = self.inner.next()?;
        if self.counter.is_multiple_of(256) {
            self.refresh();
        }
        self.counter = self.counter.wrapping_add(1);
        if !self.active || self.state.is_empty() {
            return Some(x);
        }
        let ch = self.ch;
        self.ch = (ch + 1) % self.state.len();
        let mut y = x * self.preamp;
        let states = &mut self.state[ch];
        for (b, c) in self.coeffs.iter().enumerate() {
            let Some(c) = c else { continue };
            let st = &mut states[b];
            let out = c.b0 * y + st.s1;
            st.s1 = c.b1 * y - c.a1 * out + st.s2;
            st.s2 = c.b2 * y - c.a2 * out;
            y = out;
        }
        Some(soft_clip(y))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

/// Transparent below 0.9 full scale, then a gentle knee so boosted bass
/// doesn't turn into hard digital clipping.
#[inline]
fn soft_clip(y: f32) -> f32 {
    const KNEE: f32 = 0.9;
    let a = y.abs();
    if a <= KNEE {
        y
    } else {
        let over = a - KNEE;
        let limit = 1.0 - KNEE;
        let shaped = KNEE + limit * (over / limit).tanh();
        shaped.copysign(y)
    }
}

impl<S: Source> Source for Equalized<S> {
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
        for s in &mut self.state {
            *s = [State::default(); BANDS];
        }
        self.ch = 0;
        self.inner.try_seek(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_follows_gain() {
        let mut p = EqParams::default();
        p.gains_db[5] = 6.0; // 1 kHz
        let at_1k = p.response_db(1000.0, 44_100.0);
        let at_60 = p.response_db(60.0, 44_100.0);
        assert!((at_1k - 6.0).abs() < 0.5, "1k: {at_1k}");
        assert!(at_60.abs() < 0.5, "60: {at_60}");
        p.enabled = false;
        assert_eq!(p.response_db(1000.0, 44_100.0), 0.0);
    }

    #[test]
    fn flat_eq_is_transparent() {
        let shared = Arc::new(EqShared::default());
        let src = rodio::buffer::SamplesBuffer::new(
            std::num::NonZero::new(2u16).unwrap(),
            std::num::NonZero::new(44_100u32).unwrap(),
            vec![0.25f32, -0.5, 0.75, 0.1],
        );
        let out: Vec<f32> = Equalized::new(src, shared).collect();
        assert_eq!(out, vec![0.25, -0.5, 0.75, 0.1]);
    }

    #[test]
    fn boost_raises_tone_level() {
        let shared = Arc::new(EqShared::default());
        let mut p = EqParams::default();
        p.gains_db[5] = 6.0;
        shared.set(p);
        let sr = 44_100.0;
        let samples: Vec<f32> = (0..44_100)
            .map(|i| 0.2 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin())
            .collect();
        let src = rodio::buffer::SamplesBuffer::new(
            std::num::NonZero::new(1u16).unwrap(),
            std::num::NonZero::new(44_100u32).unwrap(),
            samples,
        );
        let out: Vec<f32> = Equalized::new(src, shared).collect();
        let rms = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();
        let tail = &out[out.len() / 2..];
        let ratio_db = 20.0 * (rms(tail) / (0.2 / 2f32.sqrt())).log10();
        assert!((ratio_db - 6.0).abs() < 0.7, "boost was {ratio_db} dB");
    }

    #[test]
    fn presets_are_sane() {
        for p in PRESETS {
            assert!(p.gains_db.iter().all(|g| g.abs() <= MAX_DB), "{}", p.name);
        }
        assert_eq!(PRESETS[0].gains_db, [0.0; BANDS]);
    }
}
