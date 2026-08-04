//! Window setup and entry point. Everything else lives in the library (see
//! `src/lib.rs` for how the Haiku kits map onto the crates used here).

use rworldradio::{fonts, icon, ui};

fn main() -> eframe::Result<()> {
    // Same 700x500-ish content area the Haiku build opened with.
    let options = eframe::NativeOptions {
        viewport: icon::apply(
            egui::ViewportBuilder::default()
                .with_title("R World Radio")
                .with_inner_size([760.0, 560.0])
                .with_min_inner_size([520.0, 340.0])
                // Matches packaging/rworldradio.desktop's StartupWMClass so
                // XFCE's taskbar and window list pick up the right name and
                // icon.
                .with_app_id("rworldradio"),
        ),
        ..Default::default()
    };

    eframe::run_native(
        "R World Radio",
        options,
        Box::new(|cc| {
            fonts::install_system_fallback(&cc.egui_ctx);
            Ok(Box::new(ui::RadioApp::new(cc)))
        }),
    )
}
