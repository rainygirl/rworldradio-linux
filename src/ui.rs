//! The window: country list, station list, now-playing/status row. Port of the
//! Haiku build's `MainWindow` (BWindow + two BListViews + a status group) onto
//! egui.
//!
//! Two things differ from the original on purpose. The lists are virtualised
//! (`show_rows`) because egui draws every widget every frame and some countries
//! have a few thousand stations; and each list has a filter box, because
//! scrolling ~200 countries or ~2000 stations with no keyboard type-ahead (which
//! BListView had and egui does not) is otherwise painful.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::level_meter::level_meter;
use crate::player::{Player, PlayerEvent};
use crate::station::Station;
use crate::station_cache::{self, LoadResult};

/// How often the level meter is refreshed while playing (the Haiku build polled
/// `CurrentLevel()` from a 100ms BMessageRunner - same rate).
const LEVEL_POLL: Duration = Duration::from_millis(100);

/// Rows that fill the list's width, with their text still left-aligned.
fn list_row_layout() -> egui::Layout {
    egui::Layout::top_down_justified(egui::Align::LEFT)
}

/// One frame shared by both list panels.
///
/// egui's defaults differ - `Frame::side_top_panel` uses a vertical inner margin
/// of 2 and `Frame::central_panel` uses 8 - which put the country and station
/// search boxes on visibly different baselines. Using the same frame for both is
/// what keeps the two headers on one line.
fn list_panel_frame(ctx: &egui::Context) -> egui::Frame {
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(8, 6))
        .fill(ctx.style().visuals.panel_fill)
}

#[derive(Clone, PartialEq, Eq)]
enum PlaybackState {
    Idle,
    Connecting,
    Playing,
    Failed,
}

pub struct RadioApp {
    player: Player,
    events: Receiver<PlayerEvent>,
    load: Option<Receiver<Result<LoadResult, String>>>,

    stations_by_country: BTreeMap<String, Vec<Station>>,
    countries: Vec<String>,
    selected_country: Option<usize>,
    selected_station: Option<usize>,
    country_filter: String,
    station_filter: String,

    status: String,
    /// Longer form of `status` (dataset path, load warnings), shown on hover so
    /// the status row itself stays short enough not to crowd the level meter.
    status_detail: String,
    playback: PlaybackState,
    now_playing: String,
    /// The station the user last invoked, kept only so the format/bitrate label
    /// can be filled in from our own dataset once playback actually starts - the
    /// decoded stream doesn't expose a friendly codec name/bitrate the way the
    /// dataset record already does.
    current_station: Option<Station>,
}

/// Loads the station dataset. Boxed so each port can supply its own search path
/// without this module knowing anything about platform directory conventions.
pub type DatasetLoader = Box<dyn FnOnce() -> Result<LoadResult, String> + Send + 'static>;

impl RadioApp {
    /// Uses the platform default dataset search path (see
    /// [`station_cache::load`]).
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        Self::with_loader(cc, Box::new(station_cache::load))
    }

    pub fn with_loader(cc: &eframe::CreationContext<'_>, loader: DatasetLoader) -> Self {
        let ctx = cc.egui_ctx.clone();
        let waker: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());

        let (event_tx, event_rx) = mpsc::channel();
        let player = Player::new(event_tx, Arc::clone(&waker));

        // Loading the dataset is a few hundred file reads and a few MB of JSON -
        // fast, but not something to do on the first frame.
        let (load_tx, load_rx) = mpsc::channel();
        let load_waker = Arc::clone(&waker);
        thread::Builder::new()
            .name("station-load".into())
            .spawn(move || {
                let _ = load_tx.send(loader());
                load_waker();
            })
            .expect("could not start station loader thread");

        RadioApp {
            player,
            events: event_rx,
            load: Some(load_rx),
            stations_by_country: BTreeMap::new(),
            countries: Vec::new(),
            selected_country: None,
            selected_station: None,
            country_filter: String::new(),
            station_filter: String::new(),
            status: "Loading stations...".to_string(),
            status_detail: String::new(),
            playback: PlaybackState::Idle,
            now_playing: "Stopped".to_string(),
            current_station: None,
        }
    }

    fn poll_load(&mut self) {
        let Some(receiver) = &self.load else { return };
        let Ok(result) = receiver.try_recv() else { return };
        self.load = None;

        match result {
            Ok(loaded) => {
                let stations: usize = loaded.by_country.values().map(Vec::len).sum();
                self.status = format!(
                    "{} countries, {stations} stations",
                    loaded.by_country.len()
                );
                // The dataset path and any load warning go in the tooltip: they
                // matter when something is wrong, but they're too long to sit in
                // the status row next to the level meter.
                self.status_detail = format!("Loaded from {}", loaded.data_dir.display());
                if let Some(warning) = &loaded.warning {
                    self.status_detail.push_str(&format!("\n{warning}"));
                }
                self.countries = loaded.by_country.keys().cloned().collect();
                self.stations_by_country = loaded.by_country;
                self.selected_country = if self.countries.is_empty() { None } else { Some(0) };
                self.selected_station = None;
            }
            Err(error) => {
                self.status = "Failed to load stations".to_string();
                self.status_detail = error;
            }
        }
    }

    fn poll_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                PlayerEvent::Connecting { station } => {
                    self.playback = PlaybackState::Connecting;
                    self.now_playing = format!("Connecting: {station}");
                }
                PlayerEvent::Playing { station } => {
                    self.playback = PlaybackState::Playing;
                    self.now_playing = format!("Now Playing: {station}");
                }
                PlayerEvent::Stopped => {
                    self.playback = PlaybackState::Idle;
                    self.now_playing = "Stopped".to_string();
                }
                PlayerEvent::Error { station, detail } => {
                    self.playback = PlaybackState::Failed;
                    self.now_playing = format!("Error ({station}): {detail}");
                }
            }
        }
    }

    fn selected_country_name(&self) -> Option<&String> {
        self.selected_country.and_then(|index| self.countries.get(index))
    }

    fn stations_of_selected_country(&self) -> &[Station] {
        self.selected_country_name()
            .and_then(|name| self.stations_by_country.get(name))
            .map(|stations| stations.as_slice())
            .unwrap_or(&[])
    }

    fn play(&mut self, station: Station) {
        self.current_station = Some(station.clone());
        self.player.play(station);
    }

    fn draw_country_list(&mut self, ui: &mut egui::Ui) {
        // Just a search field, no caption: the panel it heads is obviously the
        // country list, and the hint text says what typing here does.
        ui.add(
            egui::TextEdit::singleline(&mut self.country_filter)
                .hint_text("Search countries")
                .desired_width(f32::INFINITY),
        );
        ui.separator();

        let needle = self.country_filter.to_lowercase();
        let visible: Vec<usize> = self
            .countries
            .iter()
            .enumerate()
            .filter(|(_, name)| needle.is_empty() || name.to_lowercase().contains(&needle))
            .map(|(index, _)| index)
            .collect();

        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
        egui::ScrollArea::vertical()
            .id_salt("countries")
            .auto_shrink([false, false])
            .show_rows(ui, row_height, visible.len(), |ui, range| {
                // Justified layout so each row spans the full list width: a
                // plain selectable_label is only as wide as its text, which
                // means clicking to the right of a short name does nothing -
                // not how BListView (or any list) is expected to behave.
                ui.with_layout(list_row_layout(), |ui| {
                    for &index in &visible[range] {
                        let selected = self.selected_country == Some(index);
                        if ui
                            .selectable_label(selected, &self.countries[index])
                            .clicked()
                            && !selected
                        {
                            self.selected_country = Some(index);
                            self.selected_station = None;
                            self.station_filter.clear();
                        }
                    }
                });
            });
    }

    fn draw_station_list(&mut self, ui: &mut egui::Ui) {
        // Same treatment as the country list: the selected country is already
        // highlighted next to this panel, so naming it again here is noise.
        ui.add(
            egui::TextEdit::singleline(&mut self.station_filter)
                .hint_text("Search stations")
                .desired_width(f32::INFINITY),
        );
        ui.separator();

        let needle = self.station_filter.to_lowercase();
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 4.0;

        // Selection changes are collected into locals so the row closure only
        // ever borrows the station slice immutably - `self` is updated once the
        // scroll area is done with it.
        let mut new_selection = self.selected_station;
        let mut to_play: Option<Station> = None;
        {
            let stations = self.stations_of_selected_country();
            let visible: Vec<usize> = stations
                .iter()
                .enumerate()
                .filter(|(_, station)| {
                    needle.is_empty() || station.name.to_lowercase().contains(&needle)
                })
                .map(|(index, _)| index)
                .collect();
            let selected = self.selected_station;

            egui::ScrollArea::vertical()
                .id_salt("stations")
                .auto_shrink([false, false])
                .show_rows(ui, row_height, visible.len(), |ui, range| {
                    ui.with_layout(list_row_layout(), |ui| {
                        for &index in &visible[range] {
                            let station = &stations[index];
                            let label = ui
                                .selectable_label(selected == Some(index), &station.name)
                                .on_hover_text(station.details());
                            if label.clicked() {
                                new_selection = Some(index);
                            }
                            if label.double_clicked() {
                                new_selection = Some(index);
                                to_play = Some(station.clone());
                            }
                        }
                    });
                });
        }

        self.selected_station = new_selection;
        if let Some(station) = to_play {
            self.play(station);
        }
    }

    fn draw_status_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let playing = self.playback == PlaybackState::Playing;
            // Only offer stop while something is actually playing, matching the
            // original's hidden-unless-playing stop button.
            if playing && ui.button("\u{25A0}").on_hover_text("Stop").clicked() {
                self.player.stop();
            }

            let can_play = self.selected_station.is_some();
            if ui
                .add_enabled(can_play, egui::Button::new("\u{25B6}"))
                .on_hover_text("Play the selected station (or double-click it in the list)")
                .clicked()
            {
                if let Some(station) = self
                    .selected_station
                    .and_then(|index| self.stations_of_selected_country().get(index))
                    .cloned()
                {
                    self.play(station);
                }
            }

            let color = match self.playback {
                PlaybackState::Failed => Some(ui.visuals().error_fg_color),
                _ => None,
            };
            let mut text = egui::RichText::new(&self.now_playing);
            if let Some(color) = color {
                text = text.color(color);
            }
            ui.label(text);

            if playing {
                if let Some(station) = &self.current_station {
                    let label = station.codec_bitrate_label();
                    if !label.is_empty() {
                        ui.weak(label);
                    }
                }
            }
            level_meter(ui, self.player.level());

            // Truncating, right-aligned: the status text must give way to the
            // now-playing group rather than draw over the level meter.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = ui.add(
                    egui::Label::new(egui::RichText::new(&self.status).weak())
                        .truncate(),
                );
                if !self.status_detail.is_empty() {
                    label.on_hover_text(&self.status_detail);
                }
            });
        });
    }
}

impl eframe::App for RadioApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_load();
        self.poll_events();

        // No menu bar: its only item was Quit, which the window's own close
        // control (and Cmd+Q on macOS) already covers, and the row it needed cost
        // more vertical space than it earned. The ▶ button's tooltip carries the
        // "play the selected station" hint that used to sit next to it.
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(2.0);
            self.draw_status_row(ui);
            ui.add_space(2.0);
        });

        let panel_frame = list_panel_frame(ctx);
        egui::SidePanel::left("countries")
            .frame(panel_frame)
            .default_width(220.0)
            .width_range(140.0..=400.0)
            .show(ctx, |ui| self.draw_country_list(ui));

        egui::CentralPanel::default()
            .frame(panel_frame)
            .show(ctx, |ui| self.draw_station_list(ui));

        // The level meter is the only thing that changes without user input.
        if matches!(
            self.playback,
            PlaybackState::Playing | PlaybackState::Connecting
        ) {
            ctx.request_repaint_after(LEVEL_POLL);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.player.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs one headless frame with a side panel and a central panel, and reports
    /// the top edge of the first widget in each.
    fn first_widget_tops(frame: impl Fn(&egui::Context) -> egui::Frame) -> (f32, f32) {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };

        let left = std::cell::Cell::new(f32::NAN);
        let right = std::cell::Cell::new(f32::NAN);
        let _ = ctx.run(input, |ctx| {
            let panel_frame = frame(ctx);
            egui::SidePanel::left("countries")
                .frame(panel_frame)
                .show(ctx, |ui| {
                    let mut text = String::new();
                    left.set(
                        ui.add(egui::TextEdit::singleline(&mut text))
                            .rect
                            .top(),
                    );
                });
            egui::CentralPanel::default()
                .frame(panel_frame)
                .show(ctx, |ui| {
                    let mut text = String::new();
                    right.set(
                        ui.add(egui::TextEdit::singleline(&mut text))
                            .rect
                            .top(),
                    );
                });
        });
        (left.get(), right.get())
    }

    #[test]
    fn both_search_boxes_sit_on_the_same_line() {
        let (left, right) = first_widget_tops(list_panel_frame);
        assert!(
            (left - right).abs() < 0.5,
            "country search box at y={left}, station search box at y={right}"
        );
    }

    /// Documents why `list_panel_frame` exists at all: with egui's own defaults
    /// the two panels disagree, which is the misalignment it fixes. If a future
    /// egui makes the defaults match, this test fails and the shared frame can go.
    #[test]
    fn egui_default_panel_frames_are_the_thing_that_misaligns() {
        let (left, right) = first_widget_tops(|ctx| {
            // Whichever default each panel type would have used on its own.
            egui::Frame::side_top_panel(&ctx.style())
        });
        let with_defaults = {
            let ctx = egui::Context::default();
            let style = ctx.style();
            let side = egui::Frame::side_top_panel(&style).inner_margin.top;
            let central = egui::Frame::central_panel(&style).inner_margin.top;
            (side, central)
        };
        assert_ne!(
            with_defaults.0, with_defaults.1,
            "egui's side-panel and central-panel top margins now agree ({with_defaults:?}); \
             list_panel_frame is no longer needed"
        );
        // Sanity: forcing one shared frame does align them.
        assert!((left - right).abs() < 0.5, "left {left}, right {right}");
    }
}
