//! Loads the station list from the pre-built dataset (`data/countries.json` +
//! `data/countries/<file>.json`, produced by `tools/update_stations_db.py`) that
//! ships alongside the app - no network access, no TTL, nothing to refresh.
//! Meant to be called from a worker thread since it does file I/O.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::dataset;
use crate::station::Station;

pub struct LoadResult {
    pub by_country: BTreeMap<String, Vec<Station>>,
    /// Non-fatal notes (a country file that couldn't be read or parsed), kept
    /// so the status line can mention them without failing the whole load.
    pub warning: Option<String>,
    pub data_dir: PathBuf,
}

/// `data/` locations relative to the executable itself, which is what covers
/// running straight out of a checkout (`cargo run`, `target/release/…`, or a
/// release binary sitting next to a copied `data/`). Platform-neutral, so the
/// macOS port builds its own candidate list on top of this.
pub fn exe_relative_candidates() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    let Ok(exe) = env::current_exe() else {
        return candidates;
    };
    let Some(exe_dir) = exe.parent() else {
        return candidates;
    };

    candidates.push(exe_dir.join("data"));
    let mut dir = exe_dir;
    // …/target/release/rworldradio -> <project>/data -> <repo>/data
    for _ in 0..3 {
        match dir.parent() {
            Some(parent) => {
                candidates.push(parent.join("data"));
                dir = parent;
            }
            None => break,
        }
    }
    candidates
}

/// The last-resort relative paths the Haiku original also ended its search with.
/// `data` is what `cargo run` from the repo root uses; `../data` covers being run
/// from a subdirectory (and is what the macOS port, which vendors this crate one
/// level down, relies on).
pub fn relative_candidates() -> Vec<PathBuf> {
    vec![PathBuf::from("../data"), PathBuf::from("data")]
}

/// Where `data/` may live on Linux, in priority order: next to the binary, then
/// the two places `install.sh` can put it (per-user XDG vs. system-wide).
fn candidate_data_dirs() -> Vec<PathBuf> {
    let mut candidates = exe_relative_candidates();

    if let Some(dir) = xdg_data_home() {
        candidates.push(dir.join("rworldradio/data"));
    }
    candidates.push(PathBuf::from("/usr/local/share/rworldradio/data"));
    candidates.push(PathBuf::from("/usr/share/rworldradio/data"));
    candidates.extend(relative_candidates());
    candidates
}

fn xdg_data_home() -> Option<PathBuf> {
    if let Ok(dir) = env::var("XDG_DATA_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    env::var("HOME").ok().map(|home| PathBuf::from(home).join(".local/share"))
}

fn read_file(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))
}

/// Loads the dataset using the Linux search path.
pub fn load() -> Result<LoadResult, String> {
    load_from(&candidate_data_dirs())
}

/// Loads the dataset from the first of `candidates` that holds a
/// `countries.json`. The platform-specific part of loading is the candidate list
/// and nothing else, so each port supplies its own.
pub fn load_from(candidates: &[PathBuf]) -> Result<LoadResult, String> {
    let data_dir = candidates
        .iter()
        .find(|dir| dir.join("countries.json").is_file())
        .cloned()
        .ok_or_else(|| {
            format!(
                "data/countries.json not found. Looked in: {}",
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    let index_json = read_file(&data_dir.join("countries.json"))?;
    let countries = dataset::parse_country_index(&index_json)
        .map_err(|e| format!("countries.json parse error: {e}"))?;
    if countries.is_empty() {
        return Err("countries.json parsed but is empty".to_string());
    }

    let mut by_country: BTreeMap<String, Vec<Station>> = BTreeMap::new();
    let mut skipped = 0usize;
    let mut mismatched = 0usize;
    let mut first_problem: Option<String> = None;

    for entry in &countries {
        let path = data_dir.join("countries").join(&entry.file);
        // A missing/unreadable/corrupt country file is skipped rather than
        // failing the whole load, same as the Haiku original.
        let stations = match read_file(&path).and_then(|json| {
            dataset::parse_country_stations(&json, &entry.name)
                .map_err(|e| format!("{}: {}", entry.file, e))
        }) {
            Ok(stations) => stations,
            Err(problem) => {
                skipped += 1;
                first_problem.get_or_insert(problem);
                continue;
            }
        };
        // The index records each country's station count; a disagreement means
        // data/ was regenerated only partway. Not fatal - the per-country file is
        // the source of truth for what's playable - but worth surfacing.
        if entry.count > 0 && entry.count as usize != stations.len() {
            mismatched += 1;
            first_problem.get_or_insert(format!(
                "{} lists {} stations but countries.json says {}",
                entry.file,
                stations.len(),
                entry.count
            ));
        }

        if !stations.is_empty() {
            by_country.insert(entry.name.clone(), stations);
        }
    }

    if by_country.is_empty() {
        return Err(format!(
            "no country files could be read from {}{}",
            data_dir.display(),
            first_problem
                .map(|p| format!(" - {p}"))
                .unwrap_or_default()
        ));
    }

    let warning = first_problem.map(|problem| match (skipped, mismatched) {
        (0, 1) => problem,
        (0, count) => format!("{count} country files disagree with the index, first: {problem}"),
        (1, 0) => format!("1 country file skipped: {problem}"),
        (count, 0) => format!("{count} country files skipped, first: {problem}"),
        (skipped, mismatched) => format!(
            "{skipped} country files skipped, {mismatched} disagree with the index, first: {problem}"
        ),
    });

    Ok(LoadResult {
        by_country,
        warning,
        data_dir,
    })
}
