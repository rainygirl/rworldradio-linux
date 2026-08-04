//! Extracts the raw elementary audio stream from one MPEG-TS segment (as used by
//! HLS), stripping TS packet headers and PES framing: parses PAT to find the
//! PMT, PMT to find the audio elementary stream's PID and codec, then
//! concatenates that PID's de-PES'd payload. Direct port of the Haiku build's
//! `TsDemuxer`.
//!
//! Stateless and re-parses PAT/PMT from scratch each call. Real encoders repeat
//! PAT/PMT periodically specifically so a demuxer can start from any segment, so
//! this is fine to call independently per HLS segment rather than carrying PID
//! state across segments.

const PACKET_SIZE: usize = 188;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AudioCodec {
    Unknown,
    /// stream_type 0x0F
    AdtsAac,
    /// MP3/MP2 (stream_type 0x03/0x04)
    MpegAudio,
}

#[derive(Debug)]
pub struct Extracted {
    pub codec: AudioCodec,
    pub elementary_stream: Vec<u8>,
}

struct PacketInfo<'a> {
    pid: u16,
    payload_start: bool,
    /// TS header (and adaptation field, if any) stripped
    payload: &'a [u8],
}

fn parse_packets(data: &[u8]) -> Vec<PacketInfo<'_>> {
    let mut packets = Vec::new();
    let mut position = 0usize;
    while position + PACKET_SIZE <= data.len() {
        if data[position] != 0x47 {
            position += 1; // resync byte-by-byte if we're not aligned
            continue;
        }
        let packet = &data[position..position + PACKET_SIZE];
        let payload_start = packet[1] & 0x40 != 0;
        let pid = (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2]);
        let adaptation_field_control = (packet[3] >> 4) & 0x3;

        if adaptation_field_control == 2 {
            position += PACKET_SIZE; // adaptation field only, no payload
            continue;
        }
        let payload_offset = if adaptation_field_control == 3 {
            5 + usize::from(packet[4])
        } else {
            4
        };
        if payload_offset > PACKET_SIZE {
            position += PACKET_SIZE; // malformed packet, skip it
            continue;
        }

        packets.push(PacketInfo {
            pid,
            payload_start,
            payload: &packet[payload_offset..],
        });
        position += PACKET_SIZE;
    }
    packets
}

/// Start of a PSI section inside a payload-start packet, past the pointer_field.
fn section_of(payload: &[u8], minimum_length: usize) -> Option<&[u8]> {
    let section_start = 1 + usize::from(*payload.first()?);
    let section = payload.get(section_start..)?;
    if section.len() < minimum_length {
        return None;
    }
    Some(section)
}

fn find_pmt_pid(packets: &[PacketInfo<'_>]) -> Option<u16> {
    for packet in packets {
        if packet.pid != 0 || !packet.payload_start {
            continue;
        }
        let section = match section_of(packet.payload, 8) {
            Some(section) => section,
            None => continue,
        };
        if section[0] != 0x00 {
            continue; // PAT table_id
        }
        let section_length = (usize::from(section[1] & 0x0F) << 8) | usize::from(section[2]);
        // 3 header bytes precede section_length's own coverage; the trailing 4
        // bytes are the CRC32.
        let program_end = (3 + section_length).saturating_sub(4).min(section.len());

        let mut index = 8;
        while index + 4 <= program_end {
            let program_number = (u16::from(section[index]) << 8) | u16::from(section[index + 1]);
            let pmt_pid =
                (u16::from(section[index + 2] & 0x1F) << 8) | u16::from(section[index + 3]);
            if program_number != 0 {
                // program_number 0 entries are the NIT
                return Some(pmt_pid);
            }
            index += 4;
        }
    }
    None
}

fn find_audio_stream(packets: &[PacketInfo<'_>], pmt_pid: u16) -> Option<(u16, AudioCodec)> {
    for packet in packets {
        if packet.pid != pmt_pid || !packet.payload_start {
            continue;
        }
        let section = match section_of(packet.payload, 12) {
            Some(section) => section,
            None => continue,
        };
        if section[0] != 0x02 {
            continue; // PMT table_id
        }
        let section_length = (usize::from(section[1] & 0x0F) << 8) | usize::from(section[2]);
        let program_info_length =
            (usize::from(section[10] & 0x0F) << 8) | usize::from(section[11]);
        let end = (3 + section_length).saturating_sub(4).min(section.len());

        let mut index = 12 + program_info_length;
        while index + 5 <= end {
            let stream_type = section[index];
            let elementary_pid =
                (u16::from(section[index + 1] & 0x1F) << 8) | u16::from(section[index + 2]);
            let es_info_length =
                (usize::from(section[index + 3] & 0x0F) << 8) | usize::from(section[index + 4]);

            let codec = match stream_type {
                0x0F => AudioCodec::AdtsAac,
                0x03 | 0x04 => AudioCodec::MpegAudio,
                _ => AudioCodec::Unknown,
            };
            if codec != AudioCodec::Unknown {
                return Some((elementary_pid, codec));
            }
            index += 5 + es_info_length;
        }
    }
    None
}

pub fn extract(ts_data: &[u8]) -> Extracted {
    let mut result = Extracted {
        codec: AudioCodec::Unknown,
        elementary_stream: Vec::new(),
    };

    let packets = parse_packets(ts_data);
    let pmt_pid = match find_pmt_pid(&packets) {
        Some(pid) => pid,
        None => return result,
    };
    let (audio_pid, codec) = match find_audio_stream(&packets, pmt_pid) {
        Some(found) => found,
        None => return result,
    };
    result.codec = codec;

    for packet in &packets {
        if packet.pid != audio_pid {
            continue;
        }
        if packet.payload_start {
            if packet.payload.len() < 9 {
                continue;
            }
            let payload = packet.payload;
            if !(payload[0] == 0x00 && payload[1] == 0x00 && payload[2] == 0x01) {
                continue; // not a PES start code, drop this packet's payload
            }
            let es_start = 9 + usize::from(payload[8]); // + PES_header_data_length
            if es_start < payload.len() {
                result.elementary_stream.extend_from_slice(&payload[es_start..]);
            }
        } else {
            result.elementary_stream.extend_from_slice(packet.payload);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts_packet(pid: u16, payload_start: bool, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0xFFu8; PACKET_SIZE];
        packet[0] = 0x47;
        packet[1] = if payload_start { 0x40 } else { 0x00 } | ((pid >> 8) as u8 & 0x1F);
        packet[2] = (pid & 0xFF) as u8;
        packet[3] = 0x10; // payload only, continuity counter 0
        assert!(payload.len() <= PACKET_SIZE - 4);
        packet[4..4 + payload.len()].copy_from_slice(payload);
        packet
    }

    /// PAT with a single program pointing at PMT PID 0x1000.
    fn pat_payload() -> Vec<u8> {
        let mut section = vec![
            0x00, // table_id (PAT)
            0xB0, 0x0D, // section_syntax_indicator + section_length = 13
            0x00, 0x01, 0xC1, 0x00, 0x00, // transport_stream_id ... last_section
            0x00, 0x01, // program_number 1
            0xF0, 0x00, // reserved + PMT PID 0x1000
        ];
        section.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // CRC32
        let mut payload = vec![0x00]; // pointer_field
        payload.extend_from_slice(&section);
        payload
    }

    /// PMT declaring one ADTS AAC (stream_type 0x0F) stream on PID 0x0101.
    fn pmt_payload(stream_type: u8) -> Vec<u8> {
        let mut section = vec![
            0x02, // table_id (PMT)
            0xB0, 0x12, // section_length = 18
            0x00, 0x01, 0xC1, 0x00, 0x00, // program_number ... last_section
            0xE1, 0x00, // PCR PID
            0xF0, 0x00, // program_info_length = 0
            stream_type,
            0xE1, 0x01, // elementary PID 0x0101
            0xF0, 0x00, // ES_info_length = 0
        ];
        section.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // CRC32
        let mut payload = vec![0x00];
        payload.extend_from_slice(&section);
        payload
    }

    /// PES packet start whose payload is `audio`, with no optional header bytes.
    fn pes_payload(audio: &[u8]) -> Vec<u8> {
        let mut payload = vec![
            0x00, 0x00, 0x01, // packet_start_code_prefix
            0xC0, // stream_id (audio)
            0x00, 0x00, // PES_packet_length
            0x80, 0x00, // flags
            0x00, // PES_header_data_length = 0
        ];
        payload.extend_from_slice(audio);
        payload
    }

    #[test]
    fn extracts_aac_elementary_stream_across_pes_and_continuation() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&ts_packet(0x0000, true, &pat_payload()));
        stream.extend_from_slice(&ts_packet(0x1000, true, &pmt_payload(0x0F)));
        stream.extend_from_slice(&ts_packet(0x0101, true, &pes_payload(&[0x11, 0x22, 0x33])));
        stream.extend_from_slice(&ts_packet(0x0101, false, &[0x44, 0x55]));

        let extracted = extract(&stream);
        assert_eq!(extracted.codec, AudioCodec::AdtsAac);
        // The PES packet's payload comes first, then the continuation packet's.
        assert_eq!(&extracted.elementary_stream[..3], &[0x11, 0x22, 0x33]);
        // Both packets are padded with 0xFF out to 188 bytes, so only check the
        // continuation packet's leading bytes landed in order.
        assert!(extracted.elementary_stream.windows(2).any(|w| w == [0x44, 0x55]));
    }

    #[test]
    fn recognizes_mpeg_audio_stream_type() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&ts_packet(0x0000, true, &pat_payload()));
        stream.extend_from_slice(&ts_packet(0x1000, true, &pmt_payload(0x03)));
        stream.extend_from_slice(&ts_packet(0x0101, true, &pes_payload(&[0x01])));

        assert_eq!(extract(&stream).codec, AudioCodec::MpegAudio);
    }

    #[test]
    fn resyncs_on_a_misaligned_stream() {
        let mut stream = vec![0x00, 0x11]; // junk before the first sync byte
        stream.extend_from_slice(&ts_packet(0x0000, true, &pat_payload()));
        stream.extend_from_slice(&ts_packet(0x1000, true, &pmt_payload(0x0F)));
        stream.extend_from_slice(&ts_packet(0x0101, true, &pes_payload(&[0xAB])));

        assert_eq!(extract(&stream).codec, AudioCodec::AdtsAac);
    }

    #[test]
    fn garbage_yields_no_stream_rather_than_panicking() {
        let extracted = extract(&[0x47u8; 400]);
        assert_eq!(extracted.codec, AudioCodec::Unknown);
        assert!(extracted.elementary_stream.is_empty());
        assert!(extract(&[]).elementary_stream.is_empty());
    }
}
