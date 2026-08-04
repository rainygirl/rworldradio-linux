//! A bounded producer/consumer byte pipe with an explicit close, and a
//! `symphonia::MediaSource` reader over it.
//!
//! This replaces the Haiku build's `HlsAdapterIO` (a `BAdapterIO` subclass that
//! fed `BInputAdapter`): a network thread pushes bytes in, the decoder thread
//! reads them out as a plain byte stream, and the bound gives the producer
//! backpressure so a live stream doesn't buffer without limit. Closing is what
//! makes `stop` responsive - a blocked reader wakes up and sees EOF instead of
//! waiting for a socket timeout.

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use symphonia::core::io::MediaSource;

/// ~2 MB of undecoded audio: several seconds even for a 320kbps stream, which
/// is plenty of slack for an HLS segment fetch without letting a stalled
/// decoder grow the buffer unbounded.
const DEFAULT_CAPACITY: usize = 2 * 1024 * 1024;

struct Inner {
    buf: VecDeque<u8>,
    closed: bool,
    error: Option<String>,
}

pub struct ByteQueue {
    inner: Mutex<Inner>,
    signal: Condvar,
    capacity: usize,
}

impl ByteQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(ByteQueue {
            inner: Mutex::new(Inner {
                buf: VecDeque::new(),
                closed: false,
                error: None,
            }),
            signal: Condvar::new(),
            capacity: DEFAULT_CAPACITY,
        })
    }

    /// Appends `data`, blocking while the queue is over capacity. Returns false
    /// once the queue is closed, which is the producer's signal to stop.
    pub fn push(&self, data: &[u8]) -> bool {
        let mut inner = self.inner.lock().unwrap();
        while !inner.closed && inner.buf.len() >= self.capacity {
            inner = self.signal.wait(inner).unwrap();
        }
        if inner.closed {
            return false;
        }
        inner.buf.extend(data.iter().copied());
        self.signal.notify_all();
        true
    }

    /// Marks the stream finished. `error` is reported to the reader as an I/O
    /// error instead of a clean EOF; `None` means a normal end of stream.
    pub fn close(&self, error: Option<String>) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.closed {
            inner.closed = true;
            inner.error = error;
        }
        self.signal.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().unwrap().closed
    }

    /// Returns up to `count` leading bytes **without consuming them**, waiting up
    /// to `timeout` for that many to arrive. Used to identify a stream's codec
    /// from its first frame header before handing it to a decoder.
    pub fn peek(&self, count: usize, timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut inner = self.inner.lock().unwrap();
        while inner.buf.len() < count && !inner.closed {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let (guard, _) = self.signal.wait_timeout(inner, remaining).unwrap();
            inner = guard;
        }
        inner.buf.iter().take(count).copied().collect()
    }

    /// Blocks until at least one byte of data is available, the queue is closed,
    /// or `timeout` elapses.
    pub fn wait_for_data(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut inner = self.inner.lock().unwrap();
        loop {
            if !inner.buf.is_empty() {
                return true;
            }
            if inner.closed {
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (guard, _) = self.signal.wait_timeout(inner, remaining).unwrap();
            inner = guard;
        }
    }

    fn read(&self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut inner = self.inner.lock().unwrap();
        while inner.buf.is_empty() {
            if inner.closed {
                return match inner.error.take() {
                    Some(error) => Err(io::Error::new(io::ErrorKind::Other, error)),
                    None => Ok(0), // clean EOF
                };
            }
            inner = self.signal.wait(inner).unwrap();
        }
        let count = out.len().min(inner.buf.len());
        for slot in out.iter_mut().take(count) {
            *slot = inner.buf.pop_front().unwrap();
        }
        self.signal.notify_all();
        Ok(count)
    }
}

/// `Read`/`MediaSource` view of a `ByteQueue`, handed to symphonia.
pub struct QueueReader {
    queue: Arc<ByteQueue>,
}

impl QueueReader {
    pub fn new(queue: Arc<ByteQueue>) -> Self {
        QueueReader { queue }
    }
}

impl Read for QueueReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.queue.read(buf)
    }
}

impl Seek for QueueReader {
    fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "radio streams are not seekable",
        ))
    }
}

impl MediaSource for QueueReader {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn reads_pushed_bytes_then_sees_clean_eof() {
        let queue = ByteQueue::new();
        let producer = Arc::clone(&queue);
        let handle = thread::spawn(move || {
            assert!(producer.push(b"hello "));
            assert!(producer.push(b"world"));
            producer.close(None);
        });

        let mut reader = QueueReader::new(Arc::clone(&queue));
        let mut out = String::new();
        reader.read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello world");
        handle.join().unwrap();
    }

    #[test]
    fn close_with_error_surfaces_as_io_error_after_draining() {
        let queue = ByteQueue::new();
        queue.push(b"partial");
        queue.close(Some("segment fetch failed".into()));

        let mut reader = QueueReader::new(Arc::clone(&queue));
        let mut buf = [0u8; 32];
        assert_eq!(reader.read(&mut buf).unwrap(), 7);
        let err = reader.read(&mut buf).unwrap_err();
        assert!(err.to_string().contains("segment fetch failed"));
    }

    #[test]
    fn peek_does_not_consume() {
        let queue = ByteQueue::new();
        queue.push(b"\xff\xf9rest");
        assert_eq!(
            queue.peek(2, Duration::from_millis(50)),
            vec![0xFF, 0xF9],
            "peek must see the leading bytes"
        );
        queue.close(None);

        let mut reader = QueueReader::new(Arc::clone(&queue));
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"\xff\xf9rest", "peeked bytes must still be readable");
    }

    #[test]
    fn peek_returns_what_it_has_when_the_stream_is_short() {
        let queue = ByteQueue::new();
        queue.push(b"\xff");
        queue.close(None);
        assert_eq!(queue.peek(4, Duration::from_millis(50)), vec![0xFF]);
    }

    #[test]
    fn push_after_close_is_rejected() {
        let queue = ByteQueue::new();
        queue.close(None);
        assert!(!queue.push(b"ignored"));
    }
}
