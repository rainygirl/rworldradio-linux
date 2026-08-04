# Notes for agents and contributors

Engineering notes for changing this code. The README is deliberately install- and
operation-only; everything about *why the code looks like this* lives here.

Written in English to match the source comments.

## What this is

A Linux internet radio player in Rust, ported from a Haiku OS (BeAPI) application.
The Haiku C++ sources are no longer in this repository, but the port followed them
closely and several comments still refer to them.

| Haiku (BeAPI) | here | module |
|---|---|---|
| `BApplication` / `BWindow` / `BListView` | egui + eframe | `ui`, `level_meter` |
| `BMediaFile` / `BMediaTrack` | symphonia | `player`, `format` |
| `BSoundPlayer` | cpal (ALSA) | `audio`, `player` |
| `BUrlRequest` / `BHttpRequest` | ureq + rustls | `net` |
| `BAdapterIO` / `BInputAdapter` | `ByteQueue` | `byte_queue`, `hls` |
| `JsonValue` (hand-written parser) | serde_json | `dataset` |
| Deskbar menu symlink + reboot | `.desktop` + XDG dirs | `install.sh` |

The dataset format, the HLS playlist parser, the MPEG-TS demuxer, the dataset
search-path order and the generation-counter session model were ported rather than
redesigned. Rust's ownership removed the need for the original's hand-rolled
refcounted `SessionPtr`.

`libasound` (ALSA) and `ring` (rustls's crypto backend, which compiles C and
assembly) are the only non-Rust code in the dependency tree. `ring` is why a C
compiler is a build requirement.

## Module map

```
src/
  main.rs          window setup only
  lib.rs           module list + the Haiku mapping above
  ui.rs            the whole window: two search fields, both lists, status row
  level_meter.rs   the LED bar widget
  fonts.rs         system font fallback discovery (CJK, Cyrillic, ...)
  icon.rs          window/taskbar icon, compiled in from packaging/
  station.rs       one station record
  dataset.rs       parses countries.json + countries/<file>.json
  station_cache.rs finds data/ and loads the whole dataset
  net.rs           HTTP: one-shot GETs and one streaming GET
  byte_queue.rs    bounded producer/consumer byte pipe + a MediaSource over it
  hls.rs           HLS worker: playlist polling, segment fetch, feeds ByteQueue
  m3u8.rs          HLS playlist parsing and URI resolution
  segment.rs       one HLS segment -> elementary stream (TS or packed audio)
  ts_demuxer.rs    MPEG-TS -> raw ADTS AAC / MPEG audio
  adts.rs          ADTS variant normalization (see "Invariants")
  format.rs        picks which symphonia reader to use
  player.rs        session lifecycle, decode loop, cpal output
  audio.rs         ring buffer between decoder and audio callback + resampler
examples/
  probe_stream.rs  decode a station without opening an audio device
  play_stream.rs   decode and play it, no GUI
```

## Invariants - do not "simplify" these away

Each of these exists because a real station broke without it. They are covered by
unit tests; if you change the behaviour, the tests should fail.

1. **`format.rs` must not hand a raw elementary stream to symphonia's `Probe`.**
   `Probe` finds a reader by scanning for a registered magic marker. For raw ADTS
   or MPEG audio there may be no marker it knows, and because a radio stream never
   ends, "not found" means *scanning forever*, not an error. So the reader is
   chosen from the stream's own first frame header, falling back to content type,
   then URL, then the dataset's codec field, and `AdtsReader`/`MpaReader` are
   constructed directly.

2. **`adts.rs` must keep normalizing ADTS headers.** symphonia 0.5's `AdtsReader`
   syncs only on the exact pattern `0xFFF1` (MPEG-4, no CRC). MPEG-2 ADTS
   (`0xFFF9`) and CRC-protected ADTS are equally valid and common, and they would
   otherwise block forever. The normalizer walks the frame chain and rewrites the
   `ID` bit and the CRC framing - lossless, the AAC payload is untouched. It walks
   frames rather than pattern-matching bytes precisely so that a `0xFFF9` pair
   *inside* compressed payload is not mistaken for a header.

3. **`segment.rs` checks the packed-audio syncword before the MPEG-TS heuristic.**
   TS detection is necessarily a heuristic (0x47 at a 188-byte stride), and
   compressed audio can satisfy it by chance. A leading syncword is unambiguous
   because TS always starts with 0x47.

4. **`m3u8::resolve_url` handles protocol-relative URIs (`//host/path`) before
   host-absolute ones (`/path`).** Reversing that produces a double-slash URL
   pasted onto the wrong origin. It also strips the query string before taking the
   directory, so a `?token=a/b` is not treated as a path.

5. **Playlist detection uses the response body, not the URL.** A station URL can
   302 to a CDN-signed `…m3u8?Policy=…`, and some `.m3u`/`.pls` URLs return a
   playlist while plenty of others serve audio directly despite the extension.
   `player::looks_like_playlist` therefore checks for `#EXTM3U`, `[playlist]`, an
   HLS mime type, *and* a body that starts with `http` - one real station returns a
   bare URL with content type `application/octet-stream`, where that last check is
   the only tell. Following is capped at two hops.

6. **`ui::list_panel_frame` must stay.** egui's `Frame::side_top_panel` and
   `Frame::central_panel` use different vertical inner margins (2 vs 8), which puts
   the two search fields on different baselines. One shared frame fixes it, and
   `egui_default_panel_frames_are_the_thing_that_misaligns` fails if a future egui
   makes the defaults agree - at which point the helper can go.

7. **Lists must use a justified layout.** A plain `selectable_label` is only as
   wide as its text, so clicking to the right of a short station name does
   nothing.

8. **`icon.rs` uses `include_bytes!("../packaging/...")`.** `packaging/` has to stay
   a sibling of `src/`.

## Concurrency model

- One session at a time, tracked by a generation counter in `player.rs`. `play` and
  `stop` bump it; a superseded session notices and unwinds without touching the
  status line. This is the Haiku design, kept deliberately.
- Stopping must be immediate, which takes three steps in `tear_down_active`: mute
  the sink (so the next audio callback is silent), then close the `ByteQueue` (so a
  decoder blocked on a network read returns at once rather than waiting out a
  socket timeout).
- `AudioSink::push` blocks when the ring is full. That is what paces decoding to
  real time - do not add a "just drop samples" path.
- The cpal `Stream` is owned by the decoder thread for the session's lifetime.
  Don't try to move it elsewhere.
- The cpal error callback records only the *first* error and logs it once. It used
  to log unconditionally, which produced a 2.9 GB stderr file when an output device
  went away. The decode loop polls `Output::failure()` and ends the session, so a
  dead device surfaces as an error instead of a silent "Now Playing".

## Platform seams (the macOS port depends on these)

The macOS port is a separate repository that vendors this crate into its `core/`
directory. Changing these signatures breaks it:

- `station_cache::load_from(&[PathBuf])`, `exe_relative_candidates()`,
  `relative_candidates()` - finding `data/` differs only in the candidate list.
- `fonts::install_fallbacks(ctx, candidates)` - takes the paths to try;
  `install_system_fallback` is the Linux list plus `fc-match`.
- `ui::RadioApp::with_loader(cc, loader)` - lets a port inject its own dataset
  search path.

Fix bugs here and re-sync the copy there:

```
rsync -a --delete \
    --exclude target --exclude data --exclude tools --exclude .git \
    --exclude VENDORED.md \
    /path/to/rworldradio-linux/ /path/to/rworldradio-mac/core/
```

Never edit the vendored copy - the next re-sync silently reverts it.

## Deliberate design choices

- **`dataset.rs` parses into `serde_json::Value` and pulls fields with helpers**
  rather than `#[derive(Deserialize)]`. radio-browser occasionally emits a numeric
  field as a string and vice versa, and lat/lon are frequently `null`. A strict
  derive would turn a schema wobble into a whole country failing to load. The Haiku
  original's `JsonValue::AsString`/`AsInt` were equally tolerant.
- **A country file that is missing or corrupt is skipped, not fatal.** The status
  line reports how many were skipped.
- **Resampling is linear interpolation** (`audio::Converter`). The common cases are
  44.1k→48k and 48k→48k for 64-320kbps lossy audio, where interpolation artefacts
  sit far below the codec's own. Deliberately not a dependency.
- **The lists are virtualised** (`show_rows`) because egui draws every widget every
  frame and one country has ~6500 stations.
- **Search fields exist** because egui has no `BListView`-style keyboard
  type-ahead, and there are 229 countries and ~51k stations.
- **No menu bar.** Its only item was Quit, which the window's close control already
  provides.

## Known limitations and their causes

- **HE-AAC / AAC+ decodes its AAC-LC core layer only** - symphonia has no SBR or PS
  support. Stations play, at typically half the nominal sample rate and without the
  high band. This is the one place a C library (fdk-aac) would beat pure Rust
  today.
- **No Opus** - symphonia 0.5 has no Opus decoder. Ogg/Vorbis works.
- **Encrypted HLS (`#EXT-X-KEY`) and fragmented MP4/CMAF segments are not handled**,
  same as the Haiku original.
- Some stations serve a player web page, or refuse non-browser clients, or are
  geo-restricted. Nothing in the app can fix that; the error message should make it
  obvious which it is.

## Building and testing

```
cargo test                       # 56 unit tests, all inline under #[cfg(test)]
cargo build --release
```

The inline tests compile out of release builds entirely, so they cost nothing in
the shipped binary. Keep them.

On a 2-core / ~4GB machine the release profile's thin LTO makes linking the
memory peak:

```
CARGO_PROFILE_RELEASE_LTO=false CARGO_BUILD_JOBS=2 nice cargo build --release
```

Two diagnostics drive the same code paths as the app:

```
cargo run --release --example probe_stream -- "BBC Radio 4" 5   # decode only
cargo run --release --example play_stream  -- "BBC Radio 4" 8   # decode + ALSA
```

`probe_stream` is the right tool when adding stream-format support: it reports how
the stream was classified, its rate and channel count, frames decoded and peak
amplitude, without needing an audio device.

## How this was verified

Worth reproducing if you change the streaming or audio path.

- **Debian 13 aarch64 under QEMU**: release build and all tests pass; the GUI runs
  under X11 with Mesa's software rasterizer; playback captured through the ALSA
  loopback device (`snd-aloop`) matched the level meter's reading (peak 0.392 FS),
  which is what proves audio reaches the sound card rather than just the ring
  buffer.
- **Linux Mint 20.3 x86_64, real hardware**: build 10m24s with LTO disabled, tests
  pass, `install.sh` verified end to end including `desktop-file-validate`.
- **Live dataset sampling**: 34/40 random stations and 19/25 random HLS stations
  played. Every failure was a dead or geo-blocked stream (connection refused, HTTP
  403/502), not a format or decoder problem. Re-run this after touching `format.rs`,
  `adts.rs`, `segment.rs` or `hls.rs`.

## Regenerating the icon

The icon is drawn in code, so any size can be produced:

```
cd packaging && python3 make-icons.py                 # hicolor PNGs
cd packaging && python3 make-icons.py --icns PATH     # also a macOS .icns
```

Requires Pillow; the `.icns` step requires macOS (`iconutil`).
