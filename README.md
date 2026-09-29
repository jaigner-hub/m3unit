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

Pressing previous more than three seconds into a track restarts it instead.

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
```
