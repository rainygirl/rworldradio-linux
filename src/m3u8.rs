//! Minimal HLS playlist parser: enough to follow a master playlist to one media
//! playlist, and to read a live media playlist's segment list. Not a
//! general-purpose implementation - no byte-range segments, no encryption, no
//! discontinuities. Direct port of the Haiku build's `M3u8Parser`.

#[derive(Clone, Debug, Default)]
pub struct Variant {
    pub uri: String,
    pub bandwidth: i64,
}

#[derive(Clone, Debug, Default)]
pub struct Segment {
    pub uri: String,
    pub duration: f64,
}

#[derive(Clone, Debug)]
pub struct MediaPlaylist {
    pub segments: Vec<Segment>,
    pub media_sequence: i64,
    pub target_duration: f64,
    pub is_end_list: bool,
}

impl Default for MediaPlaylist {
    fn default() -> Self {
        MediaPlaylist {
            segments: Vec::new(),
            media_sequence: 0,
            target_duration: 6.0,
            is_end_list: false,
        }
    }
}

/// Resolves a possibly-relative URI against a base URL. Handles the cases HLS
/// playlists actually produce: absolute http(s) URLs pass through,
/// protocol-relative ("//host/path"), host-absolute ("/path") and
/// playlist-relative ("segment.ts") URIs resolve against `base_url`'s
/// scheme/origin/directory respectively.
pub fn resolve_url(base_url: &str, uri: &str) -> String {
    if uri.contains("://") {
        return uri.to_string();
    }

    let scheme_end = match base_url.find("://") {
        Some(index) => index,
        // base isn't a URL we can resolve against; best effort
        None => return uri.to_string(),
    };

    // Protocol-relative: inherit only the base's scheme. This has to be checked
    // before the host-absolute case below, otherwise "//host/path" would be
    // pasted onto the base's own origin and produce a bogus double-slash URL.
    // (The Haiku original got this wrong; qingting.fm's playlists use this form.)
    if let Some(rest) = uri.strip_prefix("//") {
        return format!("{}://{}", &base_url[..scheme_end], rest);
    }

    let host_start = scheme_end + 3;
    let path_start = base_url[host_start..].find('/').map(|i| host_start + i);
    let origin = match path_start {
        Some(index) => &base_url[..index],
        None => base_url,
    };

    if uri.starts_with('/') {
        return format!("{origin}{uri}");
    }

    match path_start {
        None => format!("{origin}/{uri}"),
        Some(_) => {
            // Strip any query string before taking the directory, otherwise a
            // "?token=..." containing a slash would be treated as a path.
            let without_query = base_url.split('?').next().unwrap_or(base_url);
            let last_slash = without_query.rfind('/').unwrap_or(without_query.len() - 1);
            format!("{}{}", &without_query[..=last_slash], uri)
        }
    }
}

pub fn is_master_playlist(text: &str) -> bool {
    text.contains("#EXT-X-STREAM-INF")
}

fn lines(text: &str) -> Vec<&str> {
    text.lines().map(|line| line.trim_end()).collect()
}

/// Reads an integer attribute out of an `#EXT-X-...` line, matching either the
/// first attribute (`:KEY=`) or a later one (`,KEY=`).
fn extract_int_attribute(line: &str, key: &str) -> i64 {
    let after_colon = format!(":{key}=");
    let after_comma = format!(",{key}=");
    let (position, offset) = match line.find(&after_colon) {
        Some(index) => (index, after_colon.len()),
        None => match line.find(&after_comma) {
            Some(index) => (index, after_comma.len()),
            None => return 0,
        },
    };
    parse_leading_i64(&line[position + offset..])
}

/// `atoi`/`atof` semantics: parse as much of a leading number as there is and
/// ignore the rest, which is what these playlist fields need (`#EXTINF:6.4, no
/// desc`, `BANDWIDTH=374000,CODECS=...`).
fn parse_leading_i64(text: &str) -> i64 {
    let trimmed = text.trim_start();
    let end = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '-' && c != '+')
        .unwrap_or(trimmed.len());
    trimmed[..end].parse().unwrap_or(0)
}

fn parse_leading_f64(text: &str) -> f64 {
    let trimmed = text.trim_start();
    let end = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != '+')
        .unwrap_or(trimmed.len());
    trimmed[..end].parse().unwrap_or(0.0)
}

/// `base_url` is the playlist's own URL, used to resolve relative variant URIs.
pub fn parse_master_playlist(text: &str, base_url: &str) -> Vec<Variant> {
    let lines = lines(text);
    let mut variants = Vec::new();

    let mut index = 0;
    while index < lines.len() {
        if !lines[index].starts_with("#EXT-X-STREAM-INF:") {
            index += 1;
            continue;
        }

        let bandwidth = extract_int_attribute(lines[index], "BANDWIDTH");
        let mut next = index + 1;
        while next < lines.len() && (lines[next].is_empty() || lines[next].starts_with('#')) {
            next += 1;
        }
        if next >= lines.len() {
            break;
        }

        variants.push(Variant {
            uri: resolve_url(base_url, lines[next]),
            bandwidth,
        });
        index = next + 1;
    }
    variants
}

/// `base_url` is the playlist's own URL, used to resolve relative segment URIs.
pub fn parse_media_playlist(text: &str, base_url: &str) -> MediaPlaylist {
    let mut playlist = MediaPlaylist::default();
    let mut next_duration = 0.0;

    for line in lines(text) {
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            playlist.media_sequence = parse_leading_i64(rest);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            playlist.target_duration = parse_leading_f64(rest);
        } else if line.starts_with("#EXT-X-ENDLIST") {
            playlist.is_end_list = true;
        } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
            next_duration = parse_leading_f64(rest);
        } else if !line.is_empty() && !line.starts_with('#') {
            playlist.segments.push(Segment {
                uri: resolve_url(base_url, line),
                duration: next_duration,
            });
            next_duration = 0.0;
        }
    }
    playlist
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real playlists captured from BBC Radio 1's live HLS feed - the same
    // fixtures the Haiku build's test/test_m3u8_parser.cpp used.
    const MASTER_URL: &str = "http://as-hls-ww-live.akamaized.net/pool_01505109/live/ww/\
bbc_radio_one/bbc_radio_one.isml/bbc_radio_one.m3u8";

    const MASTER_PLAYLIST: &str = "#EXTM3U\n\
#EXT-X-VERSION:3\n\
## Created with Unified Streaming Platform  (version=1.13.5-30103)\n\
\n\
# variants\n\
#EXT-X-STREAM-INF:BANDWIDTH=56000,AVERAGE-BANDWIDTH=51000,CODECS=\"mp4a.40.5\"\n\
bbc_radio_one-audio=48000.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=112000,AVERAGE-BANDWIDTH=102000,CODECS=\"mp4a.40.5\"\n\
bbc_radio_one-audio=96000.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=150000,AVERAGE-BANDWIDTH=136000,CODECS=\"mp4a.40.2\"\n\
bbc_radio_one-audio=128000.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=374000,AVERAGE-BANDWIDTH=340000,CODECS=\"mp4a.40.2\"\n\
bbc_radio_one-audio=320000.m3u8\n";

    const MEDIA_URL: &str = "http://as-hls-ww-live.akamaized.net/pool_01505109/live/ww/\
bbc_radio_one/bbc_radio_one.isml/bbc_radio_one-audio=320000.m3u8";

    const MEDIA_PLAYLIST: &str = "#EXTM3U\n\
#EXT-X-VERSION:3\n\
#EXT-X-MEDIA-SEQUENCE:278793294\n\
#EXT-X-INDEPENDENT-SEGMENTS\n\
#EXT-X-TARGETDURATION:6\n\
#USP-X-TIMESTAMP-MAP:MPEGTS=4700405152,LOCAL=2026-07-17T08:31:15.200000Z\n\
#EXT-X-PROGRAM-DATE-TIME:2026-07-17T08:31:15.200000Z\n\
#EXTINF:6.4, no desc\n\
bbc_radio_one-audio=320000-278793294.ts\n\
#EXTINF:6.4, no desc\n\
bbc_radio_one-audio=320000-278793295.ts\n\
#EXTINF:6.4, no desc\n\
bbc_radio_one-audio=320000-278793296.ts\n";

    #[test]
    fn distinguishes_master_from_media() {
        assert!(is_master_playlist(MASTER_PLAYLIST));
        assert!(!is_master_playlist(MEDIA_PLAYLIST));
    }

    #[test]
    fn parses_master_variants_with_relative_uris() {
        let variants = parse_master_playlist(MASTER_PLAYLIST, MASTER_URL);
        assert_eq!(variants.len(), 4);
        assert_eq!(variants[3].bandwidth, 374000);
        assert_eq!(
            variants[3].uri,
            "http://as-hls-ww-live.akamaized.net/pool_01505109/live/ww/\
bbc_radio_one/bbc_radio_one.isml/bbc_radio_one-audio=320000.m3u8"
        );
    }

    #[test]
    fn parses_live_media_playlist() {
        let playlist = parse_media_playlist(MEDIA_PLAYLIST, MEDIA_URL);
        assert_eq!(playlist.segments.len(), 3);
        assert_eq!(playlist.media_sequence, 278793294);
        assert_eq!(playlist.target_duration, 6.0);
        assert!(!playlist.is_end_list);
        assert_eq!(playlist.segments[0].duration, 6.4);
        assert_eq!(
            playlist.segments[0].uri,
            "http://as-hls-ww-live.akamaized.net/pool_01505109/live/ww/\
bbc_radio_one/bbc_radio_one.isml/bbc_radio_one-audio=320000-278793294.ts"
        );
    }

    #[test]
    fn resolves_the_three_uri_shapes() {
        let base = "https://host.example/a/b/playlist.m3u8";
        assert_eq!(
            resolve_url(base, "https://other.example/x.ts"),
            "https://other.example/x.ts"
        );
        assert_eq!(resolve_url(base, "/root.ts"), "https://host.example/root.ts");
        // Protocol-relative: keep the scheme, replace the host.
        assert_eq!(
            resolve_url("http://ls.qingting.fm/live/4025.m3u8", "//cdn.example/a/b.aac"),
            "http://cdn.example/a/b.aac"
        );
        assert_eq!(
            resolve_url(base, "//cdn.example/a/b.aac"),
            "https://cdn.example/a/b.aac"
        );
        assert_eq!(
            resolve_url(base, "seg.ts"),
            "https://host.example/a/b/seg.ts"
        );
        assert_eq!(
            resolve_url("https://host.example", "seg.ts"),
            "https://host.example/seg.ts"
        );
        // A query string on the playlist URL must not be mistaken for a path.
        assert_eq!(
            resolve_url("https://host.example/a/list.m3u8?t=1/2", "seg.ts"),
            "https://host.example/a/seg.ts"
        );
    }

    #[test]
    fn endlist_is_detected() {
        let playlist = parse_media_playlist(
            "#EXTM3U\n#EXTINF:4.0,\na.ts\n#EXT-X-ENDLIST\n",
            "https://host.example/v/list.m3u8",
        );
        assert!(playlist.is_end_list);
        assert_eq!(playlist.segments.len(), 1);
    }
}
