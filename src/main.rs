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
            .with_title("Manga DL — Trình tải truyện tranh & Webtoon")
            .with_inner_size([1220.0, 800.0])
            .with_min_inner_size([940.0, 620.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Manga DL",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
