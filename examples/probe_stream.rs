//! Diagnostic: open one station's stream through exactly the code path the app
//! uses (TuneIn resolve -> HLS or progressive HTTP -> symphonia) and report what
//! came out, without touching an audio device.
//!
//! This is how to tell "the stream is dead / the codec isn't supported" apart
//! from "audio output is misconfigured" on a machine where playback is silent.
//!
//!     cargo run --example probe_stream -- <stream-url-or-station-name> [seconds]
//!
//! A bare name is looked up in the bundled dataset, so both of these work:
//!
//!     cargo run --example probe_stream -- https://host/stream.mp3
//!     cargo run --example probe_stream -- "BBC Radio 1" 5

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;

use rworldradio::byte_queue::QueueReader;
use rworldradio::station::Station;
use rworldradio::{format, player, station_cache};

fn find_station(query: &str) -> Option<Station> {
    let loaded = station_cache::load().ok()?;
    let needle = query.to_lowercase();
    loaded
        .by_country
        .values()
        .flatten()
        .find(|station| station.name.to_lowercase().contains(&needle))
        .cloned()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(target) = args.next() else {
        eprintln!("usage: probe_stream <stream-url-or-station-name> [seconds]");
        std::process::exit(2);
    };
    let budget = Duration::from_secs_f64(
        args.next()
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(5.0),
    );

    let station = if target.starts_with("http://") || target.starts_with("https://") {
        Station {
            name: target.clone(),
            url: target.clone(),
            ..Station::default()
        }
    } else {
        match find_station(&target) {
            Some(station) => {
                println!(
                    "matched \"{}\" ({}) -> {}",
                    station.name,
                    station.country,
                    station.playback_url()
                );
                station
            }
            None => {
                eprintln!("no station in the dataset matches \"{target}\"");
                std::process::exit(1);
            }
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let (queue, stream_format) = match player::open_byte_source(&station, Arc::clone(&stop)) {
        Ok(opened) => opened,
        Err(error) => {
            eprintln!("open failed: {error}");
            std::process::exit(1);
        }
    };
    println!(
        "byte source open in {:?}, read as {}",
        started.elapsed(),
        stream_format.describe()
    );

    let mut format =
        match format::open_reader(stream_format, QueueReader::new(Arc::clone(&queue))) {
            Ok(reader) => reader,
            Err(error) => {
                eprintln!("could not open reader: {error}");
                stop.store(true, Ordering::SeqCst);
                queue.close(None);
                std::process::exit(1);
            }
        };

    let track = format
        .tracks()
        .iter()
        .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        .expect("no audio track");
    let track_id = track.id;
    println!(
        "container/codec: {:?}, declared rate {:?}, channels {:?}",
        track.codec_params.codec, track.codec_params.sample_rate, track.codec_params.channels
    );

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .expect("no decoder for this stream");

    let mut frames = 0u64;
    let mut peak = 0.0f32;
    let mut spec_reported = false;
    let mut sample_buffer: Option<SampleBuffer<f32>> = None;
    let decode_start = Instant::now();

    while decode_start.elapsed() < budget {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(error) => {
                println!("stream ended: {error}");
                break;
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(error)) => {
                println!("recoverable decode error: {error}");
                continue;
            }
            Err(error) => {
                eprintln!("decode failed: {error}");
                break;
            }
        };

        let spec = *decoded.spec();
        if !spec_reported {
            println!(
                "decoding at {} Hz, {} channel(s)",
                spec.rate,
                spec.channels.count()
            );
            spec_reported = true;
        }

        let capacity = decoded.capacity() as u64;
        let buffer = match sample_buffer.as_mut() {
            Some(buffer) if buffer.capacity() as u64 >= capacity => buffer,
            _ => {
                sample_buffer = Some(SampleBuffer::<f32>::new(capacity, spec));
                sample_buffer.as_mut().unwrap()
            }
        };
        buffer.copy_interleaved_ref(decoded);
        frames += (buffer.samples().len() / spec.channels.count().max(1)) as u64;
        for &sample in buffer.samples() {
            peak = peak.max(sample.abs());
        }
    }

    stop.store(true, Ordering::SeqCst);
    queue.close(None);

    println!(
        "decoded {frames} frames in {:?}, peak amplitude {peak:.3}",
        decode_start.elapsed()
    );
    if frames == 0 {
        eprintln!("no audio decoded - this station would fail to play");
        std::process::exit(1);
    }
    if peak == 0.0 {
        eprintln!("decoded only silence - the station may be off air");
    }
}
