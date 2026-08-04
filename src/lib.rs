//! R World Radio - Linux port.
//!
//! The Haiku original was built on the BeAPI: BApplication/BWindow for the UI,
//! the Media Kit (BMediaFile/BMediaTrack/BSoundPlayer) for playback, and the
//! classic Network Kit for HTTP. None of that exists on Linux, so each kit is
//! replaced by a pure-Rust crate:
//!
//! | Haiku | Linux |
//! |---|---|
//! | BApplication / BWindow / BListView | egui + eframe ([`ui`], [`level_meter`]) |
//! | BMediaFile / BMediaTrack | symphonia ([`player`]) |
//! | BSoundPlayer | cpal ([`audio`], [`player`]) |
//! | BUrlRequest / BHttpRequest | ureq + rustls ([`net`]) |
//! | BAdapterIO / BInputAdapter | [`byte_queue`] + [`hls`] |
//! | JsonValue (hand-written) | serde_json ([`dataset`]) |
//!
//! The app's own logic - the dataset format, the HLS playlist parser, the
//! MPEG-TS demuxer, the station search paths, the generation-counter session
//! model - is ported over rather than rewritten.
//!
//! This is a library plus a thin `main.rs` so the tests and
//! `examples/probe_stream.rs` exercise the same code the app runs.

pub mod adts;
pub mod audio;
pub mod byte_queue;
pub mod dataset;
pub mod fonts;
pub mod format;
pub mod hls;
pub mod icon;
pub mod level_meter;
pub mod m3u8;
pub mod net;
pub mod player;
pub mod segment;
pub mod station;
pub mod station_cache;
pub mod ts_demuxer;
pub mod ui;
