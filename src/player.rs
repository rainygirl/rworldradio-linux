//! Streams and decodes one internet radio station at a time entirely
//! in-process: the network layer buffers bytes into a `ByteQueue`, symphonia
//! picks a demuxer/decoder for them, and cpal pulls decoded frames for
//! playback. All setup and teardown happens off the caller's thread; status
//! changes are reported asynchronously over a channel so this is safe to drive
//! from the UI thread.
//!
//! This replaces the Haiku build's `RadioPlayer`
//! (`BMediaFile`/`BMediaTrack`/`BSoundPlayer`). The generation counter that made
//! a superseded session tear itself down is kept verbatim in spirit - Rust's
//! ownership just removes the need for the hand-rolled refcounted `SessionPtr`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;

use crate::audio::{AudioSink, Converter};
use crate::byte_queue::{ByteQueue, QueueReader};
use crate::format::{self, StreamFormat};
use crate::hls;
use crate::net;
use crate::station::Station;

#[derive(Clone, Debug)]
pub enum PlayerEvent {
    Connecting { station: String },
    Playing { station: String },
    Stopped,
    Error { station: String, detail: String },
}

struct ActiveSession {
    stop: Arc<AtomicBool>,
    queue: Option<Arc<ByteQueue>>,
    sink: Option<Arc<AudioSink>>,
}

#[derive(Clone)]
struct Shared {
    events: Sender<PlayerEvent>,
    /// Called after every event so the UI thread repaints instead of waiting
    /// for the next input.
    waker: Arc<dyn Fn() + Send + Sync>,
    generation: Arc<AtomicU64>,
    active: Arc<Mutex<Option<ActiveSession>>>,
}

impl Shared {
    fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }

    fn emit(&self, event: PlayerEvent) {
        let _ = self.events.send(event);
        (self.waker)();
    }

    /// Reports `event` only if this session hasn't already been superseded - a
    /// stale session's error must not overwrite the new station's status.
    fn emit_if_current(&self, generation: u64, event: PlayerEvent) {
        if self.is_current(generation) {
            self.emit(event);
        }
    }
}

pub struct Player {
    shared: Shared,
}

impl Player {
    pub fn new(events: Sender<PlayerEvent>, waker: Arc<dyn Fn() + Send + Sync>) -> Self {
        Player {
            shared: Shared {
                events,
                waker,
                generation: Arc::new(AtomicU64::new(0)),
                active: Arc::new(Mutex::new(None)),
            },
        }
    }

    /// Stops whatever is currently playing (if anything) and starts connecting
    /// to the given station. Returns immediately.
    pub fn play(&self, station: Station) {
        let generation = self.begin_session();

        self.shared.emit(PlayerEvent::Connecting {
            station: station.name.clone(),
        });

        let stop = Arc::new(AtomicBool::new(false));
        *self.shared.active.lock().unwrap() = Some(ActiveSession {
            stop: Arc::clone(&stop),
            queue: None,
            sink: None,
        });

        let shared = self.shared.clone();
        let spawned = thread::Builder::new()
            .name("radio-session".into())
            .spawn(move || run_session(shared, station, generation, stop));

        if let Err(error) = spawned {
            self.shared.emit(PlayerEvent::Error {
                station: String::new(),
                detail: format!("could not start playback thread: {error}"),
            });
        }
    }

    /// Stops playback. Safe to call when nothing is playing.
    pub fn stop(&self) {
        if self.tear_down_active() {
            self.shared.emit(PlayerEvent::Stopped);
        }
    }

    /// Peak amplitude (0.0-1.0) of the most recently played audio buffer, or 0
    /// when nothing is playing. Cheap to poll from the UI thread every frame -
    /// it's just an atomic read behind a short-lived lock.
    pub fn level(&self) -> f32 {
        let active = self.shared.active.lock().unwrap();
        active
            .as_ref()
            .and_then(|session| session.sink.as_ref())
            .map(|sink| sink.level())
            .unwrap_or(0.0)
    }

    /// Invalidates any running session and returns the new generation.
    fn begin_session(&self) -> u64 {
        self.tear_down_active();
        self.shared.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Signals the running session to unwind. Returns true if there was one.
    fn tear_down_active(&self) -> bool {
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        let session = self.shared.active.lock().unwrap().take();
        match session {
            Some(session) => {
                // Mute first so audio stops on the very next callback, then
                // close the byte queue so a blocked decoder read returns at
                // once rather than waiting on the network.
                session.stop.store(true, Ordering::SeqCst);
                if let Some(sink) = &session.sink {
                    sink.mute();
                }
                if let Some(queue) = &session.queue {
                    queue.close(None);
                }
                true
            }
            None => false,
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(session) = self.shared.active.lock().unwrap().take() {
            session.stop.store(true, Ordering::SeqCst);
            if let Some(sink) = &session.sink {
                sink.mute();
            }
            if let Some(queue) = &session.queue {
                queue.close(None);
            }
        }
    }
}

/// `BMediaFile(BUrl)` on Haiku - via its own http_streamer add-on - only handled
/// a plain progressive HTTP body, not HLS's playlist-of-segments scheme.
/// Stations whose stream is an .m3u8 playlist (common for broadcasters like the
/// BBC, who moved their public endpoints to HLS-only some years ago) go through
/// the `hls` module instead. Same split here, same detection.
fn is_hls_url(url: &str) -> bool {
    let path = url.split('?').next().unwrap_or(url);
    path.to_ascii_lowercase().ends_with(".m3u8")
}

/// Trims trailing CR/LF/whitespace from a Tune.ashx text response, which is just
/// the resolved stream URL on its own line.
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

/// How long to wait for enough of a stream's first bytes to identify its codec.
/// Only a few bytes are needed, so this is a stall guard, not a buffering delay.
const HEAD_PEEK_TIMEOUT: Duration = Duration::from_secs(15);

/// Enough leading bytes to recognize a playlist marker (`#EXTM3U`, `[playlist]`)
/// as well as an audio frame header.
const HEAD_PEEK_BYTES: usize = 16;

/// How many playlist hops to follow. Radio-browser entries routinely point at a
/// `.m3u`/`.pls` that points at the stream, and a CDN can redirect to an HLS
/// playlist; more than two hops means something is looping.
const MAX_PLAYLIST_HOPS: usize = 2;

/// True if this response body is a playlist rather than audio.
///
/// This matters because the URL is not a reliable signal: stations very commonly
/// use an extensionless URL that 302s to `…m3u8?Policy=…` (CDN signing), so the
/// only dependable evidence is what came back.
fn looks_like_playlist(head: &[u8], content_type: Option<&str>) -> bool {
    if head.starts_with(b"#EXTM3U") || head.starts_with(b"#EXT-X") {
        return true;
    }
    // SHOUTcast/Winamp .pls
    if head.len() >= 10 && head[..10].eq_ignore_ascii_case(b"[playlist]") {
        return true;
    }
    // A bare URL list with no header at all: some .m3u endpoints return just the
    // stream URL, with a content type of application/octet-stream (confirmed:
    // streams.br.de/br-klassik_2.m3u). No audio bitstream begins with the ASCII
    // "http", so this is unambiguous.
    if head.starts_with(b"http://") || head.starts_with(b"https://") {
        return true;
    }
    let Some(content_type) = content_type else {
        return false;
    };
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();
    matches!(
        mime.as_str(),
        "application/vnd.apple.mpegurl"
            | "application/x-mpegurl"
            | "audio/x-mpegurl"
            | "audio/mpegurl"
            | "audio/x-scpls"
            | "audio/scpls"
    )
}

/// First playable URL in a legacy `.m3u` (bare URL lines) or `.pls`
/// (`FileN=<url>`) playlist.
fn first_stream_url(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // .pls entries are `File1=http://…`; everything else in a .pls (Title1=,
        // Length1=, numberofentries=) is skipped by the http check below.
        let candidate = match line.split_once('=') {
            Some((key, value)) if key.trim_start().to_ascii_lowercase().starts_with("file") => {
                value.trim()
            }
            // A comment in a .m3u, but only if it isn't a URL-bearing line.
            _ if line.starts_with('#') => continue,
            _ => line,
        };
        if candidate.starts_with("http://") || candidate.starts_with("https://") {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Resolves a station to a byte stream plus the format symphonia should read it
/// as: a TuneIn resolver round-trip if needed, then either the HLS pipeline or a
/// plain progressive body.
///
/// Split out of the session so `examples/probe_stream.rs` can drive exactly the
/// same path without opening an audio device.
pub fn open_byte_source(
    station: &Station,
    stop: Arc<AtomicBool>,
) -> Result<(Arc<ByteQueue>, StreamFormat), String> {
    let mut stream_url = station.playback_url().to_string();
    if station.needs_tunein_resolve {
        // TuneIn's browse listings only ever give a Tune.ashx resolver link,
        // never a directly playable stream - resolve it to the real URL first.
        // The response is just the URL as plain text.
        let resolved = net::get_text(&stream_url)
            .map_err(|error| format!("could not resolve TuneIn stream: {error}"))?;
        stream_url = first_line(&resolved);
        if stream_url.is_empty() {
            return Err("TuneIn resolver returned an empty stream URL".to_string());
        }
    }

    open_stream_url(&stream_url, &station.codec, stop, 0)
}

/// Opens one URL, following playlist indirection up to [`MAX_PLAYLIST_HOPS`].
fn open_stream_url(
    url: &str,
    dataset_codec: &str,
    stop: Arc<AtomicBool>,
    hops: usize,
) -> Result<(Arc<ByteQueue>, StreamFormat), String> {
    if is_hls_url(url) {
        let stream = hls::open(url, stop)
            .map_err(|error| format!("could not open HLS stream: {error}"))?;
        return Ok((stream.queue, format::for_hls(stream.codec)));
    }

    let opened = net::open_stream(url, Arc::clone(&stop))
        .map_err(|error| format!("could not open stream: {error}"))?;
    // The body's own first bytes are the most reliable signal there is, so look at
    // them before trusting the content type or the URL.
    let head = opened.queue.peek(HEAD_PEEK_BYTES, HEAD_PEEK_TIMEOUT);

    // A redirect may have landed on a playlist (CDN-signed HLS URLs do this), or
    // the station's URL may simply be a .m3u/.pls pointing at the real stream.
    let is_playlist = is_hls_url(&opened.final_url)
        || looks_like_playlist(&head, opened.content_type.as_deref());
    if is_playlist {
        if hops >= MAX_PLAYLIST_HOPS {
            return Err("playlist redirects went in circles".to_string());
        }
        // This body is text, not audio - drop it and follow where it points.
        opened.queue.close(None);
        return follow_playlist(&opened.final_url, dataset_codec, stop, hops);
    }

    let stream_format = format::for_http(
        &head,
        opened.content_type.as_deref(),
        &opened.final_url,
        dataset_codec,
    );
    Ok((opened.queue, stream_format))
}

/// Fetches a playlist and opens whatever it points at.
fn follow_playlist(
    url: &str,
    dataset_codec: &str,
    stop: Arc<AtomicBool>,
    hops: usize,
) -> Result<(Arc<ByteQueue>, StreamFormat), String> {
    let text =
        net::get_text(url).map_err(|error| format!("could not fetch playlist: {error}"))?;

    // An HLS playlist (master or media) goes to the HLS pipeline; a legacy
    // .m3u/.pls is just a pointer to a normal stream.
    if crate::m3u8::is_master_playlist(&text) || text.contains("#EXT-X-") {
        let stream = hls::open(url, stop)
            .map_err(|error| format!("could not open HLS stream: {error}"))?;
        return Ok((stream.queue, format::for_hls(stream.codec)));
    }

    let target = first_stream_url(&text)
        .ok_or_else(|| "playlist contained no stream URL".to_string())?;
    open_stream_url(&target, dataset_codec, stop, hops + 1)
}

fn run_session(shared: Shared, station: Station, generation: u64, stop: Arc<AtomicBool>) {
    match setup_and_play(&shared, &station, generation, stop) {
        Ok(EndReason::StreamEnded) => {
            shared.emit_if_current(generation, PlayerEvent::Stopped);
        }
        Ok(EndReason::Superseded) => {}
        Err(detail) => {
            shared.emit_if_current(
                generation,
                PlayerEvent::Error {
                    station: station.name.clone(),
                    detail,
                },
            );
        }
    }
}

enum EndReason {
    /// The server closed the connection or the playlist ended.
    StreamEnded,
    /// Another `play`/`stop` took over; that call owns the status line now.
    Superseded,
}

fn setup_and_play(
    shared: &Shared,
    station: &Station,
    generation: u64,
    stop: Arc<AtomicBool>,
) -> Result<EndReason, String> {
    if !shared.is_current(generation) {
        return Ok(EndReason::Superseded);
    }

    let (queue, stream_format) = open_byte_source(station, Arc::clone(&stop))?;

    // Register the queue so `stop` can close it and unblock the decoder.
    {
        let mut active = shared.active.lock().unwrap();
        match active.as_mut() {
            Some(session) if shared.is_current(generation) => {
                session.queue = Some(Arc::clone(&queue));
            }
            _ => {
                queue.close(None);
                return Ok(EndReason::Superseded);
            }
        }
    }

    let mut format = format::open_reader(stream_format, QueueReader::new(Arc::clone(&queue)))?;

    let track = format
        .tracks()
        .iter()
        .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| "no audio track in stream".to_string())?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|error| format!("no decoder for this stream: {error}"))?;

    if !shared.is_current(generation) {
        return Ok(EndReason::Superseded);
    }

    let mut output: Option<Output> = None;
    let mut sample_buffer: Option<SampleBuffer<f32>> = None;
    let mut converted: Vec<f32> = Vec::new();

    loop {
        if !shared.is_current(generation) || stop.load(Ordering::Relaxed) {
            return Ok(EndReason::Superseded);
        }

        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                return Ok(EndReason::StreamEnded);
            }
            Err(SymphoniaError::ResetRequired) => {
                // A new stream started mid-connection (icecast station change).
                decoder.reset();
                continue;
            }
            Err(error) => {
                if !shared.is_current(generation) || stop.load(Ordering::Relaxed) {
                    return Ok(EndReason::Superseded);
                }
                // Nothing ever played: report why. Otherwise treat it as the
                // stream simply ending.
                return if output.is_none() {
                    Err(format!("stream error: {error}"))
                } else {
                    Ok(EndReason::StreamEnded)
                };
            }
        };
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // Malformed packet: symphonia says these are recoverable, so skip it
            // rather than dropping the station. A radio stream joined mid-frame
            // produces a few of these before it syncs up.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(error) => {
                if !shared.is_current(generation) || stop.load(Ordering::Relaxed) {
                    return Ok(EndReason::Superseded);
                }
                return if output.is_none() {
                    Err(format!("decode failed: {error}"))
                } else {
                    Ok(EndReason::StreamEnded)
                };
            }
        };

        let spec = *decoded.spec();
        let capacity = decoded.capacity() as u64;

        // First decoded frame: now the actual sample rate/channel count is known,
        // so the audio device can be opened and the converter configured.
        if output.is_none() {
            let opened = Output::open(spec.rate, spec.channels.count())?;
            {
                let mut active = shared.active.lock().unwrap();
                match active.as_mut() {
                    Some(session) if shared.is_current(generation) => {
                        session.sink = Some(Arc::clone(&opened.sink));
                    }
                    _ => return Ok(EndReason::Superseded),
                }
            }
            opened
                .stream
                .play()
                .map_err(|error| format!("could not start audio output: {error}"))?;
            shared.emit_if_current(
                generation,
                PlayerEvent::Playing {
                    station: station.name.clone(),
                },
            );
            output = Some(opened);
        }
        let output = output.as_mut().unwrap();

        let buffer = match sample_buffer.as_mut() {
            Some(buffer) if buffer.capacity() as u64 >= capacity => buffer,
            _ => {
                sample_buffer = Some(SampleBuffer::<f32>::new(capacity, spec));
                sample_buffer.as_mut().unwrap()
            }
        };
        buffer.copy_interleaved_ref(decoded);

        converted.clear();
        output.converter.process(buffer.samples(), &mut converted);
        // A full ring blocks here, which is what paces decoding to real time.
        if !output.sink.push(&converted) {
            return Ok(EndReason::Superseded);
        }

        if let Some(error) = output.failure() {
            return Err(format!("audio output failed: {error}"));
        }
    }
}

/// The open audio device: the cpal stream (which must outlive playback and is
/// therefore owned by the decoder thread) plus the converter feeding it.
struct Output {
    stream: cpal::Stream,
    sink: Arc<AudioSink>,
    converter: Converter,
    /// First error cpal reported on the stream, if any. The decode loop polls
    /// this and ends the session - a dead output device (unplugged headphones, a
    /// PipeWire restart) otherwise means playback continues into nothing while
    /// the UI still says "Now Playing".
    device_error: Arc<Mutex<Option<String>>>,
}

impl Output {
    fn open(source_rate: u32, source_channels: usize) -> Result<Output, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no audio output device (is PulseAudio/PipeWire running?)".to_string())?;

        // ALSA can hand back a "default" device that then refuses to describe
        // itself (a machine with no working sound card, a stale asound config, a
        // PipeWire restart mid-session). Falling back to the device's own
        // supported configs recovers the cases where a real device is there but
        // isn't the one ALSA nominated.
        let supported = match device.default_output_config() {
            Ok(config) => config,
            Err(default_error) => device
                .supported_output_configs()
                .ok()
                .and_then(|mut configs| configs.next())
                .map(|range| range.with_max_sample_rate())
                .ok_or_else(|| {
                    format!(
                        "no usable audio output device: {default_error} \
                         (on a desktop, check that PulseAudio/PipeWire is running)"
                    )
                })?,
        };

        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let out_channels = usize::from(config.channels);
        let sink = Arc::new(AudioSink::new(out_channels));

        // Record only the first error and log it once: cpal calls this per failed
        // buffer, so an unconditional eprintln! here can produce gigabytes of
        // output over a long session (observed, on a device that went away).
        let device_error = Arc::new(Mutex::new(None::<String>));
        let error_sink = Arc::clone(&device_error);
        let error_callback = move |error: cpal::StreamError| {
            let mut slot = error_sink.lock().unwrap();
            if slot.is_none() {
                eprintln!("rworldradio: audio output error: {error}");
                *slot = Some(error.to_string());
            }
        };

        let stream = build_output_stream(
            &device,
            &config,
            sample_format,
            Arc::clone(&sink),
            error_callback,
        )?;

        Ok(Output {
            stream,
            sink,
            converter: Converter::new(
                source_channels,
                source_rate,
                out_channels,
                config.sample_rate.0,
            ),
            device_error,
        })
    }

    /// The error cpal reported, if the device has failed.
    fn failure(&self) -> Option<String> {
        self.device_error.lock().unwrap().clone()
    }
}

/// cpal hands the callback the device's native sample type, so each supported
/// format gets its own conversion from the f32 mix the sink holds.
fn build_output_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    sink: Arc<AudioSink>,
    error_callback: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, String> {
    use cpal::SampleFormat;

    // Scratch buffer reused across callbacks so the non-f32 paths don't allocate
    // on the audio thread.
    let mut scratch: Vec<f32> = Vec::new();

    let stream = match sample_format {
        SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                sink.fill(data);
                // Lossy codecs can decode slightly past full scale; ALSA passes
                // f32 straight through, so clamp here rather than let the driver
                // wrap it into a click.
                for sample in data.iter_mut() {
                    *sample = sample.clamp(-1.0, 1.0);
                }
            },
            error_callback,
            None,
        ),
        SampleFormat::I16 => device.build_output_stream(
            config,
            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                scratch.resize(data.len(), 0.0);
                sink.fill(&mut scratch);
                for (out, sample) in data.iter_mut().zip(scratch.iter()) {
                    *out = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                }
            },
            error_callback,
            None,
        ),
        SampleFormat::U16 => device.build_output_stream(
            config,
            move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                scratch.resize(data.len(), 0.0);
                sink.fill(&mut scratch);
                for (out, sample) in data.iter_mut().zip(scratch.iter()) {
                    let scaled = (sample.clamp(-1.0, 1.0) * 0.5 + 0.5) * u16::MAX as f32;
                    *out = scaled as u16;
                }
            },
            error_callback,
            None,
        ),
        SampleFormat::I32 => device.build_output_stream(
            config,
            move |data: &mut [i32], _: &cpal::OutputCallbackInfo| {
                scratch.resize(data.len(), 0.0);
                sink.fill(&mut scratch);
                for (out, sample) in data.iter_mut().zip(scratch.iter()) {
                    *out = (f64::from(sample.clamp(-1.0, 1.0)) * f64::from(i32::MAX)) as i32;
                }
            },
            error_callback,
            None,
        ),
        other => return Err(format!("unsupported audio sample format: {other:?}")),
    };

    stream.map_err(|error| format!("could not open audio output: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_hls_urls_including_query_strings() {
        assert!(is_hls_url("https://host/live/playlist.m3u8"));
        assert!(is_hls_url("https://host/live/PLAYLIST.M3U8?token=abc"));
        assert!(!is_hls_url("https://host/stream.mp3"));
        assert!(!is_hls_url("https://host/stream"));
        assert!(!is_hls_url("https://host/list.m3u"));
    }

    #[test]
    fn playlist_bodies_are_recognized_whatever_the_url_said() {
        assert!(looks_like_playlist(b"#EXTM3U\n#EXT-X-VER", None));
        assert!(looks_like_playlist(b"[playlist]\nFile1=h", None));
        assert!(looks_like_playlist(b"[PLAYLIST]\r\nnumbe", None));
        // Bare URL list, no header, and a useless content type.
        assert!(looks_like_playlist(
            b"http://dispatcher.rn",
            Some("application/octet-stream")
        ));
        assert!(looks_like_playlist(
            &[0x00; 16],
            Some("application/vnd.apple.mpegurl")
        ));
        assert!(looks_like_playlist(&[0x00; 16], Some("audio/x-scpls; charset=utf-8")));

        // Actual audio must not be mistaken for a playlist.
        assert!(!looks_like_playlist(&[0xFF, 0xF9, 0x50, 0x80], Some("audio/aac")));
        assert!(!looks_like_playlist(b"ID3\x04\x00\x00\x00\x00", Some("audio/mpeg")));
        assert!(!looks_like_playlist(&[], None));
    }

    #[test]
    fn extracts_the_stream_url_from_legacy_playlists() {
        let m3u = "#EXTM3U\n#EXTINF:-1,Station\nhttp://host.example/stream.mp3\n";
        assert_eq!(
            first_stream_url(m3u).unwrap(),
            "http://host.example/stream.mp3"
        );

        let pls = "[playlist]\nnumberofentries=1\nFile1=https://host.example/live\n\
                   Title1=Station\nLength1=-1\n";
        assert_eq!(first_stream_url(pls).unwrap(), "https://host.example/live");

        // A bare URL list with no header at all.
        assert_eq!(
            first_stream_url("\n\nhttp://a.example/x\nhttp://b.example/y\n").unwrap(),
            "http://a.example/x"
        );

        assert!(first_stream_url("[playlist]\nnumberofentries=0\n").is_none());
        assert!(first_stream_url("").is_none());
    }

    #[test]
    fn tunein_response_is_reduced_to_its_url() {
        assert_eq!(first_line("http://example/stream\r\n"), "http://example/stream");
        assert_eq!(first_line("\n  http://example/s  \nignored"), "http://example/s");
        assert_eq!(first_line(""), "");
    }

}
