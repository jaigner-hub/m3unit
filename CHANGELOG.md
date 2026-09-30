# Changelog

All notable changes to M3UNIT are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.1] - 2026-09-30

### Added

- Custom title bar in place of the OS window chrome, with minimize,
  maximize/restore and close buttons drawn in the skin. Drag the bar to move
  the window and double-click it to maximize.
- Resize handles on the right and bottom window edges, plus a ridge grip in
  the playlist footer. The bottom edge only resizes while the playlist is
  visible, since the window height is otherwise fitted to its contents.
- This changelog.

### Changed

- The window is created without OS decorations.
- README screenshot retaken from the frameless build, and the download link
  now points at the latest release.

## [0.1.0] - 2026-09-30

First release.

### Added

- Stream any http(s) `.m3u` / EXTM3U playlist straight from the web. Relative
  entries resolve against the playlist URL. MP3, FLAC, Vorbis, WAV and MP4/AAC
  are decoded.
- archive.org Live Music Archive browser (FIND panel): search by artist or
  show title, sort by date or popularity, page through results and
  double-click a show to play it. Only shows with MP3 derivatives are listed.
- archive.org metadata enrichment: real song titles and lengths, plus the show
  title and artist in the display.
- Gapless playback with background prefetch and decode of the next track.
- Seekable streams via a read-ahead HTTP buffer.
- 10-band graphic equalizer (31 Hz to 16 kHz, ±12 dB) with preamp, 13
  presets and a live frequency-response graph.
- FFT spectrum analyser, shuffle, repeat (all / one / off) and keyboard
  shortcuts for every transport control.
- EQ / FIND / PL toggles to show or hide panels; the window shrinks to fit
  when the playlist is hidden.
- Windows build with no system dependencies: WASAPI audio and pure-Rust TLS.

[Unreleased]: https://github.com/jaigner-hub/m3unit/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/jaigner-hub/m3unit/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/jaigner-hub/m3unit/releases/tag/v0.1.0
