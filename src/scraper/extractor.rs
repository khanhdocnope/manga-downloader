//! Vòng lặp lật trang + trích xuất ảnh (network buffer -> DOM/reqwest -> canvas).
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chromiumoxide::Page;
use futures::future::join_all;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::Mode;
use crate::events::{Sink, Status};
use crate::files::{FileManager, SavedImage};
use crate::scraper::browser::{self, eval_fn, BrowserSession};
use crate::scraper::detector::{detect_reader, ReadingDirection, WebtoonDetected};

const VISIBLE_MEDIA_JS: &str = r##"
async () => {
  const vw = window.innerWidth, vh = window.innerHeight;
  const out = [];
  const inView = r => {
    const visibleH = Math.max(0, Math.min(r.bottom, vh) - Math.max(r.top, 0));
    const visibleW = Math.max(0, Math.min(r.right, vw) - Math.max(r.left, 0));
    return visibleH >= 30 && visibleW >= 100;
  };
  const toB64 = blob => new Promise((res, rej) => {
    const fr = new FileReader();
    fr.onload = () => res(String(fr.result).split(',')[1]);
    fr.onerror = rej;
    fr.readAsDataURL(blob);
  });
  for (const el of document.querySelectorAll('img, canvas')) {
    const r = el.getBoundingClientRect();
    if (r.width < 200 || r.height < 200) continue;
    const cs = getComputedStyle(el);
    if (cs.visibility === 'hidden' || cs.display === 'none' || parseFloat(cs.opacity) === 0) continue;
    if (inView(r) < 0.1) continue;
    const item = {kind: el.tagName.toLowerCase(), left: r.left, top: r.top,
                  width: r.width, height: r.height, src: null, b64: null};
    if (el.tagName === 'IMG') {
      if (!el.complete || el.naturalWidth === 0) continue;
      item.src = el.currentSrc || el.src;
      if (item.src.startsWith('blob:') || item.src.startsWith('data:')) {
        try { item.b64 = await toB64(await (await fetch(item.src)).blob()); } catch (e) {}
      }
    } else {
      try { item.b64 = el.toDataURL('image/png').split(',')[1]; } catch (e) { continue; }
    }
    out.push(item);
  }
  return out;
}
"##;

const WAIT_IMAGES_JS: &str = r##"
() => {
  const vis = Array.from(document.querySelectorAll('img')).filter(i => {
    const r = i.getBoundingClientRect();
    return r.width >= 200 && r.height >= 200 && r.bottom > 0 && r.top < innerHeight;
  });
  return vis.every(i => i.complete && i.naturalWidth > 0);
}
"##;

const SCROLL_JS: &str = r##"
() => {
  const imgs = Array.from(document.querySelectorAll('img, canvas')).filter(el => {
    const r = el.getBoundingClientRect();
    return (el.naturalWidth >= 200 || el.width >= 200 || r.width >= 200);
  });
  const nextImg = imgs.find(el => el.getBoundingClientRect().top > 120);
  if (nextImg) {
    nextImg.scrollIntoView({ behavior: 'instant', block: 'start' });
    return false;
  }
  const bottom = (window.innerHeight + window.scrollY) >= (document.body.scrollHeight - 50);
  if (!bottom) {
    window.scrollBy(0, Math.floor(window.innerHeight * 0.85));
  }
  return bottom;
}
"##;

#[derive(Deserialize, Clone, Debug)]
pub struct Media {
    kind: String,
    left: f64,
    top: f64,
    width: f64,
    height: f64,
    src: Option<String>,
    b64: Option<String>,
}

impl Media {
    fn key(&self) -> String {
        if let Some(src) = &self.src {
            if !src.starts_with("blob:") && !src.starts_with("data:") {
                return src.clone();
            }
        }
        if let Some(b64) = &self.b64 {
            return format!("h:{:x}", Sha256::digest(b64.as_bytes()));
        }
        format!("{}:{}", self.kind, self.src.clone().unwrap_or_default())
    }
}

type Signature = Vec<String>;

fn round100(x: f64) -> i64 {
    (x / 100.0).round() as i64
}

fn signature(media: &[Media]) -> Signature {
    let mut v: Vec<&Media> = media.iter().collect();
    v.sort_by_key(|m| (round100(m.top), round100(m.left)));
    v.into_iter().map(Media::key).collect()
}

fn url_base(u: &str) -> String {
    u.split('?').next().unwrap_or("").split('#').next().unwrap_or("").trim_end_matches('/').to_string()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Key,
    Click,
    Scroll,
}

pub struct ExtractorOpts {
    pub delay: Duration,
    pub max_pages: u32,
    pub stagnation_limit: u32,
    pub strict: bool,
    pub mode: Mode,
}

pub type SavedCallback = Box<dyn Fn(SavedImage) + Send + Sync>;

pub struct Extractor {
    session: Arc<BrowserSession>,
    page: Page,
    files: Arc<FileManager>,
    sink: Sink,
    opts: ExtractorOpts,
    stop: Arc<AtomicBool>,
    on_saved: SavedCallback,
    direction: ReadingDirection,
    turn_action: Option<Action>,
    consumed: HashSet<String>,
    initial_url: String,
}

impl Extractor {
    pub async fn new(
        session: Arc<BrowserSession>,
        page: Page,
        files: Arc<FileManager>,
        sink: Sink,
        opts: ExtractorOpts,
        stop: Arc<AtomicBool>,
        on_saved: SavedCallback,
    ) -> Self {
        let initial_url = page.url().await.ok().flatten().unwrap_or_default();
        let turn_action = if opts.mode == Mode::Webtoon { Some(Action::Scroll) } else { None };
        Self {
            session,
            page,
            files,
            sink,
            opts,
            stop,
            on_saved,
            direction: ReadingDirection::Rtl,
            turn_action,
            consumed: HashSet::new(),
            initial_url,
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn status(&self, status: Status, turn: Option<u32>) {
        if !self.stopped() {
            self.sink.status(status, "", turn);
        }
    }

    /// Trả về số ảnh đã lưu. WebtoonDetected được trả lên cho caller xử lý.
    pub async fn run(&mut self) -> Result<usize> {
        self.status(Status::Guard, None);
        let info = detect_reader(&self.page, &self.sink)
            .await
            .map_err(anyhow::Error::from)?;
        self.direction = info.direction;

        if let Err(e) = self.traverse().await {
            if e.downcast_ref::<WebtoonDetected>().is_some() {
                return Err(e);
            }
            let n = self.files.count().await;
            self.sink.error(format!("Gián đoạn giữa chừng ({e}) - giữ lại {n} trang đã lưu"));
        }
        if !self.opts.strict && !self.stopped() {
            self.flush_orphans().await;
        }
        Ok(self.files.count().await)
    }

    // ------------------------------------------------------------------ traversal
    async fn traverse(&mut self) -> Result<()> {
        self.status(Status::Running, Some(0));
        self.wait_render().await;
        let mut media = self.read_visible().await?;
        let mut sig = signature(&media);
        let mut seen: HashSet<Signature> = HashSet::from([sig.clone()]);
        self.harvest(&media).await;

        let mut confirmed = false; // đã xác nhận được hướng lật chưa
        let mut fails = 0u32;
        let mut turns = 0u32;
        while !self.stopped() && turns < self.opts.max_pages && fails < self.opts.stagnation_limit {
            // Khóa URL: ngăn trang tự nhảy sang chapter kế tiếp.
            let cur = url_base(&self.page.url().await?.unwrap_or_default());
            if cur != url_base(&self.initial_url) {
                self.sink.info("Đã đến cuối chapter (trang web chuyển hướng sang chapter mới). Dừng tải để giữ đúng chapter hiện tại.");
                break;
            }

            match self.turn(&media, &sig, &seen, confirmed).await {
                None => {
                    fails += 1;
                    self.sink.debug(format!("Lật trang không đổi ({fails}/{})", self.opts.stagnation_limit));
                    media = self.read_visible().await?; // có thể trang vừa tải xong
                    continue;
                }
                Some((m, s, flipped)) => {
                    media = m;
                    sig = s;
                    if flipped {
                        self.direction = self.direction.opposite();
                        self.sink.info(format!("Đảo hướng lật trang -> {}", self.direction.label()));
                    }
                }
            }
            confirmed = true;
            fails = 0;
            turns += 1;
            seen.insert(sig.clone());
            self.status(Status::Running, Some(turns));
            self.harvest(&media).await;
        }

        if self.stopped() {
            self.sink.info(format!("Đã nhận lệnh dừng sau {turns} lần lật"));
        } else {
            self.sink.info(format!("Dừng lật trang sau {turns} lần lật (thất bại liên tiếp: {fails})"));
        }
        Ok(())
    }

    async fn turn(
        &mut self,
        media: &[Media],
        sig: &Signature,
        seen: &HashSet<Signature>,
        confirmed: bool,
    ) -> Option<(Vec<Media>, Signature, bool)> {
        let dir = self.direction;
        let opp = dir.opposite();
        let mut plan: Vec<(Action, ReadingDirection, bool)> = Vec::new();

        match self.opts.mode {
            // Webtoon: chỉ cuộn, tuyệt đối KHÔNG bấm phím mũi tên để tránh nhảy chapter
            Mode::Webtoon => plan.push((Action::Scroll, dir, false)),
            // Manga: chỉ dùng phím và click mép
            Mode::Manga => {
                plan.push((Action::Key, dir, false));
                plan.push((Action::Click, dir, false));
                if !confirmed {
                    plan.push((Action::Key, opp, true));
                    plan.push((Action::Click, opp, true));
                }
            }
            // Auto: ưu tiên action đã thành công, fallback đầy đủ
            Mode::Auto => {
                if let Some(a) = self.turn_action {
                    plan.push((a, dir, false));
                }
                let mut base = vec![(Action::Key, dir, false), (Action::Click, dir, false)];
                if !confirmed {
                    base.extend([
                        (Action::Key, opp, true),
                        (Action::Click, opp, true),
                        (Action::Scroll, dir, false),
                    ]);
                } else if self.turn_action != Some(Action::Scroll) {
                    base.push((Action::Scroll, dir, false));
                }
                for item in base {
                    if !plan.contains(&item) {
                        plan.push(item);
                    }
                }
            }
        }

        for (action, direction, flipped) in plan {
            if self.stopped() {
                return None;
            }
            let new_media = match self.try_action(action, direction, media).await {
                Ok(Some(m)) => m,
                Ok(None) => continue, // đã ở cuối trang khi cuộn
                Err(e) => {
                    self.sink.debug(format!("Lật bằng {action:?} lỗi: {e}"));
                    continue;
                }
            };
            let new_sig = signature(&new_media);
            if !new_sig.is_empty() && new_sig != *sig && !seen.contains(&new_sig) {
                if self.turn_action.is_none() && action == Action::Scroll {
                    self.sink.info("Phát hiện reader cuộn dọc -> chuyển sang chế độ cuộn trang");
                }
                self.turn_action = Some(action);
                return Some((new_media, new_sig, flipped));
            }
        }
        None
    }

    async fn try_action(
        &self,
        action: Action,
        direction: ReadingDirection,
        media: &[Media],
    ) -> Result<Option<Vec<Media>>> {
        match action {
            Action::Key => {
                let (key, vk) = if direction == ReadingDirection::Rtl {
                    ("ArrowLeft", 37)
                } else {
                    ("ArrowRight", 39)
                };
                browser::press_key(&self.page, key, vk).await?;
            }
            Action::Click => self.click_edge(media, direction == ReadingDirection::Rtl).await?,
            Action::Scroll => {
                let at_bottom = eval_fn(&self.page, SCROLL_JS).await?.as_bool().unwrap_or(false);
                if at_bottom {
                    return Ok(None);
                }
            }
        }
        self.wait_render().await;
        Ok(Some(self.read_visible().await?))
    }

    async fn click_edge(&self, media: &[Media], left: bool) -> Result<()> {
        let (x0, y0, w, h) = if media.is_empty() {
            let vp = eval_fn(&self.page, "() => [window.innerWidth, window.innerHeight]").await?;
            let w = vp.get(0).and_then(|v| v.as_f64()).unwrap_or(1920.0);
            let h = vp.get(1).and_then(|v| v.as_f64()).unwrap_or(1080.0);
            (0.0, 0.0, w, h)
        } else {
            let x0 = media.iter().map(|m| m.left).fold(f64::MAX, f64::min);
            let y0 = media.iter().map(|m| m.top).fold(f64::MAX, f64::min);
            let x1 = media.iter().map(|m| m.left + m.width).fold(f64::MIN, f64::max);
            let y1 = media.iter().map(|m| m.top + m.height).fold(f64::MIN, f64::max);
            (x0, y0, x1 - x0, y1 - y0)
        };
        let x = x0 + w * if left { 0.10 } else { 0.90 }; // nằm trong vùng 20% mép
        let y = y0 + h * 0.5;
        browser::click(&self.page, x, y).await
    }

    async fn wait_render(&self) {
        self.session.wait_network_idle(Duration::from_secs(4)).await;
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            let ok = eval_fn(&self.page, WAIT_IMAGES_JS)
                .await
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if ok {
                break;
            }
            if Instant::now() >= deadline {
                self.sink.debug("Ảnh chưa render xong trong 6s");
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(self.opts.delay).await;
    }

    // ----------------------------------------------------------------- extraction
    async fn read_visible(&self) -> Result<Vec<Media>> {
        Ok(serde_json::from_value(eval_fn(&self.page, VISIBLE_MEDIA_JS).await?)?)
    }

    async fn harvest(&mut self, media: &[Media]) {
        let mut ordered: Vec<&Media> = media.iter().collect();
        if self.turn_action == Some(Action::Scroll) {
            ordered.sort_by(|a, b| a.top.total_cmp(&b.top));
        } else if self.direction == ReadingDirection::Rtl {
            // Trang đôi: RTL đọc ảnh bên phải trước, LTR đọc ảnh bên trái trước.
            ordered.sort_by(|a, b| b.left.total_cmp(&a.left));
        } else {
            ordered.sort_by(|a, b| a.left.total_cmp(&b.left));
        }

        let referer = self.page.url().await.ok().flatten().unwrap_or_default();
        // Lấy dữ liệu của các ảnh trong cùng một lượt (trang đôi) song song, giữ nguyên thứ tự.
        let results = join_all(ordered.iter().map(|m| self.obtain_bytes(m, &referer))).await;

        for (m, (data, consumed)) in ordered.iter().zip(results) {
            if let Some(c) = consumed {
                self.consumed.insert(c);
            }
            let label = m.src.clone().unwrap_or_else(|| m.kind.clone());
            match data {
                Some((bytes, ext)) => self.save(bytes, &label, ext).await,
                None => {
                    let short: String = label.chars().take(90).collect();
                    self.sink.warn(format!("Không lấy được dữ liệu ảnh: {short}"));
                }
            }
        }
    }

    /// Trả về (dữ liệu + đuôi file ép buộc, url đã tiêu thụ).
    async fn obtain_bytes(
        &self,
        m: &Media,
        referer: &str,
    ) -> (Option<(Arc<Vec<u8>>, Option<&'static str>)>, Option<String>) {
        // Tầng 1: buffer bắt từ network (không dính 403 hotlink / blob hết hạn)
        if let Some(src) = m.src.as_deref().filter(|s| s.starts_with("http")) {
            if let Some(cap) = self.session.wait_captured(src, Duration::from_millis(1500)).await {
                return (Some((Arc::clone(&cap.data), None)), Some(cap.url.clone()));
            }
        }
        if m.kind == "img" {
            // Tầng 2: blob/data URL đã đọc trong trang, hoặc tải bằng reqwest + cookies
            if let Some(b64) = &m.b64 {
                return (decode(b64).map(|d| (Arc::new(d), None)), None);
            }
            if let Some(src) = m.src.as_deref().filter(|s| s.starts_with("http")) {
                if let Some(d) = self.session.download(&self.page, src, referer).await {
                    return (Some((Arc::new(d), None)), Some(src.to_string()));
                }
            }
        } else if let Some(b64) = &m.b64 {
            // Tầng 3: canvas.toDataURL
            return (decode(b64).map(|d| (Arc::new(d), Some(".png"))), None);
        }
        (None, None)
    }

    async fn save(&self, data: Arc<Vec<u8>>, source: &str, ext: Option<&'static str>) {
        if data.is_empty() {
            return;
        }
        if let Some(saved) = self.files.save(data, source, ext).await {
            (self.on_saved)(saved);
        }
    }

    /// Lưu nốt ảnh preload đã bắt được nhưng chưa gắn với trang nào.
    async fn flush_orphans(&self) {
        if self.turn_action == Some(Action::Scroll) {
            return;
        }
        for cap in self.session.orphans(&self.consumed) {
            self.save(Arc::clone(&cap.data), &cap.url, None).await;
        }
    }
}

fn decode(b64: &str) -> Option<Vec<u8>> {
    B64.decode(b64).ok()
}
