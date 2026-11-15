//! Giao diện egui: form cấu hình, lưới ảnh thu nhỏ, nhật ký, nút mở thư mục / nén ZIP.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

use eframe::egui::{self, Color32, RichText};

use crate::config::{Config, Mode};
use crate::events::{Level, PageInfo, Sink, Status, UiEvent};
use crate::job::{self, JobHandle};

const MAX_LOGS: usize = 400;

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
}

fn install_fonts(ctx: &egui::Context) {
    // Font mặc định của egui thiếu một số dấu tiếng Việt xếp chồng -> ưu tiên font hệ thống nếu có.
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

fn fmt_bytes(b: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if b as f64 >= MB {
        format!("{:.1} MB", b as f64 / MB)
    } else {
        format!("{:.0} KB", b as f64 / 1024.0)
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
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
                // Ảnh đã nén sẵn -> chỉ lưu (Stored)
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

    fn form_ui(&mut self, ui: &mut egui::Ui) {
        let running = self.running();
        ui.add_space(6.0);
        ui.heading(RichText::new("Manga DL").color(Color32::from_rgb(240, 88, 62)));
        ui.add_space(8.0);

        ui.add_enabled_ui(!running, |ui| {
            ui.label("URL chapter");
            ui.add(
                egui::TextEdit::singleline(&mut self.url)
                    .hint_text("https://example.com/manga/ten-truyen/chapter-12")
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(6.0);

            ui.label("Chế độ tải");
            egui::ComboBox::from_id_salt("mode")
                .selected_text(self.mode.label())
                .width(260.0)
                .show_ui(ui, |ui| {
                    for m in Mode::ALL {
                        ui.selectable_value(&mut self.mode, m, m.label());
                    }
                });
            ui.add_space(6.0);

            ui.checkbox(&mut self.headless, "Chạy ẩn trình duyệt (Headless)");
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.label("Độ trễ sau mỗi lần lật");
                ui.add(egui::DragValue::new(&mut self.delay_ms).range(0..=30_000).suffix(" ms"));
            });
            ui.horizontal(|ui| {
                ui.label("Số lần lật tối đa");
                ui.add(egui::DragValue::new(&mut self.max_pages).range(1..=2000));
            });
            ui.add_space(6.0);

            ui.label("Thư mục lưu");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.output_dir).desired_width(200.0));
                if ui.button("Chọn…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.output_dir = dir.display().to_string();
                    }
                }
            });
            ui.add_space(6.0);

            egui::CollapsingHeader::new("Cookies JSON (tuỳ chọn)").show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.cookies)
                        .hint_text("[{\"name\":\"...\",\"value\":\"...\",\"domain\":\".site.com\"}]")
                        .desired_rows(5)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace),
                );
            });
        });

        ui.add_space(10.0);
        if let Some(err) = &self.form_error {
            ui.colored_label(Color32::from_rgb(240, 88, 62), err);
            ui.add_space(4.0);
        }
        ui.horizontal(|ui| {
            if running {
                let stopping = self.status == Status::Stopping;
                if ui.add_enabled(!stopping, egui::Button::new("⏹  Dừng")).clicked() {
                    if let Some(j) = &self.job {
                        j.request_stop(self.rt.handle());
                    }
                }
            } else if ui
                .add(egui::Button::new(RichText::new("▶  Bắt đầu tải").strong()).fill(Color32::from_rgb(168, 48, 29)))
                .clicked()
            {
                self.start();
            }
        });
        ui.add_space(8.0);
        ui.label(
            RichText::new("Cần cài sẵn Google Chrome / Chromium / Edge. Chỉ tải nội dung bạn có quyền và tôn trọng điều khoản của trang nguồn.")
                .small()
                .weak(),
        );
    }

    fn results_ui(&mut self, ui: &mut egui::Ui) {
        // Thanh trạng thái
        ui.horizontal_wrapped(|ui| {
            let color = match self.status {
                Status::Done => Color32::from_rgb(111, 191, 142),
                Status::Error | Status::Webtoon => Color32::from_rgb(240, 88, 62),
                Status::Stopped | Status::Stopping => Color32::from_rgb(242, 169, 59),
                _ => Color32::LIGHT_GRAY,
            };
            ui.label(RichText::new(self.status.label()).strong().color(color));
            if self.running() {
                ui.spinner();
            }
            ui.separator();
            ui.label(format!("{} trang", self.pages.len()));
            ui.label(fmt_bytes(self.bytes));
            ui.label(format!("{} lần lật", self.turn));
        });
        if !self.message.is_empty() {
            ui.label(RichText::new(&self.message).weak());
        }
        if let Some(dir) = &self.out_dir {
            ui.label(RichText::new(dir.display().to_string()).small().monospace().weak());
        }
        ui.horizontal(|ui| {
            let has_dir = self.out_dir.as_ref().map_or(false, |d| d.exists());
            if ui.add_enabled(has_dir, egui::Button::new("📂 Mở thư mục")).clicked() {
                if let Some(d) = &self.out_dir {
                    if let Err(e) = open::that(d) {
                        self.sink.error(format!("Không mở được thư mục: {e}"));
                    }
                }
            }
            if ui.add_enabled(!self.pages.is_empty(), egui::Button::new("🗜 Tải file .ZIP")).clicked() {
                self.save_zip();
            }
        });
        ui.separator();

        if self.pages.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Ảnh sẽ hiện ở đây ngay khi được lưu").weak());
            });
            return;
        }

        let mut open_path: Option<PathBuf> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                for p in &self.pages {
                    let width = 150.0;
                    let ratio = if p.width > 0 { p.height as f32 / p.width as f32 } else { 1.4 };
                    let size = egui::vec2(width, (width * ratio).clamp(120.0, 260.0));
                    ui.vertical(|ui| {
                        let resp = match self.thumbs.get(&p.name) {
                            Some(tex) => ui.add(
                                egui::Image::new(tex).fit_to_exact_size(size).sense(egui::Sense::click()),
                            ),
                            None => {
                                let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
                                ui.painter().rect_filled(rect, 6.0, Color32::from_gray(28));
                                resp
                            }
                        }
                        .on_hover_text("Bấm để xem cỡ đầy đủ");
                        if resp.clicked() {
                            open_path = Some(p.path.clone());
                        }
                        ui.label(RichText::new(format!("{} · {}×{}", p.name, p.width, p.height)).small().weak());
                    });
                }
            });
        });
        if let Some(path) = open_path {
            if let Err(e) = open::that(&path) {
                self.sink.error(format!("Không mở được ảnh: {e}"));
            }
        }
    }

    fn log_ui(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            for l in &self.logs {
                let color = match l.level {
                    Level::Info => Color32::from_gray(190),
                    Level::Warning => Color32::from_rgb(242, 169, 59),
                    Level::Error => Color32::from_rgb(240, 88, 62),
                };
                ui.label(RichText::new(format!("{}  {}", l.ts, l.message)).monospace().small().color(color));
            }
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events(ctx);

        egui::SidePanel::left("config").exact_width(330.0).resizable(false).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.form_ui(ui));
        });
        egui::TopBottomPanel::bottom("log")
            .resizable(true)
            .default_height(150.0)
            .min_height(60.0)
            .show(ctx, |ui| {
                ui.label(RichText::new("Nhật ký").strong());
                self.log_ui(ui);
            });
        egui::CentralPanel::default().show(ctx, |ui| self.results_ui(ui));
    }
}
