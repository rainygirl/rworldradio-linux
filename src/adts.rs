//! Normalizes an ADTS AAC bitstream into the one shape symphonia can read.
//!
//! symphonia 0.5's `AdtsReader` syncs by scanning for the exact 16-bit pattern
//! `0xFFF1` - MPEG-4 ADTS with `protection_absent = 1`. Two other perfectly
//! valid and common variants never sync at all, and because a radio stream never
//! ends, "never syncs" means the reader blocks forever rather than failing:
//!
//! * `0xFFF9` - MPEG-2 ADTS. Only the `ID` bit differs; the fixed header layout
//!   and the profile numbering are identical, so clearing that bit yields an
//!   equivalent MPEG-4 frame. (qingting.fm's `.aac` HLS segments are this.)
//! * `0xFFF0`/`0xFFF8` - CRC-protected ADTS: a 9-byte header whose extra 2 bytes
//!   are a CRC over the frame. Dropping those two bytes, setting
//!   `protection_absent` and subtracting 2 from `aac_frame_length` yields the
//!   unprotected form of the same frame.
//!
//! Both rewrites are mechanical and lossless as far as the decoder is concerned -
//! the AAC payload is untouched. This walks the frame chain rather than
//! bit-twiddling on byte matches, so a `0xFFF9` pair occurring inside compressed
//! payload data can't be mistaken for a header.

use std::io::{self, Read, Seek, SeekFrom};

use symphonia::core::io::MediaSource;

const HEADER_SIZE: usize = 7;
/// ADTS frame lengths are 13 bits, so no frame can exceed this.
const MAX_FRAME_LEN: usize = 8191;

/// True if `bytes` starts with any ADTS syncword (12 sync bits, layer == 0).
fn is_sync(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] & 0xF6 == 0xF0
}

fn frame_length(header: &[u8]) -> usize {
    (usize::from(header[3] & 0x03) << 11)
        | (usize::from(header[4]) << 3)
        | (usize::from(header[5] & 0xE0) >> 5)
}

fn set_frame_length(header: &mut [u8], length: usize) {
    header[3] = (header[3] & 0xFC) | ((length >> 11) & 0x03) as u8;
    header[4] = ((length >> 3) & 0xFF) as u8;
    header[5] = (header[5] & 0x1F) | (((length & 0x07) << 5) as u8);
}

/// Rewrites one frame in place (or in `out`) so symphonia can sync on it.
/// `frame` must start at a syncword and hold the whole frame.
fn normalize_frame(frame: &[u8], out: &mut Vec<u8>) {
    let crc_present = frame[1] & 0x01 == 0;
    let header_size = if crc_present { HEADER_SIZE + 2 } else { HEADER_SIZE };
    if frame.len() < header_size {
        // Can't strip a CRC we don't fully have; pass the frame through as-is.
        out.extend_from_slice(frame);
        return;
    }

    let start = out.len();
    out.extend_from_slice(&frame[..HEADER_SIZE]);
    out.extend_from_slice(&frame[header_size..]);

    let header = &mut out[start..start + HEADER_SIZE];
    header[1] &= !0x08; // ID: MPEG-2 -> MPEG-4
    header[1] |= 0x01; // protection_absent
    if crc_present {
        let length = frame.len() - 2;
        set_frame_length(header, length);
    }
}

/// A `Read`/`MediaSource` adapter that emits a normalized ADTS stream.
pub struct AdtsNormalizer<R: Read> {
    inner: R,
    /// Bytes read from `inner` but not yet consumed as a complete frame.
    pending: Vec<u8>,
    /// Normalized bytes waiting to be handed out.
    ready: Vec<u8>,
    ready_position: usize,
    synced: bool,
    eof: bool,
}

impl<R: Read> AdtsNormalizer<R> {
    pub fn new(inner: R) -> Self {
        AdtsNormalizer {
            inner,
            pending: Vec::new(),
            ready: Vec::new(),
            ready_position: 0,
            synced: false,
            eof: false,
        }
    }

    /// Reads more bytes from `inner` into `pending`. Returns false at EOF.
    fn fill(&mut self, at_least: usize) -> io::Result<bool> {
        let mut chunk = [0u8; 8 * 1024];
        while self.pending.len() < at_least {
            if self.eof {
                return Ok(false);
            }
            let count = self.inner.read(&mut chunk)?;
            if count == 0 {
                self.eof = true;
                return Ok(false);
            }
            self.pending.extend_from_slice(&chunk[..count]);
        }
        Ok(true)
    }

    /// Drops bytes until `pending` starts at a syncword.
    fn resync(&mut self) -> io::Result<bool> {
        loop {
            if !self.fill(2)? {
                return Ok(false);
            }
            match self.pending.windows(2).position(|pair| is_sync(pair)) {
                Some(0) => return Ok(true),
                Some(offset) => {
                    self.pending.drain(..offset);
                    return Ok(true);
                }
                None => {
                    // Keep the last byte: the syncword may straddle the boundary.
                    let keep = self.pending.len() - 1;
                    self.pending.drain(..keep);
                    if !self.fill(self.pending.len() + 1)? {
                        return Ok(false);
                    }
                }
            }
        }
    }

    /// Moves exactly one normalized frame into `ready`. Returns false at EOF.
    fn next_frame(&mut self) -> io::Result<bool> {
        loop {
            if !self.synced {
                if !self.resync()? {
                    return Ok(false);
                }
                self.synced = true;
            }

            if !self.fill(HEADER_SIZE)? {
                return Ok(false);
            }
            if !is_sync(&self.pending) {
                self.synced = false; // lost sync; search again
                continue;
            }

            let length = frame_length(&self.pending);
            if length < HEADER_SIZE || length > MAX_FRAME_LEN {
                // Not a real header after all - skip a byte and re-search.
                self.pending.drain(..1);
                self.synced = false;
                continue;
            }
            if !self.fill(length)? {
                return Ok(false);
            }

            self.ready.clear();
            self.ready_position = 0;
            normalize_frame(&self.pending[..length], &mut self.ready);
            self.pending.drain(..length);

            // A valid stream has the next frame's syncword immediately after this
            // one. If it doesn't, emit this frame anyway but resync for the next.
            if self.pending.len() >= 2 && !is_sync(&self.pending) {
                self.synced = false;
            }
            return Ok(true);
        }
    }
}

impl<R: Read> Read for AdtsNormalizer<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.ready_position >= self.ready.len() && !self.next_frame()? {
            return Ok(0); // EOF
        }
        let available = &self.ready[self.ready_position..];
        let count = out.len().min(available.len());
        out[..count].copy_from_slice(&available[..count]);
        self.ready_position += count;
        Ok(count)
    }
}

impl<R: Read> Seek for AdtsNormalizer<R> {
    fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "radio streams are not seekable",
        ))
    }
}

impl<R: Read + Send + Sync> MediaSource for AdtsNormalizer<R> {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// One-shot form for a complete buffer (used by the segment path's tests and by
/// callers that already hold the whole stream).
pub fn normalize_buffer(data: &[u8]) -> Vec<u8> {
    let mut normalizer = AdtsNormalizer::new(data);
    let mut out = Vec::with_capacity(data.len());
    // Reading from a &[u8] can't fail.
    let _ = normalizer.read_to_end(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one ADTS frame. `id` 1 = MPEG-2, 0 = MPEG-4; `crc` adds the 2-byte
    /// CRC and the 9-byte header form. Payload is `payload_len` filler bytes.
    fn frame(id: u8, crc: bool, payload_len: usize) -> Vec<u8> {
        let header_size = if crc { 9 } else { 7 };
        let total = header_size + payload_len;
        let mut bytes = vec![0u8; header_size];
        bytes[0] = 0xFF;
        bytes[1] = 0xF0 | (id << 3) | if crc { 0 } else { 1 };
        bytes[2] = 0x50; // AAC-LC, 44.1kHz
        bytes[3] = 0x80; // stereo
        set_frame_length(&mut bytes, total);
        bytes[5] |= 0x1F; // buffer fullness bits
        bytes[6] = 0x00; // one AAC frame per ADTS packet
        bytes.extend((0..payload_len).map(|i| (i % 251) as u8));
        assert_eq!(bytes.len(), total);
        bytes
    }

    fn payload_of(frame: &[u8]) -> &[u8] {
        &frame[7..]
    }

    #[test]
    fn mpeg2_frames_become_mpeg4_with_payload_intact() {
        let input = [frame(1, false, 40), frame(1, false, 60)].concat();
        let out = normalize_buffer(&input);

        assert_eq!(out.len(), input.len(), "no bytes should be added or lost");
        assert_eq!(&out[..2], &[0xFF, 0xF1], "symphonia's syncword");
        assert_eq!(frame_length(&out), 47);
        assert_eq!(payload_of(&out[..47]), payload_of(&frame(1, false, 40)));

        // Second frame follows immediately and is normalized too.
        assert_eq!(&out[47..49], &[0xFF, 0xF1]);
        assert_eq!(frame_length(&out[47..]), 67);
    }

    #[test]
    fn already_mpeg4_frames_pass_through_unchanged() {
        let input = [frame(0, false, 32), frame(0, false, 32)].concat();
        assert_eq!(normalize_buffer(&input), input);
    }

    #[test]
    fn crc_protected_frames_are_stripped_to_the_unprotected_form() {
        let input = frame(1, true, 50); // 9-byte header + 50 payload = 59
        let out = normalize_buffer(&input);

        assert_eq!(&out[..2], &[0xFF, 0xF1]);
        assert_eq!(out.len(), input.len() - 2, "the 2 CRC bytes are removed");
        assert_eq!(
            frame_length(&out),
            out.len(),
            "aac_frame_length must match the rewritten frame"
        );
        // Payload after the 7-byte header equals the input's payload after its
        // 9-byte header.
        assert_eq!(&out[7..], &input[9..]);
    }

    #[test]
    fn leading_garbage_is_skipped() {
        let mut input = vec![0x00, 0x11, 0x22, 0xFF, 0x00];
        input.extend(frame(1, false, 24));
        let out = normalize_buffer(&input);
        assert_eq!(&out[..2], &[0xFF, 0xF1]);
        assert_eq!(out.len(), 31);
    }

    #[test]
    fn a_bogus_syncword_inside_payload_does_not_derail_the_chain() {
        // Frame whose payload happens to contain 0xFF 0xF9. Frame-chain walking
        // means it is copied as payload, not treated as a header.
        let mut first = frame(1, false, 40);
        first[20] = 0xFF;
        first[21] = 0xF9;
        let input = [first.clone(), frame(1, false, 40)].concat();

        let out = normalize_buffer(&input);
        assert_eq!(out.len(), input.len());
        assert_eq!(out[20], 0xFF, "payload byte preserved");
        assert_eq!(out[21], 0xF9, "payload byte NOT rewritten");
        assert_eq!(&out[47..49], &[0xFF, 0xF1], "next real frame normalized");
    }

    #[test]
    fn resyncs_after_a_corrupt_frame() {
        let mut input = frame(1, false, 30);
        input[3] = 0x00; // frame_length now smaller than the header: invalid
        input[4] = 0x00;
        input[5] = 0x1F;
        input.extend(frame(1, false, 30));
        let out = normalize_buffer(&input);
        // The good frame is still recovered.
        assert!(out.len() >= 37);
        assert_eq!(&out[out.len() - 37..out.len() - 35], &[0xFF, 0xF1]);
    }

    #[test]
    fn works_through_tiny_reads() {
        let input = [frame(1, false, 100), frame(1, true, 100)].concat();
        let expected = normalize_buffer(&input);

        // A reader that only ever returns 3 bytes at a time, to exercise the
        // buffer-boundary paths.
        struct Trickle<'a>(&'a [u8]);
        impl Read for Trickle<'_> {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                let count = out.len().min(3).min(self.0.len());
                out[..count].copy_from_slice(&self.0[..count]);
                self.0 = &self.0[count..];
                Ok(count)
            }
        }

        let mut normalizer = AdtsNormalizer::new(Trickle(&input));
        let mut out = Vec::new();
        let mut one = [0u8; 1];
        while normalizer.read(&mut one).unwrap() == 1 {
            out.push(one[0]);
        }
        assert_eq!(out, expected);
    }

    #[test]
    fn truncated_stream_yields_only_whole_frames() {
        let mut input = frame(1, false, 40);
        input.truncate(20); // half a frame
        assert!(normalize_buffer(&input).is_empty());
        assert!(normalize_buffer(&[]).is_empty());
        assert!(normalize_buffer(&[0xFF]).is_empty());
    }
}
