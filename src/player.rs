//! Audio engine: opens the default output device, streams tracks over HTTP
//! with a read-ahead buffer, and decodes them with rodio/symphonia.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use rodio::decoder::DecoderBuilder;
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player, Source};
use stream_download::storage::temp::TempStorageProvider;
use stream_download::{Settings, StreamDownload};
use url::Url;

use crate::viz::{SampleTap, Tapped};

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

pub struct Engine {
    _device: MixerDeviceSink,
    player: Arc<Player>,
    rt: tokio::runtime::Handle,
    generation: Arc<AtomicU64>,
    status: Arc<Mutex<Status>>,
    info: Arc<Mutex<TrackInfo>>,
    pub tap: SampleTap,
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
            tap: SampleTap::default(),
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

    /// True once a playing track has run out of audio.
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

    /// Start streaming `url`; replaces whatever is playing.
    pub fn load(&self, url: Url) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.player.clear();
        self.tap.clear();
        *self.status.lock().unwrap() = Status::Loading;
        *self.info.lock().unwrap() = TrackInfo::default();

        let player = self.player.clone();
        let gen_counter = self.generation.clone();
        let status = self.status.clone();
        let info = self.info.clone();
        let tap = self.tap.clone();
        let hint = url
            .path_segments()
            .and_then(|mut s| s.next_back())
            .and_then(|f| f.rsplit_once('.'))
            .map(|(_, ext)| ext.to_ascii_lowercase());

        self.rt.spawn(async move {
            let is_current = || gen_counter.load(Ordering::SeqCst) == generation;
            let fail = |msg: String| {
                if is_current() {
                    *status.lock().unwrap() = Status::Stopped;
                    info.lock().unwrap().error = Some(msg);
                }
            };

            let settings = Settings::default().prefetch_bytes(192 * 1024);
            let reader = match StreamDownload::new_http(url, TempStorageProvider::default(), settings).await {
                Ok(r) => r,
                Err(e) => return fail(format!("stream failed: {e}")),
            };
            if !is_current() {
                return;
            }
            let byte_len = reader.content_length();

            // Building the decoder reads the container header, which blocks on
            // the network prefetch, so keep it off the async executor.
            let built = tokio::task::spawn_blocking(move || {
                let mut b = DecoderBuilder::new().with_data(reader).with_seekable(true).with_gapless(true);
                if let Some(len) = byte_len {
                    b = b.with_byte_len(len);
                }
                if let Some(h) = hint.as_deref() {
                    b = b.with_hint(h);
                }
                b.build()
            })
            .await;

            let decoder = match built {
                Ok(Ok(d)) => d,
                Ok(Err(e)) => return fail(format!("decode failed: {e}")),
                Err(e) => return fail(format!("decoder task failed: {e}")),
            };
            if !is_current() {
                return;
            }

            let duration = decoder.total_duration();
            let kbps = byte_len.zip(duration).and_then(|(len, dur)| {
                let secs = dur.as_secs_f64();
                (secs > 0.0).then(|| (len as f64 * 8.0 / secs / 1000.0).round() as u32)
            });
            *info.lock().unwrap() = TrackInfo {
                duration,
                sample_rate: decoder.sample_rate().get(),
                channels: decoder.channels().get(),
                kbps,
                error: None,
            };
            player.append(Tapped::new(decoder, tap));
            player.play();
            *status.lock().unwrap() = Status::Playing;
        });
    }
}
