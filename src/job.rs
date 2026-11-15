//! Chạy một lượt tải trên tokio runtime, báo tiến độ về UI qua `Sink`.
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::runtime::Handle;
use tokio::sync::Notify;

use crate::config::Config;
use crate::events::{PageInfo, Sink, Status, Thumb, UiEvent};
use crate::files::{FileManager, SavedImage};
use crate::naming::derive_names;
use crate::scraper::browser::BrowserSession;
use crate::scraper::detector::WebtoonDetected;
use crate::scraper::extractor::{Extractor, ExtractorOpts};

#[derive(Debug, thiserror::Error)]
#[error("đã huỷ")]
struct Cancelled;

pub struct JobHandle {
    stop: Arc<AtomicBool>,
    force: Arc<Notify>,
    finished: Arc<AtomicBool>,
    sink: Sink,
}

impl JobHandle {
    /// Dừng êm: kết thúc sau bước hiện tại, giữ các trang đã lưu. Quá 20s thì huỷ cưỡng bức.
    pub fn request_stop(&self, rt: &Handle) {
        if self.stop.swap(true, Ordering::SeqCst) || self.finished.load(Ordering::SeqCst) {
            return;
        }
        self.sink.status(Status::Stopping, "Đang dừng, các trang đã lưu được giữ lại", None);
        let (force, finished, sink) =
            (Arc::clone(&self.force), Arc::clone(&self.finished), self.sink.clone());
        rt.spawn(async move {
            tokio::time::sleep(Duration::from_secs(20)).await;
            if !finished.load(Ordering::SeqCst) {
                sink.warn("Dừng êm quá 20s - huỷ cưỡng bức");
                force.notify_one();
            }
        });
    }
}

pub fn spawn(rt: &Handle, cfg: Config, sink: Sink) -> JobHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let force = Arc::new(Notify::new());
    let finished = Arc::new(AtomicBool::new(false));
    let handle = JobHandle {
        stop: Arc::clone(&stop),
        force: Arc::clone(&force),
        finished: Arc::clone(&finished),
        sink: sink.clone(),
    };
    rt.spawn(async move {
        run_job(cfg, sink, stop, force).await;
        finished.store(true, Ordering::SeqCst);
    });
    handle
}

fn make_thumb(path: &Path) -> Option<Thumb> {
    let img = image::open(path).ok()?;
    let small = img.thumbnail(360, 540).to_rgba8();
    Some(Thumb {
        size: [small.width() as usize, small.height() as usize],
        rgba: small.into_raw(),
    })
}

async fn run_job(cfg: Config, sink: Sink, stop: Arc<AtomicBool>, force: Arc<Notify>) {
    sink.status(Status::Starting, "Đang khởi tạo Chrome", None);
    sink.info(format!("Bắt đầu: {}", cfg.url));

    let session = match BrowserSession::launch(cfg.clone()).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            let text = first_line(&format!("{e:#}"));
            sink.error(text.clone());
            sink.status(Status::Error, text, None);
            return;
        }
    };

    let count = Arc::new(AtomicUsize::new(0));
    let result = tokio::select! {
        r = drive(&session, &cfg, &sink, &stop, &count) => r,
        _ = force.notified() => Err(anyhow!(Cancelled)),
    };
    session.close().await;

    let n = count.load(Ordering::SeqCst);
    match result {
        Err(e) if e.downcast_ref::<WebtoonDetected>().is_some() => {
            sink.warn(e.to_string());
            sink.status(Status::Webtoon, e.to_string(), None);
        }
        Err(e) if e.downcast_ref::<Cancelled>().is_some() => {
            sink.status(Status::Stopped, format!("Đã dừng, giữ lại {n} trang"), None);
        }
        Err(e) => {
            let text = first_line(&format!("{e:#}"));
            sink.error(text.clone());
            sink.status(Status::Error, text, None);
        }
        Ok(_) if stop.load(Ordering::SeqCst) => {
            sink.status(Status::Stopped, format!("Đã dừng, giữ lại {n} trang"), None);
        }
        Ok(_) if n == 0 => sink.status(
            Status::Error,
            "Không lưu được trang nào. Thử tắt Headless để xem trang, hoặc nạp cookies.",
            None,
        ),
        Ok(_) => sink.status(Status::Done, format!("Đã lưu {n} trang"), None),
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("Lỗi không xác định").chars().take(300).collect()
}

async fn drive(
    session: &Arc<BrowserSession>,
    cfg: &Config,
    sink: &Sink,
    stop: &Arc<AtomicBool>,
    count: &Arc<AtomicUsize>,
) -> Result<usize> {
    let page = session.open(&cfg.url, sink).await?;
    let title = page.get_title().await.ok().flatten().unwrap_or_default();
    let (manga, chapter) = derive_names(&cfg.url, &title);
    let out_dir = cfg.output_dir.join(&manga).join(&chapter);
    sink.send(UiEvent::Started { out_dir: out_dir.clone(), zip_name: format!("{manga}-{chapter}") });
    sink.info(format!("Thư mục lưu: {}", out_dir.display()));

    let files = Arc::new(FileManager::new(out_dir));
    let (cb_sink, cb_count) = (sink.clone(), Arc::clone(count));
    let on_saved = Box::new(move |item: SavedImage| {
        cb_count.fetch_add(1, Ordering::SeqCst);
        let name = item.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        cb_sink.send(UiEvent::Page(PageInfo {
            index: item.index,
            name: name.clone(),
            path: item.path.clone(),
            width: item.width,
            height: item.height,
            size: item.size,
        }));
        // Thumbnail tạo trên thread pool, không chặn vòng lật trang.
        let s = cb_sink.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(thumb) = make_thumb(&item.path) {
                s.send(UiEvent::Thumb { name, thumb });
            }
        });
    });

    let opts = ExtractorOpts {
        delay: Duration::from_millis(cfg.delay_ms),
        max_pages: cfg.max_pages,
        stagnation_limit: 3,
        strict: false,
        mode: cfg.mode,
    };
    let mut ex = Extractor::new(
        Arc::clone(session),
        page,
        Arc::clone(&files),
        sink.clone(),
        opts,
        Arc::clone(stop),
        on_saved,
    )
    .await;
    let saved = ex.run().await?;
    let st = files.stats().await;
    sink.info(format!(
        "Hoàn tất: {saved} trang (trùng: {}, nhỏ: {}, lỗi: {})",
        st.duplicate, st.small, st.invalid
    ));
    Ok(saved)
}
