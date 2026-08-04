//! Decides which symphonia `FormatReader` to open a stream with, and opens it.
//!
//! `Probe::format` is deliberately *not* the default path here. It finds a reader
//! by scanning the stream for a registered magic marker, and for a raw elementary
//! stream (ADTS AAC, MPEG audio - what internet radio actually serves) there may
//! be no marker it recognizes. On a file that ends in "end of stream"; on a live
//! radio stream, which never ends, it means scanning forever. So when the codec is
//! already known - from the HLS PMT, from the packed-audio syncword, or from the
//! HTTP content type - the matching reader is constructed directly.

use std::io::Read;

use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::adts::AdtsNormalizer;
use crate::ts_demuxer::AudioCodec;

#[derive(Debug)]
pub enum StreamFormat {
    /// Raw ADTS AAC. Goes through [`AdtsNormalizer`] first - see that module for
    /// why symphonia can't read two of the three ADTS variants directly.
    Adts,
    /// Raw MPEG-1/2 audio (MP1/MP2/MP3), the classic icecast payload.
    MpegAudio,
    /// A real container (Ogg, FLAC, MP4, WAV, ...) - let symphonia sniff it, with
    /// whatever hint we can offer.
    Container(Hint),
}

impl StreamFormat {
    /// Short name for status/diagnostic messages.
    pub fn describe(&self) -> &'static str {
        match self {
            StreamFormat::Adts => "raw ADTS AAC",
            StreamFormat::MpegAudio => "raw MPEG audio",
            StreamFormat::Container(_) => "container (sniffed)",
        }
    }
}

/// For an HLS stream, whose codec the PMT or the packed-audio syncword already
/// told us.
pub fn for_hls(codec: AudioCodec) -> StreamFormat {
    match codec {
        AudioCodec::AdtsAac => StreamFormat::Adts,
        AudioCodec::MpegAudio => StreamFormat::MpegAudio,
        AudioCodec::Unknown => StreamFormat::Container(Hint::new()),
    }
}

/// For a progressive HTTP stream. `head` is the first few bytes of the body,
/// which is the most reliable signal there is - a server's content type is
/// frequently wrong, and a URL extension is a guess.
pub fn for_http(
    head: &[u8],
    content_type: Option<&str>,
    url: &str,
    dataset_codec: &str,
) -> StreamFormat {
    if let Some(format) = from_syncword(head) {
        return format;
    }

    let mime = content_type.map(|value| {
        value
            .split(';')
            .next()
            .unwrap_or(value)
            .trim()
            .to_ascii_lowercase()
    });
    if let Some(mime) = &mime {
        match mime.as_str() {
            "audio/aac" | "audio/aacp" | "audio/x-aac" | "audio/x-hx-aac-adts" => {
                return StreamFormat::Adts
            }
            "audio/mpeg" | "audio/mp3" | "audio/mpeg3" | "audio/x-mpeg" => {
                return StreamFormat::MpegAudio
            }
            _ => {}
        }
    }

    // Not a raw stream we can name: build the best hint we can and let symphonia
    // sniff for a container.
    let mut hint = Hint::new();
    if let Some(mime) = &mime {
        if !mime.is_empty() {
            hint.mime_type(mime);
        }
        if let Some(extension) = extension_for_mime(mime) {
            hint.with_extension(extension);
            return StreamFormat::Container(hint);
        }
    }

    let path = url.split('?').next().unwrap_or(url).to_ascii_lowercase();
    for extension in ["ogg", "oga", "opus", "flac", "m4a", "mp4", "wav", "aac", "mp3"] {
        if path.ends_with(&format!(".{extension}")) {
            // These two are raw streams, not containers, even when only the URL
            // says so.
            match extension {
                "aac" => return StreamFormat::Adts,
                "mp3" => return StreamFormat::MpegAudio,
                _ => {}
            }
            hint.with_extension(extension);
            return StreamFormat::Container(hint);
        }
    }

    match dataset_codec.to_ascii_uppercase().as_str() {
        "MP3" => StreamFormat::MpegAudio,
        "AAC" | "AAC+" | "AACP" => StreamFormat::Adts,
        "OGG" | "VORBIS" | "OPUS" => {
            hint.with_extension("ogg");
            StreamFormat::Container(hint)
        }
        "FLAC" => {
            hint.with_extension("flac");
            StreamFormat::Container(hint)
        }
        _ => StreamFormat::Container(hint),
    }
}

fn extension_for_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "audio/ogg" | "application/ogg" | "audio/opus" | "audio/vorbis" => Some("ogg"),
        "audio/flac" | "audio/x-flac" => Some("flac"),
        "audio/mp4" | "audio/x-m4a" | "video/mp4" => Some("m4a"),
        "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave" => Some("wav"),
        _ => None,
    }
}

/// Identifies a raw elementary stream from its leading frame header. Shares the
/// bit patterns with the HLS packed-audio check in `segment`.
fn from_syncword(head: &[u8]) -> Option<StreamFormat> {
    // Skip an ID3v2 tag, which icecast servers sometimes prepend to an MP3 body.
    let head = if head.len() >= 3 && &head[..3] == b"ID3" {
        return Some(StreamFormat::MpegAudio);
    } else {
        head
    };
    if head.len() < 2 || head[0] != 0xFF {
        return None;
    }
    if head[1] & 0xF6 == 0xF0 {
        return Some(StreamFormat::Adts);
    }
    if head[1] & 0xE0 == 0xE0 && head[1] & 0x06 != 0x00 {
        return Some(StreamFormat::MpegAudio);
    }
    None
}

/// Opens the chosen reader over `source`.
pub fn open_reader<R>(format: StreamFormat, source: R) -> Result<Box<dyn FormatReader>, String>
where
    R: Read + MediaSource + Send + Sync + 'static,
{
    let options = FormatOptions::default();
    match format {
        StreamFormat::Adts => {
            let normalized = AdtsNormalizer::new(source);
            let stream =
                MediaSourceStream::new(Box::new(normalized), MediaSourceStreamOptions::default());
            symphonia::default::formats::AdtsReader::try_new(stream, &options)
                .map(|reader| Box::new(reader) as Box<dyn FormatReader>)
                .map_err(|error| format!("not a readable AAC stream: {error}"))
        }
        StreamFormat::MpegAudio => {
            let stream = MediaSourceStream::new(Box::new(source), MediaSourceStreamOptions::default());
            symphonia::default::formats::MpaReader::try_new(stream, &options)
                .map(|reader| Box::new(reader) as Box<dyn FormatReader>)
                .map_err(|error| format!("not a readable MPEG audio stream: {error}"))
        }
        StreamFormat::Container(hint) => {
            let stream = MediaSourceStream::new(Box::new(source), MediaSourceStreamOptions::default());
            symphonia::default::get_probe()
                .format(&hint, stream, &options, &MetadataOptions::default())
                .map(|probed| probed.format)
                .map_err(|error| format!("unsupported stream format: {error}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn describe(format: StreamFormat) -> &'static str {
        format.describe()
    }

    #[test]
    fn syncword_beats_a_wrong_content_type() {
        // Server claims MP3, body is plainly ADTS AAC.
        let format = for_http(&[0xFF, 0xF9, 0x50, 0x80], Some("audio/mpeg"), "http://h/s", "MP3");
        assert_eq!(describe(format), "raw ADTS AAC");

        // ...and the other way round.
        let format = for_http(&[0xFF, 0xFB, 0x90, 0x00], Some("audio/aac"), "http://h/s", "AAC");
        assert_eq!(describe(format), "raw MPEG audio");
    }

    #[test]
    fn id3_prefixed_body_is_treated_as_mpeg_audio() {
        let format = for_http(b"ID3\x04\x00\x00\x00\x00\x00\x20", None, "http://h/s", "");
        assert_eq!(describe(format), "raw MPEG audio");
    }

    #[test]
    fn content_type_is_used_when_the_body_gives_nothing_away() {
        for (mime, expected) in [
            ("audio/aacp; charset=utf-8", "raw ADTS AAC"),
            ("audio/mpeg", "raw MPEG audio"),
            ("application/ogg", "container (sniffed)"),
            ("audio/flac", "container (sniffed)"),
        ] {
            let format = for_http(&[0x00, 0x01], Some(mime), "http://h/s", "");
            assert_eq!(describe(format), expected, "for {mime}");
        }
    }

    #[test]
    fn url_extension_then_dataset_codec_are_the_last_resorts() {
        assert_eq!(
            describe(for_http(&[0x00], None, "http://h/s.aac?t=1", "")),
            "raw ADTS AAC"
        );
        assert_eq!(
            describe(for_http(&[0x00], None, "http://h/s.ogg", "")),
            "container (sniffed)"
        );
        assert_eq!(
            describe(for_http(&[0x00], None, "http://h/listen", "MP3")),
            "raw MPEG audio"
        );
        assert_eq!(
            describe(for_http(&[0x00], None, "http://h/listen", "UNKNOWN")),
            "container (sniffed)"
        );
    }

    #[test]
    fn hls_codec_maps_directly() {
        assert_eq!(describe(for_hls(AudioCodec::AdtsAac)), "raw ADTS AAC");
        assert_eq!(describe(for_hls(AudioCodec::MpegAudio)), "raw MPEG audio");
        assert_eq!(describe(for_hls(AudioCodec::Unknown)), "container (sniffed)");
    }

    #[test]
    fn adts_reader_opens_an_mpeg2_stream_through_the_normalizer() {
        // Two MPEG-2 ADTS frames: symphonia's own probe cannot find these, but
        // open_reader must.
        let mut stream: Vec<u8> = Vec::new();
        for _ in 0..4 {
            let mut frame = vec![0xFF, 0xF9, 0x50, 0x80, 0x00, 0x1F, 0x00];
            let payload = 60usize;
            let total = frame.len() + payload;
            frame[3] = 0x80 | ((total >> 11) & 0x03) as u8;
            frame[4] = ((total >> 3) & 0xFF) as u8;
            frame[5] = (0x1F) | (((total & 0x07) << 5) as u8);
            frame.extend(std::iter::repeat(0x55).take(payload));
            stream.extend(frame);
        }

        let reader = open_reader(StreamFormat::Adts, std::io::Cursor::new(stream))
            .expect("AdtsReader should open the normalized stream");
        assert_eq!(reader.tracks().len(), 1);
        assert_eq!(reader.tracks()[0].codec_params.sample_rate, Some(44100));
    }
}
