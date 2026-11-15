//! Giao diện egui: form cấu hình, lưới ảnh thu nhỏ, nhật ký, nút mở thư mục / nén ZIP.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

use eframe::egui::{self, Color32, Margin, RichText, Rounding, Stroke, Vec2};

use crate::config::{Config, Mode};
use crate::events::{Level, PageInfo, Sink, Status, UiEvent};
use crate::job::{self, JobHandle};

const MAX_LOGS: usize = 400;

mod theme {
    use super::*;

    pub const BG_APP: Color32 = Color32::from_rgb(15, 17, 23);       // #0F1117 (Nền ứng dụng chính)
    pub const BG_SIDEBAR: Color32 = Color32::from_rgb(20, 23, 31);   // #14171F (Nền thanh cấu hình)
    pub const BG_CARD: Color32 = Color32::from_rgb(27, 31, 43);      // #1B1F2B (Khối card)
    pub const BG_CARD_HOVER: Color32 = Color32::from_rgb(35, 41, 57);
    pub const BG_INPUT: Color32 = Color32::from_rgb(12, 14, 19);     // #0C0E13 (Ô input)
    pub const BG_BAR: Color32 = Color32::from_rgb(22, 26, 36);       // #161A24 (Thanh công cụ đỉnh)

    pub const BORDER: Color32 = Color32::from_rgb(42, 48, 66);       // #2A3042 (Đường viền ngăn cách)
    pub const BORDER_FOCUS: Color32 = Color32::from_rgb(229, 72, 77); // Torii crimson khi focus

    pub const ACCENT_PRIMARY: Color32 = Color32::from_rgb(229, 72, 77); // #E5484D (Manga Torii Crimson)
    pub const ACCENT_SUCCESS: Color32 = Color32::from_rgb(48, 164, 108); // #30A46C (Emerald)
    pub const ACCENT_WARN: Color32 = Color32::from_rgb(247, 107, 21);    // #F76B15 (Amber)
    pub const ACCENT_INFO: Color32 = Color32::from_rgb(59, 130, 246);    // #3B82F6 (Sky Blue)

    pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(243, 244, 246);
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(156, 163, 175);
    pub const TEXT_FAINT: Color32 = Color32::from_rgb(100, 110, 126);

    pub const ROUNDING_SM: Rounding = Rounding::same(6.0);
    pub const ROUNDING_MD: Rounding = Rounding::same(8.0);
    pub const ROUNDING_LG: Rounding = Rounding::same(12.0);

    pub fn card_frame() -> egui::Frame {
        egui::Frame::none()
            .fill(BG_CARD)
            .stroke(Stroke::new(1.0_f32, BORDER))
            .rounding(ROUNDING_MD)
            .inner_margin(Margin { left: 14.0, right: 14.0, top: 12.0, bottom: 12.0 })
    }

    pub fn panel_frame() -> egui::Frame {
        egui::Frame::none()
            .fill(BG_SIDEBAR)
            .inner_margin(Margin { left: 16.0, right: 16.0, top: 16.0, bottom: 16.0 })
    }
}

struct LogLine {
    ts: String,
    level: Level,
    message: String,
}

pub struct App {
    rt: tokio::runtime::Runtime,
    rx: Receiver<UiEvent>,
    sink: Sink,
    job: Option<JobHandle>,

    // form
    url: String,
    mode: Mode,
    headless: bool,
    delay_ms: u64,
    max_pages: u32,
    output_dir: String,
    cookies: String,
    form_error: Option<String>,

    // trạng thái job
    status: Status,
    message: String,
    turn: u32,
    bytes: u64,
    out_dir: Option<PathBuf>,
    zip_name: String,
    pages: Vec<PageInfo>,
    thumbs: HashMap<String, egui::TextureHandle>,
    logs: Vec<LogLine>,

    // tuỳ chọn hiển thị
    show_logs: bool,
}

fn install_fonts(ctx: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\segoeui.ttf",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/Library/Fonts/Arial Unicode.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ];
    if let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert("vi-system".into(), egui::FontData::from_owned(bytes));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            if let Some(list) = fonts.families.get_mut(&family) {
                list.insert(0, "vi-system".into());
            }
        }
        ctx.set_fonts(fonts);
    }
}

fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.override_text_color = Some(theme::TEXT_PRIMARY);
    visuals.panel_fill = theme::BG_SIDEBAR;
    visuals.window_fill = theme::BG_APP;
    visuals.extreme_bg_color = theme::BG_INPUT;
    visuals.faint_bg_color = theme::BG_CARD;

    visuals.widgets.noninteractive.bg_fill = theme::BG_CARD;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, theme::BORDER);
    visuals.widgets.noninteractive.rounding = theme::ROUNDING_MD;
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, theme::TEXT_PRIMARY);

    visuals.widgets.inactive.bg_fill = theme::BG_CARD;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, theme::BORDER);
    visuals.widgets.inactive.rounding = theme::ROUNDING_MD;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, theme::TEXT_PRIMARY);

    visuals.widgets.hovered.bg_fill = theme::BG_CARD_HOVER;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, theme::BORDER_FOCUS);
    visuals.widgets.hovered.rounding = theme::ROUNDING_MD;
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);

    visuals.widgets.active.bg_fill = theme::ACCENT_PRIMARY;
    visuals.widgets.active.bg_stroke = Stroke::new(1.5_f32, Color32::WHITE);
    visuals.widgets.active.rounding = theme::ROUNDING_MD;
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);

    visuals.selection.bg_fill = theme::ACCENT_PRIMARY;
    visuals.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);

    visuals.window_rounding = theme::ROUNDING_LG;
    visuals.menu_rounding = theme::ROUNDING_MD;

    ctx.set_visuals(visuals);

    ctx.style_mut(|s| {
        s.spacing.item_spacing = Vec2::new(8.0, 8.0);
        s.spacing.button_padding = Vec2::new(12.0, 7.0);
        s.spacing.window_margin = Margin { left: 12.0, right: 12.0, top: 12.0, bottom: 12.0 };
    });
}

fn fmt_bytes(b: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if b as f64 >= MB {
        format!("{:.2} MB", b as f64 / MB)
    } else {
        format!("{:.0} KB", b as f64 / 1024.0)
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        apply_theme(&cc.egui_ctx);

        let (tx, rx) = channel();
        Self {
            rt: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("không tạo được tokio runtime"),
            rx,
            sink: Sink::new(tx, cc.egui_ctx.clone()),
            job: None,
            url: String::new(),
            mode: Mode::Auto,
            headless: true,
            delay_ms: 800,
            max_pages: 500,
            output_dir: "downloads".into(),
            cookies: String::new(),
            form_error: None,

            status: Status::Idle,
            message: String::new(),
            turn: 0,
            bytes: 0,
            out_dir: None,
            zip_name: "chapter".into(),
            pages: Vec::new(),
            thumbs: HashMap::new(),
            logs: Vec::new(),
            show_logs: false,
        }
    }

    fn running(&self) -> bool {
        self.status.is_active()
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                UiEvent::Status { status, message, turn } => {
                    self.status = status;
                    self.message = message;
                    if let Some(t) = turn {
                        self.turn = t;
                    }
                    if !status.is_active() {
                        self.job = None;
                    }
                }
                UiEvent::Log { ts, level, message } => {
                    self.logs.push(LogLine { ts, level, message });
                    if self.logs.len() > MAX_LOGS {
                        self.logs.remove(0);
                    }
                }
                UiEvent::Started { out_dir, zip_name } => {
                    self.out_dir = Some(out_dir);
                    self.zip_name = zip_name;
                }
                UiEvent::Page(info) => {
                    self.bytes += info.size;
                    self.pages.push(info);
                }
                UiEvent::Thumb { name, thumb } => {
                    let img = egui::ColorImage::from_rgba_unmultiplied(thumb.size, &thumb.rgba);
                    let tex = ctx.load_texture(format!("thumb-{name}"), img, egui::TextureOptions::LINEAR);
                    self.thumbs.insert(name, tex);
                }
            }
        }
    }

    fn start(&mut self) {
        match Config::from_form(
            &self.url,
            self.mode,
            self.headless,
            self.delay_ms,
            self.max_pages,
            &self.cookies,
            &self.output_dir,
        ) {
            Err(e) => self.form_error = Some(e),
            Ok(cfg) => {
                self.form_error = None;
                self.pages.clear();
                self.thumbs.clear();
                self.logs.clear();
                self.bytes = 0;
                self.turn = 0;
                self.out_dir = None;
                self.status = Status::Starting;
                self.message = String::new();
                self.job = Some(job::spawn(self.rt.handle(), cfg, self.sink.clone()));
            }
        }
    }

    fn save_zip(&self) {
        let paths: Vec<PathBuf> = self.pages.iter().map(|p| p.path.clone()).filter(|p| p.is_file()).collect();
        if paths.is_empty() {
            self.sink.warn("Các file ảnh không còn trên đĩa");
            return;
        }
        let base = self.zip_name.clone();
        let Some(dest) = rfd::FileDialog::new()
            .set_file_name(format!("{base}.zip"))
            .add_filter("ZIP", &["zip"])
            .save_file()
        else {
            return;
        };
        let sink = self.sink.clone();
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<()> {
                let mut zw = zip::ZipWriter::new(std::fs::File::create(&dest)?);
                let opts = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                for p in &paths {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    zw.start_file(format!("{base}/{name}"), opts)?;
                    std::io::copy(&mut std::fs::File::open(p)?, &mut zw)?;
                }
                zw.finish()?;
                Ok(())
            })();
            match res {
                Ok(()) => sink.info(format!("Đã nén {} trang -> {}", paths.len(), dest.display())),
                Err(e) => sink.error(format!("Không nén được ZIP: {e}")),
            }
        });
    }

    // ==========================================
    // SIDEBAR CONFIG FORM
    // ==========================================
    fn form_ui(&mut self, ui: &mut egui::Ui) {
        let running = self.running();

        // 1. Header Thương hiệu
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let (rect, _resp) = ui.allocate_exact_size(Vec2::new(32.0, 32.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, theme::ROUNDING_SM, theme::ACCENT_PRIMARY);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "⛩",
                egui::FontId::proportional(18.0),
                Color32::WHITE,
            );

            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("MANGA DL").strong().size(16.0).color(theme::TEXT_PRIMARY));
                    let (badge_rect, _) = ui.allocate_exact_size(Vec2::new(38.0, 16.0), egui::Sense::hover());
                    ui.painter().rect_filled(badge_rect, Rounding::same(4.0), Color32::from_rgb(45, 25, 30));
                    ui.painter().text(
                        badge_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "v0.1.0",
                        egui::FontId::proportional(10.0),
                        theme::ACCENT_PRIMARY,
                    );
                });
                ui.label(RichText::new("Trình tải Manga & Webtoon chất lượng cao").size(11.0).color(theme::TEXT_MUTED));
            });
        });

        ui.add_space(14.0);

        // 2. Card: Nguồn tải & Chế độ
        theme::card_frame().show(ui, |ui| {
            ui.label(RichText::new("NGUỒN TRUYỆN & CHẾ ĐỘ").size(11.0).strong().color(theme::TEXT_FAINT));
            ui.add_space(6.0);

            ui.add_enabled_ui(!running, |ui| {
                ui.label(RichText::new("URL Chapter").size(12.0).strong().color(theme::TEXT_MUTED));
                ui.add(
                    egui::TextEdit::singleline(&mut self.url)
                        .hint_text("https://example.com/truyen/.../chapter-1")
                        .desired_width(f32::INFINITY)
                        .margin(Margin { left: 8.0, right: 8.0, top: 6.0, bottom: 6.0 }),
                );
                ui.add_space(6.0);

                ui.label(RichText::new("Phương thức nhận diện").size(12.0).strong().color(theme::TEXT_MUTED));
                egui::ComboBox::from_id_salt("mode_selector")
                    .selected_text(match self.mode {
                        Mode::Auto => "⚡ Tự động (Khuyên dùng)",
                        Mode::Manga => "📖 Manga (Lật từng trang)",
                        Mode::Webtoon => "📜 Webtoon (Cuộn dọc liên tục)",
                    })
                    .width(ui.available_width() - 8.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.mode, Mode::Auto, "⚡ Tự động (Khuyên dùng)");
                        ui.selectable_value(&mut self.mode, Mode::Manga, "📖 Manga (Lật từng trang)");
                        ui.selectable_value(&mut self.mode, Mode::Webtoon, "📜 Webtoon (Cuộn dọc liên tục)");
                    });
            });
        });

        ui.add_space(10.0);

        // 3. Card: Tuỳ chọn tải
        theme::card_frame().show(ui, |ui| {
            ui.label(RichText::new("THIẾT LẬP NÂNG CAO").size(11.0).strong().color(theme::TEXT_FAINT));
            ui.add_space(6.0);

            ui.add_enabled_ui(!running, |ui| {
                ui.checkbox(&mut self.headless, "Chạy ẩn trình duyệt (Headless)")
                    .on_hover_text("Tắt để hiện cửa sổ Chrome, hữu ích khi cần vượt Cloudflare hoặc CAPTCHA");

                ui.add_space(4.0);
                ui.columns(2, |cols| {
                    cols[0].vertical(|ui| {
                        ui.label(RichText::new("Độ trễ lật").size(11.0).color(theme::TEXT_MUTED));
                        ui.add(egui::DragValue::new(&mut self.delay_ms).range(200..=15_000).suffix(" ms"));
                    });
                    cols[1].vertical(|ui| {
                        ui.label(RichText::new("Số trang tối đa").size(11.0).color(theme::TEXT_MUTED));
                        ui.add(egui::DragValue::new(&mut self.max_pages).range(1..=1000));
                    });
                });

                ui.add_space(6.0);
                ui.label(RichText::new("Thư mục lưu trữ").size(11.0).color(theme::TEXT_MUTED));
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.output_dir)
                            .desired_width(ui.available_width() - 85.0)
                            .margin(Margin { left: 6.0, right: 6.0, top: 4.0, bottom: 4.0 }),
                    );
                    if ui.button("📁 Chọn…").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.output_dir = dir.display().to_string();
                        }
                    }
                });

                ui.add_space(6.0);
                let cookies_label = if self.cookies.trim().is_empty() {
                    "🔑 Cookies JSON (Trống)"
                } else {
                    "🔑 Cookies JSON (Đã cấu hình)"
                };
                egui::CollapsingHeader::new(RichText::new(cookies_label).size(11.0).color(theme::TEXT_MUTED)).show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.cookies)
                            .hint_text("[{\"name\":\"...\",\"value\":\"...\",\"domain\":\".domain.com\"}]")
                            .desired_rows(4)
                            .desired_width(f32::INFINITY)
                            .font(egui::TextStyle::Monospace),
                    );
                });
            });
        });

        ui.add_space(14.0);

        // 4. Lỗi nhập liệu (nếu có)
        if let Some(err) = &self.form_error {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, theme::ROUNDING_SM, Color32::from_rgb(60, 20, 25));
            ui.painter().rect_stroke(rect, theme::ROUNDING_SM, Stroke::new(1.0_f32, theme::ACCENT_PRIMARY));
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                format!("⚠ {err}"),
                egui::FontId::proportional(12.0),
                theme::TEXT_PRIMARY,
            );
            ui.add_space(8.0);
        }

        // 5. Nút Thao tác Chính (CTA)
        let btn_height = 42.0;
        let btn_width = ui.available_width();
        if running {
            let stopping = self.status == Status::Stopping;
            let btn_text = if stopping { "⏳ ĐANG DỪNG TIẾN TRÌNH..." } else { "⏹ DỪNG TẢI TRUYỆN" };
            let resp = ui.add_sized(
                [btn_width, btn_height],
                egui::Button::new(RichText::new(btn_text).size(14.0).strong().color(Color32::WHITE))
                    .fill(if stopping { Color32::from_rgb(80, 50, 20) } else { theme::ACCENT_WARN })
                    .rounding(theme::ROUNDING_MD),
            );
            if resp.clicked() && !stopping {
                if let Some(j) = &self.job {
                    j.request_stop(self.rt.handle());
                }
            }
        } else {
            let resp = ui.add_sized(
                [btn_width, btn_height],
                egui::Button::new(RichText::new("▶  BẮT ĐẦU TẢI TRUYỆN").size(14.0).strong().color(Color32::WHITE))
                    .fill(theme::ACCENT_PRIMARY)
                    .rounding(theme::ROUNDING_MD),
            );
            if resp.clicked() {
                self.start();
            }
        }

        ui.add_space(12.0);
        ui.label(
            RichText::new("Yêu cầu Chrome / Edge / Chromium. Vui lòng tôn trọng bản quyền và điều khoản của trang nguồn.")
                .size(10.0)
                .color(theme::TEXT_FAINT),
        );
    }

    // ==========================================
    // TOP TOOLBAR & STATS BAR
    // ==========================================
    fn top_bar_ui(&mut self, ui: &mut egui::Ui) {
        let (rect, _resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 52.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, Rounding::ZERO, theme::BG_BAR);
        ui.painter().line_segment(
            [rect.left_bottom(), rect.right_bottom()],
            Stroke::new(1.0_f32, theme::BORDER),
        );

        let status_color = match self.status {
            Status::Done => theme::ACCENT_SUCCESS,
            Status::Running | Status::Guard | Status::Starting => theme::ACCENT_INFO,
            Status::Error | Status::Webtoon => theme::ACCENT_PRIMARY,
            Status::Stopped | Status::Stopping => theme::ACCENT_WARN,
            Status::Idle => theme::TEXT_MUTED,
        };

        ui.allocate_new_ui(
            egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(16.0, 10.0))),
            |ui| {
                ui.horizontal(|ui| {
                    // Chip trạng thái
                    let (badge_rect, _) = ui.allocate_exact_size(Vec2::new(130.0, 28.0), egui::Sense::hover());
                    ui.painter().rect_filled(badge_rect, Rounding::same(14.0), Color32::from_rgb(32, 38, 52));
                    ui.painter().circle_filled(
                        badge_rect.left_center() + Vec2::new(14.0, 0.0),
                        4.0,
                        status_color,
                    );
                    ui.painter().text(
                        badge_rect.left_center() + Vec2::new(26.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        self.status.label(),
                        egui::FontId::proportional(12.0),
                        theme::TEXT_PRIMARY,
                    );

                    if self.running() {
                        ui.add_space(4.0);
                        ui.spinner();
                    }

                    ui.add_space(8.0);

                    // Các chỉ số thống kê
                    let stat_pill = |ui: &mut egui::Ui, icon: &str, label: String| {
                    let text = format!("{icon}  {label}");
                    let galley = ui.painter().layout_no_wrap(
                        text,
                        egui::FontId::proportional(12.0),
                        theme::TEXT_PRIMARY,
                    );
                    let w = galley.size().x + 20.0;
                    let (r, _) = ui.allocate_exact_size(Vec2::new(w, 28.0), egui::Sense::hover());
                    ui.painter().rect_filled(r, Rounding::same(6.0), Color32::from_rgb(26, 30, 42));
                    ui.painter().rect_stroke(r, Rounding::same(6.0), Stroke::new(1.0_f32, theme::BORDER));
                    ui.painter().galley(r.left_center() + Vec2::new(10.0, -galley.size().y / 2.0), galley, theme::TEXT_PRIMARY);
                };

                stat_pill(ui, "📄", format!("{} trang", self.pages.len()));
                stat_pill(ui, "💾", fmt_bytes(self.bytes));
                if self.turn > 0 {
                    stat_pill(ui, "🔄", format!("{} lần lật", self.turn));
                }

                // Nhóm nút thao tác bên phải
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let log_btn_text = if self.show_logs { "📋 Ẩn nhật ký" } else { "📋 Xem nhật ký" };
                    if ui.add(egui::Button::new(RichText::new(log_btn_text).size(12.0)).rounding(theme::ROUNDING_SM)).clicked() {
                        self.show_logs = !self.show_logs;
                    }

                    let has_pages = !self.pages.is_empty();
                    if ui.add_enabled(
                        has_pages,
                        egui::Button::new(RichText::new("🗜 Nén ZIP").size(12.0)).rounding(theme::ROUNDING_SM),
                    ).clicked() {
                        self.save_zip();
                    }

                    let has_dir = self.out_dir.as_ref().map_or(false, |d| d.exists());
                    if ui.add_enabled(
                        has_dir,
                        egui::Button::new(RichText::new("📂 Mở thư mục").size(12.0)).rounding(theme::ROUNDING_SM),
                    ).clicked() {
                        if let Some(d) = &self.out_dir {
                            let _ = open::that(d);
                        }
                    }
                });
            });
        });
    }

    // ==========================================
    // GALLERY & READER GRID
    // ==========================================
    fn gallery_ui(&mut self, ui: &mut egui::Ui) {
        if self.pages.is_empty() {
            // EMPTY STATE THIẾT KẾ ĐẸP MẮT
            ui.vertical_centered(|ui| {
                ui.add_space(100.0);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(72.0, 72.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, Rounding::same(36.0), Color32::from_rgb(26, 31, 44));
                ui.painter().rect_stroke(rect, Rounding::same(36.0), Stroke::new(1.5_f32, theme::BORDER));
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "🖼",
                    egui::FontId::proportional(32.0),
                    theme::TEXT_PRIMARY,
                );

                ui.add_space(16.0);
                ui.label(RichText::new("Chưa có trang truyện nào được tải").size(17.0).strong().color(theme::TEXT_PRIMARY));
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Dán đường dẫn chapter vào khung bên trái và bấm 'Bắt đầu tải'")
                        .size(13.0)
                        .color(theme::TEXT_MUTED),
                );

                ui.add_space(20.0);
                let (card_rect, _) = ui.allocate_exact_size(Vec2::new(420.0, 80.0), egui::Sense::hover());
                ui.painter().rect_filled(card_rect, theme::ROUNDING_MD, theme::BG_CARD);
                ui.painter().rect_stroke(card_rect, theme::ROUNDING_MD, Stroke::new(1.0_f32, theme::BORDER));
                ui.allocate_new_ui(
                    egui::UiBuilder::new().max_rect(card_rect.shrink(12.0)),
                    |ui| {
                        ui.label(RichText::new("💡 Tính năng nổi bật:").strong().size(12.0).color(theme::ACCENT_PRIMARY));
                        ui.add_space(4.0);
                        ui.label(RichText::new("• Tự động chống trùng ảnh và tải ảnh chất lượng gốc").size(11.0).color(theme::TEXT_MUTED));
                        ui.label(RichText::new("• Hỗ trợ cả truyện Manga lật trang & truyện Webtoon cuộn dài").size(11.0).color(theme::TEXT_MUTED));
                    },
                );
            });
            return;
        }

        // LƯỚI CARD ẢNH MANGA
        let mut open_path: Option<PathBuf> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(14.0, 14.0);
                let col_width = 170.0;

                for (idx, p) in self.pages.iter().enumerate() {
                    let ratio = if p.width > 0 { p.height as f32 / p.width as f32 } else { 1.42 };
                    let img_h = (col_width * ratio).clamp(130.0, 260.0);
                    let card_h = img_h + 46.0;

                    let (card_rect, resp) = ui.allocate_exact_size(Vec2::new(col_width, card_h), egui::Sense::click());
                    let is_hovered = resp.hovered();

                    // Nền card
                    let bg_fill = if is_hovered { theme::BG_CARD_HOVER } else { theme::BG_CARD };
                    let border_color = if is_hovered { theme::ACCENT_PRIMARY } else { theme::BORDER };
                    ui.painter().rect_filled(card_rect, theme::ROUNDING_MD, bg_fill);
                    ui.painter().rect_stroke(card_rect, theme::ROUNDING_MD, Stroke::new(1.0_f32, border_color));

                    // Vùng ảnh
                    let img_rect = egui::Rect::from_min_size(card_rect.min + Vec2::new(6.0, 6.0), Vec2::new(col_width - 12.0, img_h));
                    if let Some(tex) = self.thumbs.get(&p.name) {
                        egui::Image::new(tex)
                            .rounding(theme::ROUNDING_SM)
                            .paint_at(ui, img_rect);
                    } else {
                        ui.painter().rect_filled(img_rect, theme::ROUNDING_SM, Color32::from_rgb(18, 20, 28));
                        ui.painter().text(
                            img_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "⌛",
                            egui::FontId::proportional(20.0),
                            theme::TEXT_MUTED,
                        );
                    }

                    // Badge số trang (Style Manga Reader)
                    let page_badge_rect = egui::Rect::from_min_size(img_rect.min + Vec2::new(6.0, 6.0), Vec2::new(42.0, 18.0));
                    ui.painter().rect_filled(page_badge_rect, Rounding::same(4.0), Color32::from_black_alpha(190));
                    ui.painter().text(
                        page_badge_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        format!("#{:02}", idx + 1),
                        egui::FontId::proportional(10.0),
                        Color32::WHITE,
                    );

                    // Thông tin ảnh bên dưới
                    let info_y = img_rect.max.y + 6.0;
                    ui.painter().text(
                        egui::pos2(card_rect.min.x + 8.0, info_y),
                        egui::Align2::LEFT_TOP,
                        &p.name,
                        egui::FontId::proportional(11.0),
                        theme::TEXT_PRIMARY,
                    );

                    let size_str = format!("{}×{} · {}", p.width, p.height, fmt_bytes(p.size));
                    ui.painter().text(
                        egui::pos2(card_rect.min.x + 8.0, info_y + 16.0),
                        egui::Align2::LEFT_TOP,
                        size_str,
                        egui::FontId::proportional(10.0),
                        theme::TEXT_MUTED,
                    );

                    if resp.clicked() {
                        open_path = Some(p.path.clone());
                    }
                }
            });
            ui.add_space(20.0);
        });

        if let Some(path) = open_path {
            let _ = open::that(&path);
        }
    }

    // ==========================================
    // LOG DRAWER
    // ==========================================
    fn log_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("NHẬT KÝ HỆ THỐNG").size(11.0).strong().color(theme::TEXT_FAINT));
            ui.label(RichText::new(format!("({} dòng)", self.logs.len())).size(11.0).color(theme::TEXT_MUTED));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✕ Đóng").clicked() {
                    self.show_logs = false;
                }
                if ui.button("🗑 Xóa").clicked() {
                    self.logs.clear();
                }
            });
        });
        ui.add_space(4.0);

        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            for l in &self.logs {
                let (color, level_str) = match l.level {
                    Level::Info => (theme::TEXT_MUTED, "INFO"),
                    Level::Warning => (theme::ACCENT_WARN, "WARN"),
                    Level::Error => (theme::ACCENT_PRIMARY, "ERR "),
                };
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&l.ts).size(11.0).monospace().color(theme::TEXT_FAINT));
                    ui.label(RichText::new(level_str).size(10.0).monospace().color(color));
                    ui.label(RichText::new(&l.message).size(11.0).monospace().color(theme::TEXT_PRIMARY));
                });
            }
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Luôn đảm bảo giao diện dùng theme dark cao cấp
        apply_theme(ctx);
        self.drain_events(ctx);

        // Sidebar bên trái
        egui::SidePanel::left("config_panel")
            .exact_width(340.0)
            .resizable(false)
            .frame(theme::panel_frame())
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.form_ui(ui));
            });

        // Panel nhật ký (chỉ hiện khi người dùng mở)
        if self.show_logs {
            egui::TopBottomPanel::bottom("log_panel")
                .resizable(true)
                .default_height(160.0)
                .min_height(80.0)
                .max_height(350.0)
                .frame(egui::Frame::none().fill(theme::BG_BAR).stroke(Stroke::new(1.0_f32, theme::BORDER)).inner_margin(Margin { left: 16.0, right: 16.0, top: 10.0, bottom: 10.0 }))
                .show(ctx, |ui| {
                    self.log_ui(ui);
                });
        }

        // Vùng trung tâm
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(theme::BG_APP))
            .show(ctx, |ui| {
                self.top_bar_ui(ui);
                self.gallery_ui(ui);
            });
    }
}
