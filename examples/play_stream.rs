//! Diagnostic: actually play a station through the real `Player` (symphonia ->
//! cpal -> ALSA) for a few seconds and report the level meter, with no GUI.
//!
//! Use this to separate "the stream doesn't work" from "audio output doesn't
//! work" - `probe_stream` decodes without touching the sound card, this one does
//! open it.
//!
//!     cargo run --example play_stream -- <stream-url-or-station-name> [seconds]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rworldradio::player::{Player, PlayerEvent};
use rworldradio::station::Station;
use rworldradio::station_cache;

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
        eprintln!("usage: play_stream <stream-url-or-station-name> [seconds]");
        std::process::exit(2);
    };
    let seconds = args
        .next()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(8.0);

    let station = if target.starts_with("http://") || target.starts_with("https://") {
        Station {
            name: target.clone(),
            url: target.clone(),
            ..Station::default()
        }
    } else {
        match find_station(&target) {
            Some(station) => {
                println!("matched \"{}\" -> {}", station.name, station.playback_url());
                station
            }
            None => {
                eprintln!("no station in the dataset matches \"{target}\"");
                std::process::exit(1);
            }
        }
    };

    let (event_tx, event_rx) = mpsc::channel();
    let woken = Arc::new(AtomicBool::new(false));
    let waker_flag = Arc::clone(&woken);
    let player = Player::new(event_tx, Arc::new(move || {
        waker_flag.store(true, Ordering::Relaxed);
    }));

    player.play(station);

    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let mut playing = false;
    let mut peak = 0.0f32;
    let mut failed: Option<String> = None;

    while Instant::now() < deadline {
        while let Ok(event) = event_rx.try_recv() {
            match event {
                PlayerEvent::Connecting { station } => println!("connecting: {station}"),
                PlayerEvent::Playing { station } => {
                    println!("playing: {station}");
                    playing = true;
                }
                PlayerEvent::Stopped => println!("stopped"),
                PlayerEvent::Error { station, detail } => {
                    println!("error ({station}): {detail}");
                    failed = Some(detail);
                }
            }
        }
        if failed.is_some() {
            break;
        }
        let level = player.level();
        if level > peak {
            peak = level;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    player.stop();
    std::thread::sleep(Duration::from_millis(300));

    println!("peak level while playing: {peak:.3}");
    if let Some(detail) = failed {
        eprintln!("FAILED: {detail}");
        std::process::exit(1);
    }
    if !playing {
        eprintln!("FAILED: never reached the playing state");
        std::process::exit(1);
    }
    if peak == 0.0 {
        eprintln!("WARNING: audio device opened but the level meter never moved");
        std::process::exit(1);
    }
    println!("OK");
}
