/// One station record, straight port of the Haiku build's `Station` struct.
#[derive(Clone, Debug, Default)]
pub struct Station {
    pub name: String,
    pub url: String,
    pub url_resolved: String,
    pub country: String,
    pub codec: String,
    pub bitrate: i32,
    pub language: String,
    pub lat: f64,
    pub lon: f64,
    pub has_location: bool,

    /// True for stations sourced from TuneIn: `playback_url()` then returns a
    /// Tune.ashx resolver link (a tiny text response containing the real
    /// stream URL) rather than a directly playable stream, since TuneIn
    /// doesn't hand out raw stream URLs in its browse listings.
    pub needs_tunein_resolve: bool,
}

impl Station {
    /// Prefer the pre-resolved stream URL (radio-browser follows playlist
    /// redirects server-side) and fall back to the raw one.
    pub fn playback_url(&self) -> &str {
        if self.url_resolved.is_empty() {
            &self.url
        } else {
            &self.url_resolved
        }
    }

    /// e.g. "MP3 128kbps", "AAC", "128kbps", or "" if neither is known.
    /// radio-browser uses the literal string "UNKNOWN" for stations whose
    /// codec it couldn't determine - treat that the same as an empty codec
    /// rather than showing the placeholder text to the user.
    pub fn codec_bitrate_label(&self) -> String {
        let mut text = if self.codec == "UNKNOWN" {
            String::new()
        } else {
            self.codec.clone()
        };
        if self.bitrate > 0 {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&format!("{}kbps", self.bitrate));
        }
        text
    }

    /// Everything the dataset knows about a station, for the row tooltip.
    pub fn details(&self) -> String {
        let mut lines = vec![self.name.clone()];

        let mut second = self.codec_bitrate_label();
        if !self.language.is_empty() {
            if !second.is_empty() {
                second.push_str(" - ");
            }
            second.push_str(&self.language);
        }
        if !second.is_empty() {
            lines.push(second);
        }

        if self.has_location {
            lines.push(format!(
                "{} ({:.3}, {:.3})",
                self.country, self.lat, self.lon
            ));
        } else if !self.country.is_empty() {
            lines.push(self.country.clone());
        }

        lines.push(self.playback_url().to_string());
        lines.join("\n")
    }
}
