//! Sự kiện gửi từ luồng tải sang giao diện (thay cho WebSocket của bản Python).
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use eframe::egui;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Idle,
    Starting,
    Guard,
    Running,
    Stopping,
    Stopped,
    Done,
    Error,
    Webtoon,
}

impl Status {
    pub fn is_active(self) -> bool {
        matches!(self, Status::Starting | Status::Guard | Status::Running | Status::Stopping)
    }
    pub fn label(self) -> &'static str {
        match self {
            Status::Idle => "Sẵn sàng",
            Status::Starting => "Đang khởi tạo",
            Status::Guard => "Đang nhận diện reader",
            Status::Running => "Đang tải",
            Status::Stopping => "Đang dừng",
            Status::Stopped => "Đã dừng",
            Status::Done => "Hoàn tất",
            Status::Error => "Lỗi",
            Status::Webtoon => "Webtoon (không hỗ trợ)",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug)]
pub struct PageInfo {
    pub index: usize,
    pub name: String,
    pub path: PathBuf,
    pub width: usize,
    pub height: usize,
    pub size: u64,
}

pub struct Thumb {
    pub size: [usize; 2],
    pub rgba: Vec<u8>,
}

pub enum UiEvent {
    Status { status: Status, message: String, turn: Option<u32> },
    Log { ts: String, level: Level, message: String },
    Started { out_dir: PathBuf, zip_name: String },
    Page(PageInfo),
    Thumb { name: String, thumb: Thumb },
}

/// Kênh gửi sự kiện + đánh thức UI để vẽ lại.
#[derive(Clone)]
pub struct Sink {
    tx: Sender<UiEvent>,
    ctx: egui::Context,
}

impl Sink {
    pub fn new(tx: Sender<UiEvent>, ctx: egui::Context) -> Self {
        Self { tx, ctx }
    }

    pub fn send(&self, ev: UiEvent) {
        let _ = self.tx.send(ev);
        self.ctx.request_repaint();
    }

    pub fn status(&self, status: Status, message: impl Into<String>, turn: Option<u32>) {
        self.send(UiEvent::Status { status, message: message.into(), turn });
    }

    pub fn log(&self, level: Level, message: impl Into<String>) {
        let ts = chrono::Local::now().format("%H:%M:%S").to_string();
        self.send(UiEvent::Log { ts, level, message: message.into() });
    }

    pub fn info(&self, m: impl Into<String>) {
        self.log(Level::Info, m);
    }
    pub fn warn(&self, m: impl Into<String>) {
        self.log(Level::Warning, m);
    }
    pub fn error(&self, m: impl Into<String>) {
        self.log(Level::Error, m);
    }
    /// Log chi tiết: chỉ in ra stderr khi đặt biến môi trường MANGA_DL_DEBUG=1.
    pub fn debug(&self, m: impl AsRef<str>) {
        if std::env::var_os("MANGA_DL_DEBUG").is_some() {
            eprintln!("[debug] {}", m.as_ref());
        }
    }
}
