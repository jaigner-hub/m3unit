//! Audio engine: opens the default output device, streams tracks over HTTP
//! with a read-ahead buffer, and decodes them with rodio/symphonia.
//!
//! Gapless playback: while a track plays, the next one is opened and decoded
//! in the background and appended to the same rodio queue, so the switch
//! happens sample-accurately inside the audio thread with no fetch pause.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use rodio::decoder::{Decoder, DecoderBuilder};
use rodio::source::SeekError;
use rodio::{ChannelCount, DeviceSinkBuilder, MixerDeviceSink, Player, Sample, SampleRate, Source};
use stream_download::storage::temp::TempStorageProvider;
use stream_download::{Settings, StreamDownload};
use url::Url;

use crate::eq::{EqShared, Equalized};
use crate::viz::{SampleTap, Tapped};

type Reader = StreamDownload<TempStorageProvider>;

/// Bytes to fetch before the decoder is allowed to start reading.
const PREFETCH_BYTES: u64 = 192 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Stopped,
    Loading,
    Playing,
    Paused,
}

#[derive(Clone, Debug, Default)]
pub struct TrackInfo {
    pub duration: Option<Duration>,
    pub sample_rate: u32,
    pub channels: u16,
    pub kbps: Option<u32>,
    pub error: Option<String>,
}

/// Bookkeeping for a track that has been (or is being) queued behind the
/// current one.
struct Queued {
    token: u64,
    cancel: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
    info: Arc<Mutex<TrackInfo>>,
}

pub struct Engine {
    _device: MixerDeviceSink,
    player: Arc<Player>,
    rt: tokio::runtime::Handle,
    generation: Arc<AtomicU64>,
    status: Arc<Mutex<Status>>,
    info: Arc<Mutex<TrackInfo>>,
    queued: Arc<Mutex<Option<Queued>>>,
    pub tap: SampleTap,
    pub eq: Arc<EqShared>,
}

impl Engine {
    pub fn new(rt: tokio::runtime::Handle) -> anyhow::Result<Self> {
        let mut device = DeviceSinkBuilder::open_default_sink().context("no audio output device")?;
        device.log_on_drop(false);
        let player = Player::connect_new(device.mixer());
        Ok(Self {
            _device: device,
            player: Arc::new(player),
            rt,
            generation: Arc::new(AtomicU64::new(0)),
            status: Arc::new(Mutex::new(Status::Stopped)),
            info: Arc::new(Mutex::new(TrackInfo::default())),
            queued: Arc::new(Mutex::new(None)),
            tap: SampleTap::default(),
            eq: Arc::new(EqShared::default()),
        })
    }

    pub fn status(&self) -> Status {
        *self.status.lock().unwrap()
    }

    pub fn info(&self) -> TrackInfo {
        self.info.lock().unwrap().clone()
    }

    /// Take (and clear) the last error message, if any.
    pub fn take_error(&self) -> Option<String> {
        self.info.lock().unwrap().error.take()
    }

    pub fn position(&self) -> Duration {
        self.player.get_pos()
    }

    /// True once a playing track has run out of audio and nothing was queued
    /// behind it.
    pub fn finished(&self) -> bool {
        self.status() == Status::Playing && self.player.empty()
    }

    pub fn set_volume(&self, v: f32) {
        self.player.set_volume(v.clamp(0.0, 1.0));
    }

    pub fn toggle_pause(&self) {
        let mut status = self.status.lock().unwrap();
        match *status {
            Status::Playing => {
                self.player.pause();
                *status = Status::Paused;
            }
            Status::Paused => {
                self.player.play();
                *status = Status::Playing;
            }
            _ => {}
        }
    }

    pub fn resume(&self) {
        if self.status() == Status::Paused {
            self.toggle_pause();
        }
    }

    pub fn stop(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.cancel_queued();
        self.player.clear();
        *self.status.lock().unwrap() = Status::Stopped;
        *self.info.lock().unwrap() = TrackInfo::default();
        self.tap.clear();
    }

    pub fn seek(&self, pos: Duration) {
        if matches!(self.status(), Status::Playing | Status::Paused) {
            let _ = self.player.try_seek(pos);
        }
    }

    /// Start streaming `url` now; replaces whatever is playing or queued.
    pub fn load(&self, url: Url) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.cancel_queued();
        self.player.clear();
        self.tap.clear();
        *self.status.lock().unwrap() = Status::Loading;
        *self.info.lock().unwrap() = TrackInfo::default();

        let player = self.player.clone();
        let gen_counter = self.generation.clone();
        let status = self.status.clone();
        let info = self.info.clone();
        let tap = self.tap.clone();
        let eq = self.eq.clone();

        self.rt.spawn(async move {
            let is_current = || gen_counter.load(Ordering::SeqCst) == generation;
            let (decoder, track_info) = match open(url).await {
                Ok(x) => x,
                Err(e) => {
                    if is_current() {
                        *status.lock().unwrap() = Status::Stopped;
                        info.lock().unwrap().error = Some(e);
                    }
                    return;
                }
            };
            if !is_current() {
                return;
            }
            *info.lock().unwrap() = track_info;
            player.append(Tapped::new(Equalized::new(decoder, eq), tap));
            player.play();
            *status.lock().unwrap() = Status::Playing;
        });
    }

    /// Token of the track queued to play next, if any.
    pub fn queued_token(&self) -> Option<u64> {
        self.queued.lock().unwrap().as_ref().map(|q| q.token)
    }

    /// Drop the queued track. If it was already sitting in the rodio queue it
    /// will yield no samples when its turn comes, so it costs nothing.
    pub fn cancel_queued(&self) {
        if let Some(q) = self.queued.lock().unwrap().take() {
            q.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// If the queued track has begun playing, promote it to "current" and
    /// return its token so the UI can follow along.
    pub fn poll_switch(&self) -> Option<u64> {
        let mut slot = self.queued.lock().unwrap();
        if !slot.as_ref().is_some_and(|q| q.started.load(Ordering::SeqCst)) {
            return None;
        }
        let q = slot.take().unwrap();
        *self.info.lock().unwrap() = q.info.lock().unwrap().clone();
        Some(q.token)
    }

    /// Pre-open `url` and append it behind the current track. `token` is
    /// whatever the caller needs to recognise it later (the playlist index).
    pub fn queue_next(&self, url: Url, token: u64) {
        if !matches!(self.status(), Status::Playing | Status::Paused) {
            return;
        }
        self.cancel_queued();
        let generation = self.generation.load(Ordering::SeqCst);
        let cancel = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let q_info = Arc::new(Mutex::new(TrackInfo::default()));
        *self.queued.lock().unwrap() = Some(Queued {
            token,
            cancel: cancel.clone(),
            started: started.clone(),
            info: q_info.clone(),
        });

        let player = self.player.clone();
        let gen_counter = self.generation.clone();
        let tap = self.tap.clone();
        let eq = self.eq.clone();
        let info = self.info.clone();
        let queued = self.queued.clone();

        self.rt.spawn(async move {
            let still_wanted = || gen_counter.load(Ordering::SeqCst) == generation && !cancel.load(Ordering::SeqCst);
            let (decoder, track_info) = match open(url).await {
                Ok(x) => x,
                Err(e) => {
                    if still_wanted() {
                        // Forget it; the UI will fall back to a normal load
                        // when the current track ends.
                        info.lock().unwrap().error = Some(format!("prefetch: {e}"));
                        let mut slot = queued.lock().unwrap();
                        if slot.as_ref().is_some_and(|q| q.token == token) {
                            *slot = None;
                        }
                    }
                    return;
                }
            };
            if !still_wanted() {
                return;
            }
            *q_info.lock().unwrap() = track_info;
            let chain = Tapped::new(Equalized::new(decoder, eq), tap);
            player.append(QueuedSource { inner: chain, cancel, started });
        });
    }
}

/// Open an HTTP stream and build a decoder over it. Runs the blocking header
/// parse on the blocking pool because it waits on the network prefetch.
async fn open(url: Url) -> Result<(Decoder<Reader>, TrackInfo), String> {
    let hint = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .and_then(|f| f.rsplit_once('.'))
        .map(|(_, ext)| ext.to_ascii_lowercase());

    let settings = Settings::default().prefetch_bytes(PREFETCH_BYTES);
    let reader = StreamDownload::new_http(url, TempStorageProvider::default(), settings)
        .await
        .map_err(|e| format!("stream failed: {e}"))?;
    let byte_len = reader.content_length();

    let decoder = tokio::task::spawn_blocking(move || {
        let mut b = DecoderBuilder::new().with_data(reader).with_seekable(true).with_gapless(true);
        if let Some(len) = byte_len {
            b = b.with_byte_len(len);
        }
        if let Some(h) = hint.as_deref() {
            b = b.with_hint(h);
        }
        b.build()
    })
    .await
    .map_err(|e| format!("decoder task failed: {e}"))?
    .map_err(|e| format!("decode failed: {e}"))?;

    let duration = decoder.total_duration();
    let kbps = byte_len.zip(duration).and_then(|(len, dur)| {
        let secs = dur.as_secs_f64();
        (secs > 0.0).then(|| (len as f64 * 8.0 / secs / 1000.0).round() as u32)
    });
    let info = TrackInfo {
        duration,
        sample_rate: decoder.sample_rate().get(),
        channels: decoder.channels().get(),
        kbps,
        error: None,
    };
    Ok((decoder, info))
}

/// Wraps a queued track so it can be cancelled before it starts and so the
/// UI can tell the moment it takes over.
struct QueuedSource<S> {
    inner: S,
    cancel: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
}

impl<S: Source> Iterator for QueuedSource<S> {
    type Item = Sample;

    #[inline]
    fn next(&mut self) -> Option<Sample> {
        if !self.started.load(Ordering::Relaxed) {
            if self.cancel.load(Ordering::SeqCst) {
                return None;
            }
            self.started.store(true, Ordering::SeqCst);
        }
        self.inner.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: Source> Source for QueuedSource<S> {
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
        self.inner.try_seek(pos)
    }
}
