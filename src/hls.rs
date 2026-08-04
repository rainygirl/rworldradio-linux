//! Bridges a live HLS audio stream (radio-browser's BBC-style .m3u8 entries)
//! into the decoder: fetches/refreshes the playlist, downloads new segments,
//! demuxes each via `ts_demuxer`, and writes the resulting elementary stream
//! into a `ByteQueue` that symphonia reads as a plain byte stream.
//!
//! This is the Haiku build's `HlsAdapterIO` (a `BAdapterIO` subclass feeding
//! `BMediaFile`) with the Haiku-specific plumbing removed - the worker loop,
//! the "block until the first segment decoded, then report success or a useful
//! reason" open semantics, and the sliding-window bookkeeping are the same.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::byte_queue::ByteQueue;
use crate::m3u8;
use crate::net;
use crate::segment;
use crate::ts_demuxer::AudioCodec;

/// How long `open` waits for the first decoded segment before giving up.
/// Matches the spirit of Haiku's own HTTPMediaIO (10s for a single HTTP
/// response); HLS involves an extra playlist round-trip first, so this is a bit
/// more generous.
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);

/// A live sliding-window playlist won't repeat old segment URIs, so the
/// seen-set can be dropped periodically rather than growing forever across a
/// multi-hour listen.
const MAX_SEEN_SEGMENTS: usize = 500;

pub struct HlsStream {
    pub queue: Arc<ByteQueue>,
    /// Codec of the elementary stream, from the PMT - used as a decoder hint so
    /// symphonia doesn't have to sniff a raw ADTS/MPEG-audio stream blind.
    pub codec: AudioCodec,
}

/// Shared between `open` and the worker: the worker publishes the codec it found
/// and the last thing that went wrong, so a failed open can explain itself.
struct WorkerState {
    codec: AudioCodec,
    last_issue: String,
}

/// Starts the HLS worker and blocks until it has written the first segment's
/// audio (success) or determined the stream is unusable (error) - the same
/// contract as `HlsAdapterIO::Open()`.
pub fn open(playlist_url: &str, stop: Arc<AtomicBool>) -> Result<HlsStream, String> {
    let queue = ByteQueue::new();
    let state = Arc::new(Mutex::new(WorkerState {
        codec: AudioCodec::Unknown,
        last_issue: "no segments were listed in the media playlist".to_string(),
    }));

    let worker_queue = Arc::clone(&queue);
    let worker_state = Arc::clone(&state);
    let worker_url = playlist_url.to_string();
    let worker_stop = Arc::clone(&stop);

    thread::Builder::new()
        .name("radio-hls".into())
        .spawn(move || run_worker(&worker_url, worker_queue, worker_state, worker_stop))
        .map_err(|e| format!("could not start HLS worker: {e}"))?;

    if queue.wait_for_data(OPEN_TIMEOUT) {
        let codec = state.lock().unwrap().codec;
        return Ok(HlsStream { queue, codec });
    }

    // No data within the timeout. Either the worker gave up (queue closed, and
    // last_issue says why) or it is still spinning on a playlist that never
    // produces anything - close it either way so the worker exits.
    let issue = state.lock().unwrap().last_issue.clone();
    queue.close(None);
    stop.store(true, Ordering::Relaxed);
    Err(issue)
}

fn run_worker(
    playlist_url: &str,
    queue: Arc<ByteQueue>,
    state: Arc<Mutex<WorkerState>>,
    stop: Arc<AtomicBool>,
) {
    let set_issue = |issue: String| {
        state.lock().unwrap().last_issue = issue;
    };
    let stopped = || stop.load(Ordering::Relaxed) || queue.is_closed();

    let mut media_playlist_url = playlist_url.to_string();

    let initial = match net::get_text(playlist_url) {
        Ok(body) => body,
        Err(error) => {
            let issue = format!("could not fetch playlist: {error}");
            set_issue(issue.clone());
            queue.close(Some(issue));
            return;
        }
    };

    // If this is a master playlist, pick the highest-bandwidth variant -
    // internet radio bitrates are all small enough that "best available" isn't a
    // real bandwidth concern for anyone able to stream at all.
    if m3u8::is_master_playlist(&initial) {
        let variants = m3u8::parse_master_playlist(&initial, playlist_url);
        match variants.iter().max_by_key(|variant| variant.bandwidth) {
            Some(best) => media_playlist_url = best.uri.clone(),
            None => {
                let issue = "master playlist has no variants".to_string();
                set_issue(issue.clone());
                queue.close(Some(issue));
                return;
            }
        }
    }

    let mut seen_segments: HashSet<String> = HashSet::new();
    let mut wrote_anything = false;

    while !stopped() {
        let playlist_body = match net::get_text(&media_playlist_url) {
            Ok(body) => body,
            Err(error) => {
                let issue = format!("could not fetch media playlist: {error}");
                set_issue(issue.clone());
                if !wrote_anything {
                    // Nothing has played yet, so this is a hard failure - don't
                    // leave `open` waiting out its whole timeout.
                    queue.close(Some(issue));
                    return;
                }
                sleep_interruptibly(Duration::from_secs(2), &stop);
                continue;
            }
        };

        let playlist = m3u8::parse_media_playlist(&playlist_body, &media_playlist_url);

        let mut wrote_this_round = false;
        for segment in &playlist.segments {
            if stopped() {
                break;
            }
            if !seen_segments.insert(segment.uri.clone()) {
                continue;
            }

            let segment_data = match net::get_bytes(&segment.uri) {
                Ok(data) => data,
                Err(error) => {
                    set_issue(format!("segment fetch failed: {error}"));
                    continue;
                }
            };

            let (elementary_stream, codec) =
                match segment::elementary_stream(&segment_data, &segment.uri) {
                    Some(extracted) => extracted,
                    None => {
                        set_issue(
                            "segment produced no audio elementary stream \
                             (unsupported codec/container?)"
                                .to_string(),
                        );
                        continue;
                    }
                };

            if codec != AudioCodec::Unknown {
                state.lock().unwrap().codec = codec;
            }
            if !queue.push(&elementary_stream) {
                return; // consumer went away
            }
            wrote_this_round = true;
            wrote_anything = true;
        }

        if playlist.is_end_list || stopped() {
            break;
        }

        if !wrote_this_round {
            // Nothing new this round - wait roughly a target-duration before
            // asking again, matching standard HLS live polling.
            let poll_seconds = if playlist.target_duration > 0.0 {
                playlist.target_duration
            } else {
                // No #EXT-X-TARGETDURATION (non-conforming, but it happens):
                // fall back to the last segment's own duration, then to 3s.
                playlist
                    .segments
                    .last()
                    .map(|segment| segment.duration)
                    .filter(|duration| *duration > 0.0)
                    .unwrap_or(3.0)
            };
            let wait = Duration::from_secs_f64(poll_seconds);
            sleep_interruptibly(wait, &stop);
        }

        if seen_segments.len() > MAX_SEEN_SEGMENTS {
            seen_segments.clear();
        }
    }

    if wrote_anything {
        queue.close(None);
    } else {
        let issue = state.lock().unwrap().last_issue.clone();
        queue.close(Some(issue));
    }
}

/// Sleeps in short slices so a stop request doesn't have to wait out a full
/// target-duration poll interval.
fn sleep_interruptibly(total: Duration, stop: &AtomicBool) {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}
