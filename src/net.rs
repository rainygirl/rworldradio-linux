//! HTTP for the two things this app needs it for: small one-shot GETs (TuneIn
//! resolver links, HLS playlists and segments) and one long-lived streaming GET
//! (an icecast-style progressive audio body).
//!
//! `ureq` with its default rustls backend keeps this pure Rust - no OpenSSL
//! development package needed to build, which is the point of the port. This
//! replaces the Haiku build's `NetworkFetch` (BUrlProtocolRoster) and the
//! implicit network I/O that `BMediaFile(BUrl)` did via Haiku's http_streamer
//! add-on: on Linux there is no equivalent, so the app fetches the bytes itself
//! and hands them to symphonia.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::byte_queue::ByteQueue;

const USER_AGENT: &str = concat!("RWorldRadio/", env!("CARGO_PKG_VERSION"));

fn agent(read_timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(read_timeout)
        .user_agent(USER_AGENT)
        .build()
}

fn describe(url: &str, error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(status, _) => format!("HTTP status {status} for {url}"),
        ureq::Error::Transport(transport) => format!("{transport}"),
    }
}

/// Blocking GET returning the whole body as text. Used for playlists and the
/// TuneIn Tune.ashx resolver response.
pub fn get_text(url: &str) -> Result<String, String> {
    let response = agent(Duration::from_secs(15))
        .get(url)
        .call()
        .map_err(|e| describe(url, e))?;
    // 8 MB ceiling: a playlist or resolver response is a few kB at most, so
    // anything near this is a wrong URL rather than something worth buffering.
    let mut body = String::new();
    response
        .into_reader()
        .take(8 * 1024 * 1024)
        .read_to_string(&mut body)
        .map_err(|e| format!("{url}: {e}"))?;
    if body.is_empty() {
        return Err(format!("empty response body from {url}"));
    }
    Ok(body)
}

/// Blocking GET returning the whole body as bytes. Used for HLS segments.
pub fn get_bytes(url: &str) -> Result<Vec<u8>, String> {
    let response = agent(Duration::from_secs(20))
        .get(url)
        .call()
        .map_err(|e| describe(url, e))?;
    let mut body = Vec::new();
    response
        .into_reader()
        // A single HLS audio segment is well under a megabyte; 32 MB is a
        // runaway guard, not a real limit.
        .take(32 * 1024 * 1024)
        .read_to_end(&mut body)
        .map_err(|e| format!("{url}: {e}"))?;
    if body.is_empty() {
        return Err(format!("empty response body from {url}"));
    }
    Ok(body)
}

/// Result of opening a progressive stream: the queue the body is being written
/// into, plus whatever the server said the content type was (used as a decoder
/// hint).
pub struct OpenedStream {
    pub queue: Arc<ByteQueue>,
    pub content_type: Option<String>,
    /// The URL actually streamed from, after redirects.
    pub final_url: String,
}

/// Starts a streaming GET on a worker thread, writing the response body into a
/// `ByteQueue`. Returns once the response *headers* are in, so the caller can
/// read the content type before any audio arrives.
///
/// The worker stops as soon as `stop` is set or the queue is closed; the caller
/// closing the queue is what makes stopping playback immediate rather than
/// waiting on a socket read.
pub fn open_stream(url: &str, stop: Arc<AtomicBool>) -> Result<OpenedStream, String> {
    let response = agent(Duration::from_secs(20))
        .get(url)
        .call()
        .map_err(|e| describe(url, e))?;

    let content_type = response.header("Content-Type").map(|s| s.to_string());
    let final_url = response.get_url().to_string();

    let queue = ByteQueue::new();
    let producer = Arc::clone(&queue);
    let mut reader = response.into_reader();

    thread::Builder::new()
        .name("radio-http".into())
        .spawn(move || {
            let mut chunk = vec![0u8; 32 * 1024];
            loop {
                if stop.load(Ordering::Relaxed) {
                    producer.close(None);
                    return;
                }
                match reader.read(&mut chunk) {
                    Ok(0) => {
                        producer.close(None); // server closed the stream
                        return;
                    }
                    Ok(count) => {
                        if !producer.push(&chunk[..count]) {
                            return; // consumer went away
                        }
                    }
                    Err(error) => {
                        // A stop closes the queue first, so don't report the
                        // read error that inevitably follows it as a failure.
                        if stop.load(Ordering::Relaxed) || producer.is_closed() {
                            producer.close(None);
                        } else {
                            producer.close(Some(format!("stream read failed: {error}")));
                        }
                        return;
                    }
                }
            }
        })
        .map_err(|e| format!("could not start stream thread: {e}"))?;

    Ok(OpenedStream {
        queue,
        content_type,
        final_url,
    })
}
