//! Registers system fonts that can actually render the dataset.
//!
//! egui's bundled fonts cover Latin and a little more, but the station list is
//! full of CJK, Cyrillic, Greek, Arabic and Thai names ("775ライブリーFM",
//! "Радио Классик", ...) that would otherwise draw as empty boxes. Haiku's Noto
//! stack handled this for free; on other platforms the app has to go find fonts
//! itself.
//!
//! [`install_fallbacks`] is platform-neutral - the caller supplies the candidate
//! paths, so the macOS port passes its own list. [`install_system_fallback`] is
//! the Linux list plus an `fc-match` last resort.

use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// Broad-coverage Linux fallbacks, tried in order. `.ttc` collections are fine -
/// the first face in the collection is used.
const LINUX_CANDIDATES: &[&str] = &[
    // Noto CJK: the widest single-file coverage available on Mint/Ubuntu
    // (fonts-noto-cjk).
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJKjp-Regular.otf",
    "/usr/share/fonts/opentype/noto/NotoSansCJKkr-Regular.otf",
    "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    // Other CJK-capable packages commonly installed on Mint.
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
    "/usr/share/fonts/truetype/unfonts-core/UnDotum.ttf",
    "/usr/share/fonts/truetype/hanazono/HanaMinA.ttf",
    // No CJK, but covers Cyrillic/Greek/Latin-extended and is present on
    // essentially every Debian-derived desktop.
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    // Sparse glyphs but nearly complete Unicode coverage - better than tofu.
    "/usr/share/fonts/truetype/unifont/unifont.ttf",
];

/// How many fallback fonts to load at most. Each one is a few MB of glyph data
/// resident for the process's lifetime, and past a handful the extra coverage
/// stops being worth it.
const MAX_FALLBACKS: usize = 4;

fn fc_match(pattern: &str) -> Option<PathBuf> {
    let output = Command::new("fc-match")
        .args(["-f", "%{file}", pattern])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim().to_string());
    if path.is_file() {
        Some(path)
    } else {
        None
    }
}

/// Builds the font definitions for a set of already-read fonts.
fn definitions(fonts: &[(String, Vec<u8>)]) -> egui::FontDefinitions {
    let mut definitions = egui::FontDefinitions::default();
    for (name, bytes) in fonts {
        definitions.font_data.insert(
            name.clone(),
            Arc::new(egui::FontData::from_owned(bytes.clone())),
        );
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            definitions
                .families
                .entry(family)
                .or_default()
                .push(name.clone());
        }
    }
    definitions
}

/// Lays out some text with `fonts` applied on a throwaway context, to find out
/// whether the font data actually parses.
///
/// A system font that egui's parser chokes on would otherwise panic on the first
/// frame and take the whole app with it - and which fonts exist is a property of
/// the user's machine, not something this code can be sure of. Probing here turns
/// that crash into a skipped font.
fn fonts_are_usable(fonts: &[(String, Vec<u8>)]) -> bool {
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {})); // the probe's panic is expected; stay quiet
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let probe = egui::Context::default();
        probe.set_fonts(definitions(fonts));
        // Text covering the scripts these fallbacks exist for, so the glyph
        // lookup path runs and not just the header parse.
        let _ = probe.run(Default::default(), |ctx| {
            egui::Area::new("probe".into()).show(ctx, |ui| {
                ui.label("ライブリー 음악 世界 Классик A");
            });
        });
    }));
    panic::set_hook(previous_hook);
    outcome.is_ok()
}

/// Installs up to [`MAX_FALLBACKS`] of `candidates` as the lowest-priority font
/// families, so egui's own fonts still render Latin text and these only fill in
/// what those can't. Returns the fonts actually installed, in priority order.
///
/// Candidates that don't exist, can't be read, or don't parse are skipped.
pub fn install_fallbacks<P: AsRef<Path>>(ctx: &egui::Context, candidates: &[P]) -> Vec<PathBuf> {
    let mut fonts: Vec<(String, Vec<u8>)> = Vec::new();
    let mut installed: Vec<PathBuf> = Vec::new();

    for candidate in candidates {
        if installed.len() >= MAX_FALLBACKS {
            break;
        }
        let path = candidate.as_ref();
        if !path.is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };

        let name = format!("fallback-{}", installed.len());
        fonts.push((name, bytes));
        if fonts_are_usable(&fonts) {
            installed.push(path.to_path_buf());
        } else {
            // Unparseable: drop it and keep going with the rest.
            eprintln!("rworldradio: skipping unusable font {}", path.display());
            fonts.pop();
        }
    }

    if fonts.is_empty() {
        return installed;
    }
    ctx.set_fonts(definitions(&fonts));
    installed
}

/// Linux font discovery: the packaged paths above, then `fc-match` for anything
/// installed somewhere this list doesn't know about. "sans:lang=ja" biases
/// fontconfig towards a CJK-capable face.
pub fn install_system_fallback(ctx: &egui::Context) {
    let mut candidates: Vec<PathBuf> = LINUX_CANDIDATES.iter().map(PathBuf::from).collect();
    candidates.extend(fc_match("sans:lang=ja"));
    candidates.extend(fc_match("sans:lang=ko"));
    candidates.extend(fc_match("sans"));

    if install_fallbacks(ctx, &candidates).is_empty() {
        eprintln!(
            "rworldradio: no usable system font found; non-Latin station names may \
             not render. Install fonts-noto-cjk."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_candidates_are_skipped_without_error() {
        let ctx = egui::Context::default();
        let installed = install_fallbacks(&ctx, &["/nonexistent/font.ttf", "/also/missing.otf"]);
        assert!(installed.is_empty());
    }

    #[test]
    fn garbage_font_data_is_rejected_rather_than_panicking() {
        let dir = std::env::temp_dir().join("rworldradio-font-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not-a-font.ttf");
        std::fs::write(&path, b"this is definitely not a font file").unwrap();

        let ctx = egui::Context::default();
        let installed = install_fallbacks(&ctx, &[&path]);
        assert!(installed.is_empty(), "garbage must not be installed");

        std::fs::remove_file(&path).ok();
    }
}
