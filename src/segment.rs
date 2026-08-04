//! Turns one downloaded HLS segment into a raw elementary audio stream,
//! whichever of the two segment formats it happens to be.
//!
//! The Haiku build assumed every segment was MPEG-TS and ran it through
//! `TsDemuxer`. That covers the common case, but the HLS spec also allows
//! "packed audio" segments - a bare ADTS AAC, MPEG audio or similar bitstream
//! with no TS wrapper - and real stations use them (qingting.fm serves `.aac`
//! segments this way). Those need no demuxing at all, just their per-segment ID3
//! tag stripped, so a stream that used to fail with "segment produced no audio
//! elementary stream" now plays.

use crate::ts_demuxer::{self, AudioCodec};

/// Extracts the audio elementary stream from a segment, or `None` if there is no
/// recognizable audio in it.
pub fn elementary_stream(data: &[u8], uri: &str) -> Option<(Vec<u8>, AudioCodec)> {
    // Strip the ID3v2 tag HLS requires on each packed-audio segment (it carries
    // the PRIV timestamp). A TS segment starts with 0x47, so this is a no-op for
    // those.
    let audio = strip_id3(data);
    if audio.is_empty() {
        return None;
    }

    // A packed-audio segment opens with a codec syncword, and MPEG-TS always
    // opens with 0x47 - so when a syncword is present there's no ambiguity, and
    // this is checked before the (necessarily heuristic) TS detection.
    if let Some(codec) = syncword_codec(audio) {
        return Some((audio.to_vec(), codec));
    }

    if looks_like_mpeg_ts(data) {
        let demuxed = ts_demuxer::extract(data);
        if !demuxed.elementary_stream.is_empty() {
            return Some((demuxed.elementary_stream, demuxed.codec));
        }
        // A TS container we couldn't get audio out of: encrypted, video-only, or
        // a codec the PMT parser doesn't recognize. Don't fall through to the
        // packed-audio path - TS bytes are not an elementary stream.
        return None;
    }

    // Neither a syncword nor TS: we probably joined a segment mid-frame. Trust
    // the segment's extension and let symphonia resynchronize.
    let codec = extension_codec(uri)?;
    Some((audio.to_vec(), codec))
}

/// MPEG-TS is a stream of 188-byte packets each starting with 0x47. Checking two
/// consecutive sync bytes at the right stride avoids mistaking an ADTS frame that
/// happens to contain a 0x47 for a transport stream.
fn looks_like_mpeg_ts(data: &[u8]) -> bool {
    const PACKET_SIZE: usize = 188;
    // Allow a little junk before the first packet, the way the demuxer's resync
    // does, but require the 188-byte stride to hold once found.
    for start in 0..data.len().min(PACKET_SIZE).max(1) {
        if data.get(start) != Some(&0x47) {
            continue;
        }
        if data.get(start + PACKET_SIZE) == Some(&0x47) {
            return true;
        }
    }
    false
}

/// Skips any number of leading ID3v2 tags.
fn strip_id3(data: &[u8]) -> &[u8] {
    let mut rest = data;
    while rest.len() >= 10 && &rest[..3] == b"ID3" {
        let flags = rest[5];
        // Size is four syncsafe bytes: 7 significant bits each.
        let size = rest[6..10]
            .iter()
            .fold(0usize, |total, byte| (total << 7) | usize::from(byte & 0x7F));
        let footer = if flags & 0x10 != 0 { 10 } else { 0 };
        let total = 10 + size + footer;
        if total >= rest.len() {
            return &[];
        }
        rest = &rest[total..];
    }
    rest
}

/// Identifies a packed-audio bitstream from its leading frame header.
fn syncword_codec(audio: &[u8]) -> Option<AudioCodec> {
    if audio.len() < 2 || audio[0] != 0xFF {
        return None;
    }
    // ADTS: syncword 0xFFF, layer bits (0x06) must be zero.
    if audio[1] & 0xF6 == 0xF0 {
        return Some(AudioCodec::AdtsAac);
    }
    // MPEG audio frame: 11 sync bits, then a non-reserved layer.
    if audio[1] & 0xE0 == 0xE0 && audio[1] & 0x06 != 0x00 {
        return Some(AudioCodec::MpegAudio);
    }
    None
}

fn extension_codec(uri: &str) -> Option<AudioCodec> {
    let path = uri.split('?').next().unwrap_or(uri).to_ascii_lowercase();
    if path.ends_with(".aac") {
        return Some(AudioCodec::AdtsAac);
    }
    if path.ends_with(".mp3") {
        return Some(AudioCodec::MpegAudio);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adts_frame() -> Vec<u8> {
        // ADTS header: syncword 0xFFF, MPEG-4, no CRC, AAC-LC, 44.1kHz, stereo.
        vec![0xFF, 0xF1, 0x50, 0x80, 0x01, 0xFF, 0xFC, 0x00, 0x11, 0x22]
    }

    fn id3v2_tag(payload_len: usize) -> Vec<u8> {
        let mut tag = vec![b'I', b'D', b'3', 0x04, 0x00, 0x00];
        // syncsafe length
        tag.push(((payload_len >> 21) & 0x7F) as u8);
        tag.push(((payload_len >> 14) & 0x7F) as u8);
        tag.push(((payload_len >> 7) & 0x7F) as u8);
        tag.push((payload_len & 0x7F) as u8);
        tag.extend(std::iter::repeat(0u8).take(payload_len));
        tag
    }

    #[test]
    fn packed_adts_segment_passes_through() {
        let segment = adts_frame();
        let (stream, codec) = elementary_stream(&segment, "http://h/a/b.aac").unwrap();
        assert_eq!(codec, AudioCodec::AdtsAac);
        assert_eq!(stream, segment);
    }

    #[test]
    fn leading_id3_tag_is_stripped() {
        let mut segment = id3v2_tag(64);
        segment.extend(adts_frame());
        let (stream, codec) = elementary_stream(&segment, "http://h/a/b.aac").unwrap();
        assert_eq!(codec, AudioCodec::AdtsAac);
        assert_eq!(stream, adts_frame(), "ID3 tag was not removed");
    }

    #[test]
    fn stacked_id3_tags_are_all_stripped() {
        let mut segment = id3v2_tag(16);
        segment.extend(id3v2_tag(8));
        segment.extend(adts_frame());
        let (stream, _) = elementary_stream(&segment, "http://h/a/b.aac").unwrap();
        assert_eq!(stream, adts_frame());
    }

    #[test]
    fn mpeg_audio_syncword_is_recognized() {
        // 0xFFFB: MPEG-1 Layer III.
        let segment = vec![0xFF, 0xFB, 0x90, 0x00, 0x01, 0x02];
        let (_, codec) = elementary_stream(&segment, "http://h/a/b.ts").unwrap();
        assert_eq!(codec, AudioCodec::MpegAudio);
    }

    #[test]
    fn extension_is_the_fallback_when_joined_mid_frame() {
        let segment = vec![0x12, 0x34, 0x56, 0x78];
        let (_, codec) = elementary_stream(&segment, "http://h/a/b.aac?token=1").unwrap();
        assert_eq!(codec, AudioCodec::AdtsAac);
        assert!(elementary_stream(&segment, "http://h/a/b.bin").is_none());
    }

    #[test]
    fn mpeg_ts_still_goes_through_the_demuxer() {
        // Two sync bytes at a 188 stride is enough to be treated as TS; with no
        // parseable PAT/PMT the result is None rather than raw TS bytes.
        let mut segment = vec![0x00u8; 400];
        segment[0] = 0x47;
        segment[188] = 0x47;
        assert!(looks_like_mpeg_ts(&segment));
        assert!(elementary_stream(&segment, "http://h/a/b.ts").is_none());
    }

    #[test]
    fn an_adts_frame_full_of_0x47_bytes_is_still_read_as_aac() {
        // Compressed audio can contain any byte pattern, including one that
        // satisfies the 188-byte TS stride heuristic. The leading syncword is
        // checked first precisely so that can't hijack the segment.
        let mut segment = adts_frame();
        segment.resize(400, 0x47);
        assert!(looks_like_mpeg_ts(&segment), "fixture no longer exercises the clash");
        let (_, codec) = elementary_stream(&segment, "http://h/a/b.aac").unwrap();
        assert_eq!(codec, AudioCodec::AdtsAac);
    }

    #[test]
    fn truncated_id3_tag_yields_nothing_rather_than_panicking() {
        let segment = id3v2_tag(1000); // claims more payload than it has
        assert!(elementary_stream(&segment, "http://h/a/b.aac").is_none());
        assert!(elementary_stream(&[], "http://h/a/b.aac").is_none());
    }
}
