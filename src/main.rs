#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod events;
mod files;
mod job;
mod naming;
mod scraper;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Manga DL")
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([880.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Manga DL",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
