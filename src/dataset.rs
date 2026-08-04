//! Parses the pre-built dataset produced by `tools/update_stations_db.py`:
//! `data/countries.json` (an index) plus one `data/countries/<file>.json` per
//! country. Pure JSON parsing, no filesystem/network access, so it's testable
//! on its own.
//!
//! Field access goes through the small `as_*` helpers rather than a `#[derive]`d
//! struct on purpose: radio-browser occasionally emits a numeric-looking field
//! as a string (and vice versa), and lat/lon are frequently `null`. That
//! tolerance is what the Haiku original's `JsonValue::AsString`/`AsInt` did, and
//! dropping it would turn a schema wobble into a whole country failing to load.

use serde_json::Value;

use crate::station::Station;

#[derive(Clone, Debug, Default)]
pub struct CountryEntry {
    pub name: String,
    pub file: String,
    pub count: i32,
}

fn as_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn as_f64(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn as_i32(value: Option<&Value>) -> i32 {
    as_f64(value).unwrap_or(0.0) as i32
}

fn as_bool(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Bool(true)))
}

/// Parses `data/countries.json`.
pub fn parse_country_index(json: &str) -> Result<Vec<CountryEntry>, String> {
    let root: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let array = match root {
        Value::Array(items) => items,
        _ => return Ok(Vec::new()),
    };

    let mut entries = Vec::with_capacity(array.len());
    for item in &array {
        let object = match item {
            Value::Object(_) => item,
            _ => continue,
        };
        let entry = CountryEntry {
            name: as_string(object.get("name")),
            file: as_string(object.get("file")),
            count: as_i32(object.get("count")),
        };
        if entry.name.is_empty() || entry.file.is_empty() {
            continue;
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// Parses one `data/countries/<file>.json` (a flat array of station records).
pub fn parse_country_stations(json: &str, country_name: &str) -> Result<Vec<Station>, String> {
    let root: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let array = match root {
        Value::Array(items) => items,
        _ => return Ok(Vec::new()),
    };

    let mut stations = Vec::with_capacity(array.len());
    for item in &array {
        let object = match item {
            Value::Object(_) => item,
            _ => continue,
        };

        let mut station = Station {
            country: country_name.to_string(),
            name: as_string(object.get("name")),
            url: as_string(object.get("url")),
            url_resolved: as_string(object.get("url_resolved")),
            codec: as_string(object.get("codec")),
            bitrate: as_i32(object.get("bitrate")),
            language: as_string(object.get("language")),
            needs_tunein_resolve: as_bool(object.get("needsResolve")),
            ..Station::default()
        };

        if let (Some(lat), Some(lon)) = (as_f64(object.get("lat")), as_f64(object.get("lon"))) {
            station.lat = lat;
            station.lon = lon;
            station.has_location = true;
        }

        if station.name.is_empty() || station.url.is_empty() {
            continue;
        }
        stations.push(station);
    }
    Ok(stations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_country_index_and_skips_incomplete_entries() {
        let json = r#"[
            {"name": "Germany", "file": "germany.json", "count": 3},
            {"name": "", "file": "broken.json", "count": 1},
            {"file": "nameless.json"},
            {"name": "Japan", "file": "japan.json", "count": "42"}
        ]"#;
        let entries = parse_country_index(json).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "Germany");
        assert_eq!(entries[0].count, 3);
        // count arriving as a string is still accepted
        assert_eq!(entries[1].count, 42);
    }

    #[test]
    fn parses_stations_with_null_location_and_resolve_flag() {
        let json = r#"[
            {"name": "A", "url": "http://a/", "codec": "MP3", "bitrate": 128,
             "needsResolve": false, "lat": null, "lon": null, "language": "german"},
            {"name": "B", "url": "http://b/", "codec": "AAC", "bitrate": 0,
             "needsResolve": true, "lat": 52.5, "lon": 13.4, "language": ""},
            {"name": "no url", "url": "", "codec": "MP3"}
        ]"#;
        let stations = parse_country_stations(json, "Germany").unwrap();
        assert_eq!(stations.len(), 2);

        assert_eq!(stations[0].country, "Germany");
        assert!(!stations[0].has_location);
        assert!(!stations[0].needs_tunein_resolve);
        assert_eq!(stations[0].codec_bitrate_label(), "MP3 128kbps");

        assert!(stations[1].has_location);
        assert!(stations[1].needs_tunein_resolve);
        assert_eq!(stations[1].codec_bitrate_label(), "AAC");
    }

    #[test]
    fn unknown_codec_is_not_shown() {
        let station = Station {
            codec: "UNKNOWN".into(),
            bitrate: 96,
            ..Station::default()
        };
        assert_eq!(station.codec_bitrate_label(), "96kbps");
    }

    #[test]
    fn playback_url_prefers_resolved() {
        let mut station = Station {
            url: "http://raw/".into(),
            ..Station::default()
        };
        assert_eq!(station.playback_url(), "http://raw/");
        station.url_resolved = "http://resolved/".into();
        assert_eq!(station.playback_url(), "http://resolved/");
    }
}
