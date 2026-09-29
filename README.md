# M3UNIT

A small desktop audio player with late-90s skinned-player vibes. Paste the URL
of an `.m3u` playlist that lives on the internet and it streams the tracks
straight from the web, no download step.

Built in Rust with [egui](https://github.com/emilk/egui) for the window,
[rodio](https://github.com/RustAudio/rodio) (symphonia) for decoding, and
[stream-download](https://github.com/aschey/stream-download-rs) for a
read-ahead HTTP buffer that also lets you seek inside a stream.

## Run it

```
cargo run --release
```

Playback is gapless: while a track plays, the next one is opened, buffered
and decoded in the background and appended to the audio queue, so the switch
happens inside the audio thread with no fetch pause.

It opens with a Billy Strings show from archive.org already loaded. Put any
other http(s) `.m3u` / `.m3u8`-style playlist URL in the box and hit **GO**.

For archive.org items the player also pulls the item's metadata so the
playlist shows real song titles and lengths instead of file names.

## Controls

| Action | Mouse | Keyboard |
| --- | --- | --- |
| Previous / next track | ⏮ ⏭ | `Z` / `B` |
| Play, pause, stop | ▶ ⏸ ⏹ | `X`, `C` or `Space`, `V` |
| Seek | drag the seek bar | `←` / `→` (5 s) |
| Volume | drag the VOL slider | `↑` / `↓` |
| Play a specific track | double-click it in the playlist | |
| Shuffle / repeat (all → one → off) | SHUF / REP toggles | |
| Show / hide equalizer and playlist | EQ / PL toggles | |

Pressing previous more than three seconds into a track restarts it instead.

## Equalizer

The EQ button in the transport row shows or hides a 10-band graphic equalizer
(ISO octave centres, 31 Hz to 16 kHz, ±12 dB) plus a preamp. Drag a fader,
double-click it to zero, or pick a preset: Flat, Rock, Pop, Live, Dance,
Classical, Jazz, Bluegrass, Acoustic, Vocal, Bass Boost, Treble Boost,
Loudness. Moving a fader after picking a preset switches the menu to
"Custom". The graph on the right shows the resulting frequency response.

Under the hood each band is an RBJ peaking biquad applied per channel before
the visualizer tap, with a soft knee above 0.9 full scale so heavy boosts
don't hard-clip.

## Building on Windows

Requires the Rust toolchain (`x86_64-pc-windows-msvc`) with the Visual Studio
Build Tools. No other system dependencies: audio goes through WASAPI and TLS is
pure Rust (`rustls` + `ring`).

If **Smart App Control** is enabled, Windows may occasionally refuse to run a
freshly compiled build script ("An Application Control policy has blocked this
file", os error 4551). Re-running `cargo build` usually gets past it.

The `target/` directory can get large. If this folder is synced by OneDrive,
consider excluding `target` from sync or setting `CARGO_TARGET_DIR` elsewhere.

## Building on Linux

Needs ALSA headers and pkg-config (and the usual X11/Wayland libs for a GUI):

```
sudo apt install pkg-config libasound2-dev libxkbcommon-dev libgl1-mesa-dev
```

## Layout

```
src/main.rs      entry point: tokio runtime, TLS provider, window setup
src/app.rs       egui UI: display, spectrum, seek/volume, transport, playlist
src/player.rs    audio engine: HTTP streaming -> decoder -> output device
src/playlist.rs  M3U parsing + archive.org metadata enrichment
src/viz.rs       sample tap and FFT-based spectrum analyser
src/eq.rs        10-band peaking-biquad equalizer and presets
```
