#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auto_sync;
mod config;
mod tracker;
mod ui;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let config_path = config::default_config_path();
    let cfg = config::load_config(&config_path);

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Antigravity Usage")
        .with_inner_size([62.0, 62.0])
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(false)
        .with_taskbar(false);

    if cfg.always_on_top {
        viewport = viewport.with_always_on_top();
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Antigravity Usage",
        options,
        Box::new(|cc| {
            let mut visuals = egui::Visuals::dark();
            visuals.panel_fill = egui::Color32::TRANSPARENT;
            visuals.window_fill = egui::Color32::TRANSPARENT;
            cc.egui_ctx.set_visuals(visuals);
            Ok(Box::new(ui::UsageOverlayApp::new(config_path, cfg)))
        }),
    )
}
