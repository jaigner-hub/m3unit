//! The egui front end. Everything is drawn by hand with the painter so it
//! reads like a late-90s skinned player rather than a stock toolkit window.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Key, Pos2, Rect, Response, RichText, Sense, Shape,
    Stroke, StrokeKind, Ui, pos2, vec2,
};
use url::Url;

use crate::eq::{self, EqParams, MAX_DB, PRESETS};
use crate::player::{Engine, Status};
use crate::playlist::{Playlist, fetch, fmt_time};
use crate::viz::{BANDS, Spectrum, WINDOW};

pub const WIN_W: f32 = 560.0;
pub const WIN_H: f32 = 740.0;
/// Compact height: everything but the EQ and playlist panels.
pub const MIN_H: f32 = 216.0;

const DEFAULT_URL: &str =
    "https://archive.org/download/BillyStrings2026-09-26/BillyStrings2026-09-26_vbr.m3u";

// Palette: original, loosely in the spirit of a dark-blue classic skin.
mod pal {
    use egui::Color32;
    pub const BG: Color32 = Color32::from_rgb(28, 30, 44);
    pub const BG_DEEP: Color32 = Color32::from_rgb(18, 19, 30);
    pub const FACE: Color32 = Color32::from_rgb(58, 62, 84);
    pub const FACE_HI: Color32 = Color32::from_rgb(78, 84, 112);
    pub const LIGHT: Color32 = Color32::from_rgb(122, 130, 168);
    pub const DARK: Color32 = Color32::from_rgb(8, 8, 14);
    pub const LCD: Color32 = Color32::from_rgb(0, 0, 0);
    pub const GREEN: Color32 = Color32::from_rgb(0, 255, 64);
    pub const GREEN_DIM: Color32 = Color32::from_rgb(0, 96, 24);
    pub const AMBER: Color32 = Color32::from_rgb(255, 200, 60);
    pub const SEL: Color32 = Color32::from_rgb(0, 0, 198);
    pub const WHITE: Color32 = Color32::from_rgb(240, 240, 255);
    pub const TITLE: Color32 = Color32::from_rgb(200, 206, 236);
    pub const RED: Color32 = Color32::from_rgb(255, 70, 70);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Repeat {
    Off,
    All,
    One,
}

pub struct App {
    rt: tokio::runtime::Runtime,
    client: reqwest::Client,
    engine: Option<Engine>,
    engine_error: Option<String>,

    url_input: String,
    playlist: Playlist,
    playlist_rx: Option<mpsc::Receiver<anyhow::Result<Playlist>>>,

    current: Option<usize>,
    selected: Option<usize>,
    /// Track index we intend to play after the current one (queued for
    /// gapless playback). Reset whenever the answer might change.
    planned_next: Option<usize>,
    scroll_to_current: bool,
    shuffle: bool,
    repeat: Repeat,
    volume: f32,

    eq_visible: bool,
    playlist_visible: bool,
    /// Window height to restore when the playlist is shown again.
    full_height: f32,
    eq: EqParams,
    eq_preset: Option<usize>,

    spectrum: Spectrum,
    started: Instant,
    last_frame: Instant,
    seek_preview: Option<f32>,
    status_msg: String,
    rng: u64,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Runtime) -> Self {
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        cc.egui_ctx.all_styles_mut(|style| {
            style.visuals = egui::Visuals::dark();
            style.visuals.extreme_bg_color = pal::LCD;
            style.visuals.selection.bg_fill = pal::SEL;
            style.visuals.selection.stroke = Stroke::new(1.0, pal::GREEN);
            style.spacing.item_spacing = vec2(4.0, 4.0);
        });

        let client = reqwest::Client::builder()
            .user_agent(concat!("m3unit/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("http client");

        let (engine, engine_error) = match Engine::new(rt.handle().clone()) {
            Ok(e) => (Some(e), None),
            Err(e) => (None, Some(format!("audio unavailable: {e:#}"))),
        };
        if let Some(e) = &engine {
            e.set_volume(0.8);
        }

        let seed = Instant::now().elapsed().as_nanos() as u64
            ^ std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E37_79B9_7F4A_7C15);

        let mut app = Self {
            rt,
            client,
            engine,
            engine_error,
            url_input: DEFAULT_URL.to_string(),
            playlist: Playlist::default(),
            playlist_rx: None,
            current: None,
            selected: None,
            planned_next: None,
            scroll_to_current: false,
            shuffle: false,
            repeat: Repeat::Off,
            volume: 0.8,
            eq_visible: true,
            playlist_visible: true,
            full_height: WIN_H,
            eq: EqParams::default(),
            eq_preset: Some(0),
            spectrum: Spectrum::default(),
            started: Instant::now(),
            last_frame: Instant::now(),
            seek_preview: None,
            status_msg: String::new(),
            rng: seed | 1,
        };
        app.load_playlist();
        app
    }

    // ---------------------------------------------------------------- state

    fn load_playlist(&mut self) {
        let raw = self.url_input.trim().to_string();
        let url = match Url::parse(&raw) {
            Ok(u) if matches!(u.scheme(), "http" | "https") => u,
            _ => {
                self.status_msg = "enter an http(s) URL to an .m3u file".into();
                return;
            }
        };
        let (tx, rx) = mpsc::channel();
        let client = self.client.clone();
        self.rt.spawn(async move {
            let _ = tx.send(fetch(&client, url).await);
        });
        self.playlist_rx = Some(rx);
        self.status_msg = "loading playlist...".into();
    }

    fn poll_playlist(&mut self) {
        let Some(rx) = &self.playlist_rx else { return };
        match rx.try_recv() {
            Ok(Ok(pl)) => {
                self.stop();
                self.status_msg = format!("loaded {} tracks", pl.tracks.len());
                self.playlist = pl;
                self.current = None;
                self.selected = None;
                self.planned_next = None;
                self.playlist_rx = None;
            }
            Ok(Err(e)) => {
                self.status_msg = format!("playlist error: {e:#}");
                self.playlist_rx = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.status_msg = "playlist load aborted".into();
                self.playlist_rx = None;
            }
        }
    }

    fn play_index(&mut self, idx: usize) {
        let Some(track) = self.playlist.tracks.get(idx) else { return };
        let Some(engine) = &self.engine else {
            self.status_msg = self.engine_error.clone().unwrap_or_default();
            return;
        };
        engine.load(track.url.clone());
        self.current = Some(idx);
        self.selected = Some(idx);
        self.planned_next = None;
        self.scroll_to_current = true;
        self.spectrum = Spectrum::default();
        self.status_msg = format!("buffering {}...", track.title);
    }

    fn play(&mut self) {
        match self.engine.as_ref().map(|e| e.status()) {
            Some(Status::Paused) => self.engine.as_ref().unwrap().resume(),
            Some(Status::Playing) | Some(Status::Loading) => {
                if let Some(cur) = self.current {
                    self.play_index(cur);
                }
            }
            _ => {
                let idx = self.selected.or(self.current).unwrap_or(0);
                self.play_index(idx);
            }
        }
    }

    fn stop(&mut self) {
        if let Some(e) = &self.engine {
            e.stop();
        }
        self.planned_next = None;
        self.spectrum = Spectrum::default();
    }

    /// Height of everything above the playlist panel.
    fn fixed_height(&self) -> f32 {
        let base = 16.0 + 118.0 + 18.0 + 34.0 + 30.0;
        base + if self.eq_visible { 16.0 + 120.0 } else { 0.0 }
    }

    /// Resize the window to fit: compact when the playlist is hidden, the
    /// remembered full height when it is shown.
    fn fit_window(&self, ctx: &egui::Context) {
        let width = ctx.viewport_rect().width().max(WIN_W);
        let height = if self.playlist_visible {
            self.full_height.max(self.fixed_height() + 120.0)
        } else {
            self.fixed_height()
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(vec2(width, height)));
    }

    /// Keep the engine's queued track in line with what should play next.
    fn maintain_queue(&mut self) {
        let active = self
            .engine
            .as_ref()
            .is_some_and(|e| matches!(e.status(), Status::Playing | Status::Paused));
        if !active || self.current.is_none() {
            return;
        }
        if self.planned_next.is_none() {
            self.planned_next = self.next_index(false);
        }
        let want = self.planned_next;
        let Some(engine) = &self.engine else { return };
        if engine.queued_token() == want.map(|i| i as u64) {
            return;
        }
        engine.cancel_queued();
        if let Some(i) = want
            && let Some(t) = self.playlist.tracks.get(i)
        {
            engine.queue_next(t.url.clone(), i as u64);
        }
    }

    /// The queued track has taken over inside the audio thread.
    fn on_switched(&mut self, idx: usize) {
        self.current = Some(idx);
        self.selected = Some(idx);
        self.planned_next = None;
        self.scroll_to_current = true;
        if let Some(t) = self.playlist.tracks.get(idx) {
            self.status_msg = format!("now playing {}", t.title);
        }
    }

    fn next_index(&mut self, manual: bool) -> Option<usize> {
        let n = self.playlist.tracks.len();
        if n == 0 {
            return None;
        }
        let cur = self.current?;
        if !manual && self.repeat == Repeat::One {
            return Some(cur);
        }
        if self.shuffle && n > 1 {
            let mut pick = (self.rand() % n as u64) as usize;
            if pick == cur {
                pick = (pick + 1) % n;
            }
            return Some(pick);
        }
        if cur + 1 < n {
            Some(cur + 1)
        } else if self.repeat == Repeat::All || manual {
            Some(0)
        } else {
            None
        }
    }

    fn next(&mut self, manual: bool) {
        match self.next_index(manual) {
            Some(i) => self.play_index(i),
            None => {
                self.stop();
                self.status_msg = "end of playlist".into();
            }
        }
    }

    fn prev(&mut self) {
        let n = self.playlist.tracks.len();
        if n == 0 {
            return;
        }
        // Like most players: restart the track if we're well into it.
        if let (Some(e), Some(cur)) = (&self.engine, self.current)
            && e.position() > Duration::from_secs(3)
        {
            self.play_index(cur);
            return;
        }
        let idx = match self.current {
            Some(0) | None => n - 1,
            Some(c) => c - 1,
        };
        self.play_index(idx);
    }

    fn rand(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn tick(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;

        self.poll_playlist();

        let mut finished = false;
        let mut err = None;
        let mut playing = false;
        let mut switched = None;
        let mut sample_rate = 44_100.0;
        if let Some(e) = &self.engine {
            switched = e.poll_switch();
            finished = e.finished();
            err = e.take_error();
            playing = e.status() == Status::Playing;
            let sr = e.info().sample_rate;
            if sr > 0 {
                sample_rate = sr as f32;
            }
        }
        if let Some(err) = err {
            self.status_msg = err;
        }
        if let Some(idx) = switched {
            self.on_switched(idx as usize);
        }
        if finished {
            self.next(false);
        }
        self.maintain_queue();

        if playing {
            let samples = self.engine.as_ref().unwrap().tap.latest(WINDOW);
            self.spectrum.update(&samples, sample_rate, dt);
        } else {
            self.spectrum.decay(dt);
        }

        self.handle_keys(ctx);

        let interval = if playing { 33 } else { 100 };
        ctx.request_repaint_after(Duration::from_millis(interval));
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.memory(|m| m.focused().is_some()) {
            return; // typing in the URL box
        }
        let (space, z, x, c, v, b, left, right, up, down) = ctx.input(|i| {
            (
                i.key_pressed(Key::Space),
                i.key_pressed(Key::Z),
                i.key_pressed(Key::X),
                i.key_pressed(Key::C),
                i.key_pressed(Key::V),
                i.key_pressed(Key::B),
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
            )
        });
        if z {
            self.prev();
        }
        if x {
            self.play();
        }
        if c || space {
            match self.engine.as_ref().map(|e| e.status()) {
                Some(Status::Playing) | Some(Status::Paused) => {
                    self.engine.as_ref().unwrap().toggle_pause()
                }
                _ => self.play(),
            }
        }
        if v {
            self.stop();
        }
        if b {
            self.next(true);
        }
        if left || right {
            self.seek_relative(if left { -5.0 } else { 5.0 });
        }
        if up || down {
            self.volume = (self.volume + if up { 0.05 } else { -0.05 }).clamp(0.0, 1.0);
            if let Some(e) = &self.engine {
                e.set_volume(self.volume);
            }
        }
    }

    fn seek_relative(&mut self, secs: f32) {
        let Some(e) = &self.engine else { return };
        let pos = e.position().as_secs_f32() + secs;
        let max = e.info().duration.map(|d| d.as_secs_f32()).unwrap_or(f32::MAX);
        e.seek(Duration::from_secs_f32(pos.clamp(0.0, max)));
    }

    // ------------------------------------------------------------- drawing

    fn draw_title_bar(&self, ui: &mut Ui, label: &str) {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 16.0), Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, 0.0, pal::BG_DEEP);
        let galley = p.layout_no_wrap(label.to_string(), FontId::monospace(10.0), pal::TITLE);
        let text_w = galley.size().x + 16.0;
        let cx = rect.center().x;
        // hatched grip lines on both sides of the label
        for side in [-1.0f32, 1.0] {
            let (x0, x1) = if side < 0.0 {
                (rect.left() + 6.0, cx - text_w / 2.0)
            } else {
                (cx + text_w / 2.0, rect.right() - 6.0)
            };
            if x1 > x0 {
                for k in 0..3 {
                    let y = rect.top() + 4.0 + k as f32 * 3.0;
                    p.line_segment([pos2(x0, y), pos2(x1, y)], Stroke::new(1.0, pal::LIGHT));
                    p.line_segment([pos2(x0, y + 1.0), pos2(x1, y + 1.0)], Stroke::new(1.0, pal::DARK));
                }
            }
        }
        p.galley(pos2(cx - galley.size().x / 2.0, rect.center().y - galley.size().y / 2.0), galley, pal::TITLE);
    }

    fn draw_display(&mut self, ui: &mut Ui) {
        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(w, 118.0), Sense::hover());
        let p = ui.painter().clone();
        p.rect_filled(rect, 0.0, pal::BG);
        let lcd = Rect::from_min_max(rect.min + vec2(10.0, 6.0), rect.max - vec2(10.0, 6.0));
        inset(&p, lcd, pal::LCD);

        let (status, info, pos) = match &self.engine {
            Some(e) => (e.status(), e.info(), e.position()),
            None => (Status::Stopped, Default::default(), Duration::ZERO),
        };
        let blink = (self.started.elapsed().as_millis() / 500).is_multiple_of(2);

        // --- status glyph + time -------------------------------------------
        let glyph_rect = Rect::from_min_size(lcd.min + vec2(10.0, 14.0), vec2(14.0, 14.0));
        match status {
            Status::Playing => draw_play_glyph(&p, glyph_rect, pal::GREEN),
            Status::Paused => {
                if blink {
                    draw_pause_glyph(&p, glyph_rect, pal::GREEN)
                }
            }
            Status::Loading => {
                if blink {
                    p.circle_filled(glyph_rect.center(), 5.0, pal::AMBER);
                }
            }
            Status::Stopped => draw_stop_glyph(&p, glyph_rect, pal::GREEN_DIM),
        }
        let shown_secs = match (self.seek_preview, info.duration) {
            (Some(f), Some(d)) => (d.as_secs_f32() * f) as u32,
            _ => pos.as_secs() as u32,
        };
        let time_text = if status == Status::Stopped {
            "--:--".to_string()
        } else if status == Status::Paused && !blink {
            String::new()
        } else {
            fmt_time(shown_secs)
        };
        p.text(
            lcd.min + vec2(34.0, 4.0),
            Align2::LEFT_TOP,
            time_text,
            FontId::monospace(38.0),
            pal::GREEN,
        );

        // --- spectrum -------------------------------------------------------
        let viz = Rect::from_min_size(lcd.min + vec2(12.0, 60.0), vec2(150.0, 44.0));
        draw_spectrum(&p, viz, &self.spectrum);

        // --- ticker ---------------------------------------------------------
        let tick = Rect::from_min_size(lcd.min + vec2(180.0, 8.0), vec2(lcd.width() - 190.0, 16.0));
        let text = match (self.current, self.playlist.tracks.get(self.current.unwrap_or(usize::MAX))) {
            (Some(i), Some(t)) => {
                let dur = t.duration_secs.or(info.duration.map(|d| d.as_secs() as u32));
                let mut s = format!("{}. {}", i + 1, t.title);
                if let Some(a) = &self.playlist.artist {
                    s = format!("{}. {} - {}", i + 1, a, t.title);
                }
                if let Some(d) = dur {
                    s.push_str(&format!(" ({})", fmt_time(d)));
                }
                s
            }
            _ if !self.playlist.tracks.is_empty() => {
                format!("{}  -  {} tracks", self.playlist.name, self.playlist.tracks.len())
            }
            _ => "M3UNIT  -  paste an .m3u URL below and press GO".to_string(),
        };
        draw_ticker(&p, tick, &text, self.started.elapsed().as_secs_f32());

        // --- little readouts ----------------------------------------------
        let kbps = info.kbps.map(|k| k.to_string()).unwrap_or_else(|| "---".into());
        let khz = if info.sample_rate > 0 {
            format!("{}", (info.sample_rate as f32 / 1000.0).round() as u32)
        } else {
            "--".into()
        };
        let small = FontId::monospace(10.0);
        let y = lcd.min.y + 36.0;
        readout(&p, pos2(lcd.min.x + 182.0, y), 40.0, &kbps, &small);
        p.text(pos2(lcd.min.x + 226.0, y + 2.0), Align2::LEFT_TOP, "kbps", small.clone(), pal::GREEN_DIM);
        readout(&p, pos2(lcd.min.x + 262.0, y), 24.0, &khz, &small);
        p.text(pos2(lcd.min.x + 290.0, y + 2.0), Align2::LEFT_TOP, "kHz", small.clone(), pal::GREEN_DIM);
        let (mono_c, stereo_c) = match info.channels {
            0 => (pal::GREEN_DIM, pal::GREEN_DIM),
            1 => (pal::GREEN, pal::GREEN_DIM),
            _ => (pal::GREEN_DIM, pal::GREEN),
        };
        p.text(pos2(lcd.max.x - 96.0, y + 2.0), Align2::LEFT_TOP, "mono", small.clone(), mono_c);
        p.text(pos2(lcd.max.x - 56.0, y + 2.0), Align2::LEFT_TOP, "stereo", small.clone(), stereo_c);

        // --- playlist name / status -----------------------------------------
        let sub = if self.status_msg.is_empty() {
            self.playlist.name.clone()
        } else {
            self.status_msg.clone()
        };
        let sub_rect = Rect::from_min_size(lcd.min + vec2(180.0, 62.0), vec2(lcd.width() - 190.0, 40.0));
        let color = if self.status_msg.contains("error") || self.status_msg.contains("failed") {
            pal::RED
        } else {
            pal::GREEN_DIM
        };
        let galley = p.layout(sub, FontId::monospace(10.0), color, sub_rect.width());
        p.with_clip_rect(sub_rect).galley(sub_rect.min, galley, color);
    }

    fn draw_seek_bar(&mut self, ui: &mut Ui) {
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 18.0), Sense::click_and_drag());
        let p = ui.painter().clone();
        p.rect_filled(rect, 0.0, pal::BG);
        let track = Rect::from_min_max(rect.min + vec2(12.0, 6.0), rect.max - vec2(12.0, 6.0));
        inset(&p, track, pal::BG_DEEP);

        let (frac, seekable) = match &self.engine {
            Some(e) => {
                let info = e.info();
                match (info.duration, e.status()) {
                    (Some(d), Status::Playing | Status::Paused) if d.as_secs_f32() > 0.0 => {
                        ((e.position().as_secs_f32() / d.as_secs_f32()).clamp(0.0, 1.0), true)
                    }
                    _ => (0.0, false),
                }
            }
            None => (0.0, false),
        };
        let shown = self.seek_preview.unwrap_or(frac);

        if seekable {
            let fill = Rect::from_min_max(track.min, pos2(track.min.x + track.width() * shown, track.max.y));
            p.rect_filled(fill.shrink(1.0), 0.0, pal::GREEN_DIM);
            let knob_x = track.min.x + track.width() * shown;
            let knob = Rect::from_center_size(pos2(knob_x, track.center().y), vec2(14.0, 12.0));
            bevel(&p, knob, resp.is_pointer_button_down_on());
        }

        if seekable {
            if let Some(pos) = resp.interact_pointer_pos() {
                let f = ((pos.x - track.min.x) / track.width()).clamp(0.0, 1.0);
                if resp.dragged() || resp.drag_started() || resp.clicked() {
                    self.seek_preview = Some(f);
                }
            }
            if (resp.drag_stopped() || resp.clicked())
                && let (Some(f), Some(e)) = (self.seek_preview.take(), &self.engine)
                && let Some(d) = e.info().duration
            {
                e.seek(d.mul_f32(f));
            }
        } else {
            self.seek_preview = None;
        }
    }

    fn draw_controls(&mut self, ui: &mut Ui) {
        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(w, 34.0), Sense::hover());
        let p = ui.painter().clone();
        p.rect_filled(rect, 0.0, pal::BG);

        let mut x = rect.min.x + 12.0;
        let y = rect.min.y + 6.0;
        let size = vec2(26.0, 20.0);
        let mut button = |ui: &mut Ui, glyph: fn(&egui::Painter, Rect, Color32)| -> Response {
            let r = Rect::from_min_size(pos2(x, y), size);
            x += size.x + 2.0;
            let resp = ui.interact(r, ui.id().with(("btn", x as i32)), Sense::click());
            bevel(ui.painter(), r, resp.is_pointer_button_down_on());
            glyph(ui.painter(), r.shrink2(vec2(8.0, 6.0)), if resp.hovered() { pal::WHITE } else { pal::TITLE });
            resp
        };
        if button(ui, draw_prev_glyph).clicked() {
            self.prev();
        }
        if button(ui, draw_play_glyph).clicked() {
            self.play();
        }
        if button(ui, draw_pause_glyph).clicked()
            && let Some(e) = &self.engine
        {
            e.toggle_pause();
        }
        if button(ui, draw_stop_glyph).clicked() {
            self.stop();
        }
        if button(ui, draw_next_glyph).clicked() {
            self.next(true);
        }

        // shuffle / repeat toggles
        let mut tx = x + 10.0;
        let mut toggle = |ui: &mut Ui, label: &str, on: bool| -> Response {
            let r = Rect::from_min_size(pos2(tx, y + 2.0), vec2(44.0, 16.0));
            tx += 48.0;
            let resp = ui.interact(r, ui.id().with(("tgl", label)), Sense::click());
            bevel(ui.painter(), r, on || resp.is_pointer_button_down_on());
            let led = Rect::from_min_size(r.min + vec2(4.0, 5.0), vec2(6.0, 6.0));
            ui.painter().rect_filled(led, 1.0, if on { pal::GREEN } else { pal::DARK });
            ui.painter().text(
                r.min + vec2(14.0, 8.0),
                Align2::LEFT_CENTER,
                label,
                FontId::monospace(9.0),
                if on { pal::WHITE } else { pal::TITLE },
            );
            resp
        };
        if toggle(ui, "SHUF", self.shuffle).clicked() {
            self.shuffle = !self.shuffle;
            self.planned_next = None;
        }
        let rep_label = match self.repeat {
            Repeat::Off | Repeat::All => "REP",
            Repeat::One => "REP1",
        };
        if toggle(ui, rep_label, self.repeat != Repeat::Off).clicked() {
            self.repeat = match self.repeat {
                Repeat::Off => Repeat::All,
                Repeat::All => Repeat::One,
                Repeat::One => Repeat::Off,
            };
            self.planned_next = None;
        }
        if toggle(ui, "EQ", self.eq_visible).clicked() {
            self.eq_visible = !self.eq_visible;
            if !self.playlist_visible {
                self.fit_window(ui.ctx());
            }
        }
        if toggle(ui, "PL", self.playlist_visible).clicked() {
            if self.playlist_visible {
                self.full_height = ui.ctx().viewport_rect().height();
            }
            self.playlist_visible = !self.playlist_visible;
            self.fit_window(ui.ctx());
        }

        // volume slider on the right
        let vol_w = 110.0;
        let vol = Rect::from_min_size(pos2(rect.max.x - 12.0 - vol_w, y + 5.0), vec2(vol_w, 10.0));
        p.text(vol.min + vec2(-6.0, 5.0), Align2::RIGHT_CENTER, "VOL", FontId::monospace(9.0), pal::TITLE);
        inset(&p, vol, pal::BG_DEEP);
        // green -> amber -> red gradient as it gets loud, drawn as strips
        let strips = 22;
        for i in 0..strips {
            let t = i as f32 / strips as f32;
            if t > self.volume {
                break;
            }
            let c = if t < 0.6 {
                pal::GREEN_DIM.lerp_to_gamma(pal::GREEN, t / 0.6)
            } else {
                pal::AMBER.lerp_to_gamma(pal::RED, (t - 0.6) / 0.4)
            };
            let sx = vol.min.x + t * vol.width();
            p.rect_filled(Rect::from_min_max(pos2(sx + 1.0, vol.min.y + 1.0), pos2(sx + vol.width() / strips as f32 - 1.0, vol.max.y - 1.0)), 0.0, c);
        }
        let resp = ui.interact(vol.expand2(vec2(0.0, 6.0)), ui.id().with("vol"), Sense::click_and_drag());
        if let Some(pos) = resp.interact_pointer_pos()
            && (resp.dragged() || resp.clicked())
        {
            self.volume = ((pos.x - vol.min.x) / vol.width()).clamp(0.0, 1.0);
            if let Some(e) = &self.engine {
                e.set_volume(self.volume);
            }
        }
        let knob = Rect::from_center_size(pos2(vol.min.x + vol.width() * self.volume, vol.center().y), vec2(10.0, 14.0));
        bevel(&p, knob, resp.is_pointer_button_down_on());
    }

    fn draw_url_bar(&mut self, ui: &mut Ui) {
        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(w, 30.0), Sense::hover());
        ui.painter().rect_filled(rect, 0.0, pal::BG);
        let inner = Rect::from_min_max(rect.min + vec2(12.0, 5.0), rect.max - vec2(12.0, 5.0));
        let go_w = 40.0;
        let field = Rect::from_min_max(inner.min, pos2(inner.max.x - go_w - 6.0, inner.max.y));
        inset(ui.painter(), field, pal::LCD);
        let mut go = false;
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(field.shrink2(vec2(4.0, 2.0))));
        let edit = child.add_sized(
            child.available_size(),
            egui::TextEdit::singleline(&mut self.url_input)
                .font(FontId::monospace(11.0))
                .text_color(pal::GREEN)
                .background_color(pal::LCD)
                .hint_text("https://.../playlist.m3u")
                .frame(egui::Frame::NONE),
        );
        if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            go = true;
        }
        let go_rect = Rect::from_min_max(pos2(inner.max.x - go_w, inner.min.y), inner.max);
        let resp = ui.interact(go_rect, ui.id().with("go"), Sense::click());
        bevel(ui.painter(), go_rect, resp.is_pointer_button_down_on());
        ui.painter().text(go_rect.center(), Align2::CENTER_CENTER, "GO", FontId::monospace(10.0), pal::WHITE);
        if resp.clicked() {
            go = true;
        }
        if go {
            self.load_playlist();
        }
    }

    fn draw_playlist(&mut self, ui: &mut Ui) {
        self.draw_title_bar(ui, "PLAYLIST");
        let w = ui.available_width();
        let footer_h = 18.0;
        let list_h = (ui.available_height() - footer_h).max(40.0);
        let (outer, _) = ui.allocate_exact_size(vec2(w, list_h), Sense::hover());
        ui.painter().rect_filled(outer, 0.0, pal::BG);
        let list = Rect::from_min_max(outer.min + vec2(10.0, 2.0), outer.max - vec2(10.0, 2.0));
        inset(ui.painter(), list, pal::LCD);
        let inner = list.shrink(2.0);

        let mut play_now = None;
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner));
        child.set_clip_rect(inner);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(&mut child, |ui| {
                ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                let row_h = 15.0;
                let font = FontId::monospace(11.0);
                let width = ui.available_width();
                if self.playlist.tracks.is_empty() {
                    ui.add_space(6.0);
                    let msg = if self.playlist_rx.is_some() { "loading..." } else { "(empty)" };
                    ui.painter().text(
                        ui.cursor().min + vec2(6.0, 0.0),
                        Align2::LEFT_TOP,
                        msg,
                        font.clone(),
                        pal::GREEN_DIM,
                    );
                    return;
                }
                for (i, t) in self.playlist.tracks.iter().enumerate() {
                    let (r, resp) = ui.allocate_exact_size(vec2(width, row_h), Sense::click());
                    let is_cur = self.current == Some(i);
                    let is_sel = self.selected == Some(i);
                    if is_cur {
                        ui.painter().rect_filled(r, 0.0, pal::SEL);
                    } else if is_sel {
                        ui.painter().rect_stroke(r.shrink(0.5), 0.0, Stroke::new(1.0, pal::GREEN_DIM), StrokeKind::Inside);
                    }
                    let color = if is_cur { pal::WHITE } else { pal::GREEN };
                    let dur = t.duration_secs.map(fmt_time).unwrap_or_default();
                    let dur_galley = ui.painter().layout_no_wrap(dur, font.clone(), color);
                    let dur_w = dur_galley.size().x;
                    let label = format!("{:>3}. {}", i + 1, t.title);
                    let label_w = (width - dur_w - 12.0).max(20.0);
                    let galley = ui.painter().layout(label, font.clone(), color, label_w);
                    let text_rect = Rect::from_min_size(r.min + vec2(4.0, 1.0), vec2(label_w, row_h));
                    ui.painter().with_clip_rect(text_rect).galley(text_rect.min, galley, color);
                    ui.painter().galley(pos2(r.max.x - 4.0 - dur_w, r.min.y + 1.0), dur_galley, color);
                    if resp.clicked() {
                        self.selected = Some(i);
                    }
                    if resp.double_clicked() {
                        play_now = Some(i);
                    }
                    if is_cur && self.scroll_to_current {
                        ui.scroll_to_rect(r, Some(Align::Center));
                        self.scroll_to_current = false;
                    }
                }
            });
        if let Some(i) = play_now {
            self.play_index(i);
        }

        // footer
        let (foot, _) = ui.allocate_exact_size(vec2(w, footer_h), Sense::hover());
        ui.painter().rect_filled(foot, 0.0, pal::BG_DEEP);
        let total = self.playlist.total_secs();
        let left = if self.playlist.tracks.is_empty() {
            String::new()
        } else {
            format!("{} tracks  /  {}", self.playlist.tracks.len(), fmt_time(total))
        };
        ui.painter().text(foot.min + vec2(12.0, 9.0), Align2::LEFT_CENTER, left, FontId::monospace(9.0), pal::TITLE);
        ui.painter().text(
            pos2(foot.max.x - 12.0, foot.min.y + 9.0),
            Align2::RIGHT_CENTER,
            "Z prev  X play  C pause  V stop  B next",
            FontId::monospace(9.0),
            pal::LIGHT,
        );
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.tick(&ctx);
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(pal::BG))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                self.draw_title_bar(ui, "M3UNIT");
                self.draw_display(ui);
                self.draw_seek_bar(ui);
                self.draw_controls(ui);
                if self.eq_visible {
                    self.draw_eq(ui);
                }
                self.draw_url_bar(ui);
                if self.playlist_visible {
                    self.draw_playlist(ui);
                }
            });
    }
}

// --------------------------------------------------------------- equalizer

impl App {
    fn push_eq(&self) {
        if let Some(e) = &self.engine {
            e.eq.set(self.eq);
        }
    }

    fn draw_eq(&mut self, ui: &mut Ui) {
        self.draw_title_bar(ui, "EQUALIZER");
        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(w, 120.0), Sense::hover());
        let p = ui.painter().clone();
        p.rect_filled(rect, 0.0, pal::BG);
        let mut changed = false;
        let mut custom = false;

        // --- row 1: ON, presets, reset ---------------------------------------
        let y1 = rect.min.y + 6.0;
        let on_r = Rect::from_min_size(pos2(rect.min.x + 12.0, y1), vec2(36.0, 16.0));
        let resp = ui.interact(on_r, ui.id().with("eq_on"), Sense::click());
        bevel(&p, on_r, self.eq.enabled || resp.is_pointer_button_down_on());
        let led = Rect::from_min_size(on_r.min + vec2(4.0, 5.0), vec2(6.0, 6.0));
        p.rect_filled(led, 1.0, if self.eq.enabled { pal::GREEN } else { pal::DARK });
        p.text(
            on_r.min + vec2(14.0, 8.0),
            Align2::LEFT_CENTER,
            "ON",
            FontId::monospace(9.0),
            if self.eq.enabled { pal::WHITE } else { pal::TITLE },
        );
        if resp.clicked() {
            self.eq.enabled = !self.eq.enabled;
            changed = true;
        }

        let combo_r = Rect::from_min_size(pos2(on_r.max.x + 10.0, y1 - 1.0), vec2(140.0, 18.0));
        let selected = self.eq_preset.map(|i| PRESETS[i].name).unwrap_or("Custom");
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(combo_r));
        egui::ComboBox::from_id_salt("eq_preset")
            .width(140.0)
            .selected_text(RichText::new(selected).monospace().size(10.0).color(pal::GREEN))
            .show_ui(&mut child, |ui| {
                for (i, pr) in PRESETS.iter().enumerate() {
                    let label = RichText::new(pr.name).monospace().size(10.0);
                    if ui.selectable_label(self.eq_preset == Some(i), label).clicked() {
                        self.eq_preset = Some(i);
                        self.eq.gains_db = pr.gains_db;
                        changed = true;
                    }
                }
            });
        p.text(
            pos2(combo_r.max.x + 8.0, combo_r.center().y),
            Align2::LEFT_CENTER,
            "PRESET",
            FontId::monospace(9.0),
            pal::TITLE,
        );

        let reset_r = Rect::from_min_size(pos2(rect.max.x - 12.0 - 46.0, y1), vec2(46.0, 16.0));
        let resp = ui.interact(reset_r, ui.id().with("eq_reset"), Sense::click());
        bevel(&p, reset_r, resp.is_pointer_button_down_on());
        p.text(reset_r.center(), Align2::CENTER_CENTER, "RESET", FontId::monospace(9.0), pal::WHITE);
        if resp.clicked() {
            self.eq.gains_db = [0.0; eq::BANDS];
            self.eq.preamp_db = 0.0;
            self.eq_preset = Some(0);
            changed = true;
        }

        // --- row 2: sliders + response graph ---------------------------------
        let top = rect.min.y + 30.0;
        let h = 62.0;
        let label_y = top + h + 6.0;
        let small = FontId::monospace(8.0);

        let x_pre = rect.min.x + 26.0;
        if vslider(ui, "eq_pre", x_pre, top, h, &mut self.eq.preamp_db) {
            changed = true;
        }
        p.text(pos2(x_pre, label_y), Align2::CENTER_TOP, "PRE", small.clone(), pal::TITLE);
        p.text(pos2(x_pre + 6.0, top - 1.0), Align2::CENTER_BOTTOM, format!("{:+.0}", MAX_DB), small.clone(), pal::LIGHT);

        let x0 = rect.min.x + 70.0;
        let pitch = 24.0;
        for b in 0..eq::BANDS {
            let x = x0 + b as f32 * pitch;
            if vslider(ui, ("eq_band", b), x, top, h, &mut self.eq.gains_db[b]) {
                changed = true;
                custom = true;
            }
            p.text(pos2(x, label_y), Align2::CENTER_TOP, eq::LABELS[b], small.clone(), pal::TITLE);
        }

        let gx0 = x0 + eq::BANDS as f32 * pitch + 6.0;
        let graph = Rect::from_min_max(pos2(gx0, top), pos2(rect.max.x - 12.0, top + h));
        let sr = self
            .engine
            .as_ref()
            .map(|e| e.info().sample_rate)
            .filter(|&s| s > 0)
            .unwrap_or(44_100) as f32;
        draw_eq_graph(&p, graph, &self.eq, sr);

        if custom {
            self.eq_preset = None;
        }
        if changed {
            self.push_eq();
        }
    }
}

/// Vertical dB fader in `-MAX_DB..=MAX_DB`, 0.5 dB steps; double-click to zero.
fn vslider(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, x: f32, top: f32, h: f32, value: &mut f32) -> bool {
    let track = Rect::from_min_size(pos2(x - 3.0, top), vec2(6.0, h));
    let hit = track.expand2(vec2(8.0, 4.0));
    let resp = ui.interact(hit, ui.id().with(id), Sense::click_and_drag());
    let p = ui.painter().clone();
    inset(&p, track, pal::BG_DEEP);
    let mid = track.center().y;
    p.line_segment([pos2(track.min.x - 4.0, mid), pos2(track.max.x + 4.0, mid)], Stroke::new(1.0, pal::LIGHT));

    let mut changed = false;
    if let Some(pos) = resp.interact_pointer_pos()
        && (resp.dragged() || resp.clicked())
    {
        let t = ((pos.y - track.min.y) / track.height()).clamp(0.0, 1.0);
        let v = ((MAX_DB - t * 2.0 * MAX_DB) * 2.0).round() / 2.0;
        if (v - *value).abs() > f32::EPSILON {
            *value = v;
            changed = true;
        }
    }
    if resp.double_clicked() && *value != 0.0 {
        *value = 0.0;
        changed = true;
    }

    let t = (MAX_DB - *value) / (2.0 * MAX_DB);
    let ky = track.min.y + t * track.height();
    let (y0, y1) = if ky < mid { (ky, mid) } else { (mid, ky) };
    p.rect_filled(
        Rect::from_min_max(pos2(track.min.x + 1.0, y0), pos2(track.max.x - 1.0, y1)),
        0.0,
        pal::GREEN_DIM,
    );
    let knob = Rect::from_center_size(pos2(x, ky), vec2(14.0, 7.0));
    bevel(&p, knob, resp.is_pointer_button_down_on());
    if resp.hovered() || resp.dragged() {
        p.text(
            pos2(x, top - 2.0),
            Align2::CENTER_BOTTOM,
            format!("{:+.1}", value),
            FontId::monospace(8.0),
            pal::GREEN,
        );
    }
    changed
}

fn draw_eq_graph(p: &egui::Painter, r: Rect, eq: &EqParams, sample_rate: f32) {
    inset(p, r, pal::LCD);
    let inner = r.shrink(1.0);
    let y_of = |db: f32| inner.center().y - (db / MAX_DB) * (inner.height() / 2.0 - 2.0);
    for db in [-6.0, 0.0, 6.0] {
        let c = if db == 0.0 { pal::GREEN_DIM } else { Color32::from_rgb(0, 40, 12) };
        p.line_segment([pos2(inner.min.x, y_of(db)), pos2(inner.max.x, y_of(db))], Stroke::new(1.0, c));
    }
    for f in eq::FREQS {
        let t = (f / 20.0).log10() / (20_000.0f32 / 20.0).log10();
        let x = inner.min.x + t * inner.width();
        p.line_segment([pos2(x, inner.max.y - 3.0), pos2(x, inner.max.y)], Stroke::new(1.0, pal::GREEN_DIM));
    }
    let n = 96;
    let pts: Vec<Pos2> = (0..n)
        .map(|i| {
            let t = i as f32 / (n - 1) as f32;
            let f = 20.0 * 1000f32.powf(t);
            let db = eq.response_db(f, sample_rate).clamp(-MAX_DB, MAX_DB);
            pos2(inner.min.x + t * inner.width(), y_of(db))
        })
        .collect();
    let color = if eq.enabled { pal::GREEN } else { pal::GREEN_DIM };
    p.add(Shape::line(pts, Stroke::new(1.5, color)));
}

// ------------------------------------------------------------------ helpers

/// Sunken panel with a dark top/left edge and a light bottom/right edge.
fn inset(p: &egui::Painter, r: Rect, fill: Color32) {
    p.rect_filled(r, 0.0, fill);
    p.line_segment([r.left_top(), r.right_top()], Stroke::new(1.0, pal::DARK));
    p.line_segment([r.left_top(), r.left_bottom()], Stroke::new(1.0, pal::DARK));
    p.line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.0, pal::LIGHT));
    p.line_segment([r.right_top(), r.right_bottom()], Stroke::new(1.0, pal::LIGHT));
}

/// Raised button face; `pressed` flips the edges.
fn bevel(p: &egui::Painter, r: Rect, pressed: bool) {
    let (top, bottom) = if pressed { (pal::DARK, pal::LIGHT) } else { (pal::LIGHT, pal::DARK) };
    p.rect_filled(r, CornerRadius::same(1), if pressed { pal::FACE } else { pal::FACE_HI });
    p.line_segment([r.left_top(), r.right_top()], Stroke::new(1.0, top));
    p.line_segment([r.left_top(), r.left_bottom()], Stroke::new(1.0, top));
    p.line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.0, bottom));
    p.line_segment([r.right_top(), r.right_bottom()], Stroke::new(1.0, bottom));
}

fn readout(p: &egui::Painter, at: Pos2, w: f32, text: &str, font: &FontId) {
    let r = Rect::from_min_size(at, vec2(w, 14.0));
    p.rect_filled(r, 0.0, Color32::from_rgb(6, 10, 6));
    p.text(pos2(r.max.x - 3.0, r.center().y), Align2::RIGHT_CENTER, text, font.clone(), pal::GREEN);
}

fn draw_spectrum(p: &egui::Painter, r: Rect, spec: &Spectrum) {
    let gap = 1.0;
    let bar_w = (r.width() - gap * (BANDS as f32 - 1.0)) / BANDS as f32;
    for b in 0..BANDS {
        let x = r.min.x + b as f32 * (bar_w + gap);
        let segs = 11;
        let seg_h = r.height() / segs as f32;
        let lit = (spec.bars[b] * segs as f32).round() as usize;
        for s in 0..lit.min(segs) {
            let t = s as f32 / (segs - 1) as f32;
            let c = if t < 0.55 {
                pal::GREEN_DIM.lerp_to_gamma(pal::GREEN, t / 0.55)
            } else if t < 0.85 {
                pal::GREEN.lerp_to_gamma(pal::AMBER, (t - 0.55) / 0.3)
            } else {
                pal::AMBER.lerp_to_gamma(pal::RED, (t - 0.85) / 0.15)
            };
            let y1 = r.max.y - s as f32 * seg_h;
            p.rect_filled(Rect::from_min_max(pos2(x, y1 - seg_h + 1.0), pos2(x + bar_w, y1)), 0.0, c);
        }
        let py = r.max.y - spec.peaks[b] * r.height();
        if spec.peaks[b] > 0.02 {
            p.rect_filled(Rect::from_min_max(pos2(x, py - 1.0), pos2(x + bar_w, py)), 0.0, pal::WHITE);
        }
    }
}

fn draw_ticker(p: &egui::Painter, r: Rect, text: &str, t: f32) {
    let font = FontId::monospace(12.0);
    let galley = p.layout_no_wrap(text.to_string(), font.clone(), pal::GREEN);
    let clip = p.with_clip_rect(r);
    let y = r.center().y - galley.size().y / 2.0;
    if galley.size().x <= r.width() {
        clip.galley(pos2(r.min.x, y), galley, pal::GREEN);
        return;
    }
    let sep = "   ***   ";
    let looped = p.layout_no_wrap(format!("{text}{sep}"), font, pal::GREEN);
    let period = looped.size().x;
    let offset = (t * 32.0) % period;
    let x0 = r.min.x - offset;
    clip.galley(pos2(x0, y), looped.clone(), pal::GREEN);
    clip.galley(pos2(x0 + period, y), looped, pal::GREEN);
}

fn draw_play_glyph(p: &egui::Painter, r: Rect, c: Color32) {
    p.add(Shape::convex_polygon(vec![r.left_top(), pos2(r.max.x, r.center().y), r.left_bottom()], c, Stroke::NONE));
}

fn draw_pause_glyph(p: &egui::Painter, r: Rect, c: Color32) {
    let w = r.width() * 0.35;
    p.rect_filled(Rect::from_min_max(r.min, pos2(r.min.x + w, r.max.y)), 0.0, c);
    p.rect_filled(Rect::from_min_max(pos2(r.max.x - w, r.min.y), r.max), 0.0, c);
}

fn draw_stop_glyph(p: &egui::Painter, r: Rect, c: Color32) {
    p.rect_filled(r.shrink(1.0), 0.0, c);
}

fn draw_prev_glyph(p: &egui::Painter, r: Rect, c: Color32) {
    let bar_w = r.width() * 0.2;
    p.rect_filled(Rect::from_min_max(r.min, pos2(r.min.x + bar_w, r.max.y)), 0.0, c);
    let tri = Rect::from_min_max(pos2(r.min.x + bar_w + 1.0, r.min.y), r.max);
    p.add(Shape::convex_polygon(vec![tri.right_top(), pos2(tri.min.x, tri.center().y), tri.right_bottom()], c, Stroke::NONE));
}

fn draw_next_glyph(p: &egui::Painter, r: Rect, c: Color32) {
    let bar_w = r.width() * 0.2;
    p.rect_filled(Rect::from_min_max(pos2(r.max.x - bar_w, r.min.y), r.max), 0.0, c);
    let tri = Rect::from_min_max(r.min, pos2(r.max.x - bar_w - 1.0, r.max.y));
    p.add(Shape::convex_polygon(vec![tri.left_top(), pos2(tri.max.x, tri.center().y), tri.left_bottom()], c, Stroke::NONE));
}
