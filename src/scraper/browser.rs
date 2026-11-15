//! Vòng đời Chrome (CDP), nạp cookies, bắt response ảnh và tải ảnh dự phòng bằng reqwest.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams,
    DispatchMouseEventType, MouseButton,
};
use chromiumoxide::cdp::browser_protocol::network::{
    EventLoadingFailed, EventLoadingFinished, EventRequestWillBeSent, EventResponseReceived,
    GetCookiesParams, GetResponseBodyParams, SetUserAgentOverrideParams,
};
use chromiumoxide::cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams;
use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use chromiumoxide::handler::viewport::Viewport;
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures::StreamExt;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::events::Sink;

pub const DEFAULT_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
const MIN_CAPTURE_BYTES: f64 = 50.0 * 1024.0;
const IMAGE_TYPES: [&str; 5] = ["image/jpeg", "image/jpg", "image/png", "image/webp", "image/avif"];
const BINARY_TYPES: [&str; 2] = ["application/octet-stream", "binary/octet-stream"];

const STEALTH_JS: &str = r##"
Object.defineProperty(navigator, 'webdriver', {get: () => undefined});
Object.defineProperty(navigator, 'languages', {get: () => ['en-US', 'en', 'vi']});
Object.defineProperty(navigator, 'plugins', {get: () => [1, 2, 3, 4, 5]});
window.chrome = window.chrome || {runtime: {}};
"##;

pub struct Captured {
    pub url: String,
    pub data: Arc<Vec<u8>>,
    pub seq: u64,
}

pub struct BrowserSession {
    browser: Mutex<Browser>,
    handler_task: JoinHandle<()>,
    tasks: StdMutex<Vec<JoinHandle<()>>>,
    store: Arc<StdMutex<HashMap<String, Arc<Captured>>>>,
    notify: Arc<Notify>,
    seq: Arc<AtomicU64>,
    inflight: Arc<AtomicI64>,
    http: reqwest::Client,
    cfg: Config,
}

impl BrowserSession {
    pub async fn launch(cfg: Config) -> Result<Self> {
        let mut builder = BrowserConfig::builder()
            .window_size(1920, 1080)
            .viewport(None::<Viewport>)
            .arg("--disable-blink-features=AutomationControlled");
        if !cfg.headless {
            builder = builder.with_head();
        }
        let bcfg = builder.build().map_err(|e| anyhow!(e))?;
        let (browser, mut handler) = Browser::launch(bcfg).await.context(
            "Không khởi động được Chrome/Chromium. Hãy cài Google Chrome, Chromium hoặc Edge \
             (hoặc đặt biến môi trường CHROME trỏ tới file chạy).",
        )?;
        let handler_task = tokio::spawn(async move {
            while let Some(h) = handler.next().await {
                if h.is_err() {
                    break;
                }
            }
        });
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .danger_accept_invalid_certs(true)
            .build()?;
        Ok(Self {
            browser: Mutex::new(browser),
            handler_task,
            tasks: StdMutex::new(Vec::new()),
            store: Arc::new(StdMutex::new(HashMap::new())),
            notify: Arc::new(Notify::new()),
            seq: Arc::new(AtomicU64::new(0)),
            inflight: Arc::new(AtomicI64::new(0)),
            http,
            cfg,
        })
    }

    pub async fn close(&self) {
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
        {
            let mut b = self.browser.lock().await;
            let _ = b.close().await;
            let _ = b.wait().await;
        }
        self.handler_task.abort();
    }

    // ------------------------------------------------------------ network capture
    async fn start_capture(&self, page: &Page) -> Result<()> {
        let mut resp = page.event_listener::<EventResponseReceived>().await?;
        let mut fin = page.event_listener::<EventLoadingFinished>().await?;
        let mut req = page.event_listener::<EventRequestWillBeSent>().await?;
        let mut fail = page.event_listener::<EventLoadingFailed>().await?;

        let store = Arc::clone(&self.store);
        let notify = Arc::clone(&self.notify);
        let seq = Arc::clone(&self.seq);
        let inflight = Arc::clone(&self.inflight);
        let page = page.clone();

        let task = tokio::spawn(async move {
            let mut pending: HashMap<String, String> = HashMap::new(); // request_id -> url
            loop {
                tokio::select! {
                    Some(_) = req.next() => { inflight.fetch_add(1, Ordering::Relaxed); }
                    Some(_) = fail.next() => { inflight.fetch_sub(1, Ordering::Relaxed); }
                    Some(ev) = resp.next() => {
                        if ev.response.status == 200 {
                            let mime = ev.response.mime_type.to_lowercase();
                            if IMAGE_TYPES.contains(&mime.as_str()) || BINARY_TYPES.contains(&mime.as_str()) {
                                pending.insert(ev.request_id.inner().clone(), ev.response.url.clone());
                            }
                        }
                    }
                    Some(ev) = fin.next() => {
                        inflight.fetch_sub(1, Ordering::Relaxed);
                        let Some(url) = pending.remove(ev.request_id.inner()) else { continue };
                        if ev.encoded_data_length < MIN_CAPTURE_BYTES { continue; }
                        // Lấy body song song, không chặn vòng lặp sự kiện.
                        let (page, store, notify, seq) =
                            (page.clone(), Arc::clone(&store), Arc::clone(&notify), Arc::clone(&seq));
                        let rid = ev.request_id.clone();
                        tokio::spawn(async move {
                            let Ok(r) = page.execute(GetResponseBodyParams::new(rid)).await else { return };
                            let body = if r.result.base64_encoded {
                                match B64.decode(r.result.body.as_bytes()) { Ok(b) => b, Err(_) => return }
                            } else {
                                r.result.body.clone().into_bytes()
                            };
                            if (body.len() as f64) < MIN_CAPTURE_BYTES { return; }
                            let n = seq.fetch_add(1, Ordering::Relaxed) + 1;
                            store.lock().unwrap().insert(
                                url.clone(),
                                Arc::new(Captured { url, data: Arc::new(body), seq: n }),
                            );
                            notify.notify_waiters();
                        });
                    }
                    else => break,
                }
            }
        });
        self.tasks.lock().unwrap().push(task);
        Ok(())
    }

    pub fn get_captured(&self, url: &str) -> Option<Arc<Captured>> {
        let map = self.store.lock().unwrap();
        map.get(url)
            .cloned()
            .or_else(|| url.split_once('#').and_then(|(base, _)| map.get(base).cloned()))
    }

    /// Chờ tới khi ảnh `url` được bắt (đánh thức tức thì bằng Notify thay vì polling 150ms).
    pub async fn wait_captured(&self, url: &str, timeout: Duration) -> Option<Arc<Captured>> {
        let deadline = Instant::now() + timeout;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(c) = self.get_captured(url) {
                return Some(c);
            }
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
                return None;
            }
            let _ = tokio::time::timeout(remain, notified).await;
        }
    }

    /// Ảnh đã bắt nhưng chưa dùng (vd: preload), theo thứ tự tải về.
    pub fn orphans(&self, consumed: &HashSet<String>) -> Vec<Arc<Captured>> {
        let map = self.store.lock().unwrap();
        let mut v: Vec<_> =
            map.iter().filter(|(u, _)| !consumed.contains(*u)).map(|(_, c)| Arc::clone(c)).collect();
        v.sort_by_key(|c| c.seq);
        v
    }

    pub async fn wait_network_idle(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut quiet_since: Option<Instant> = None;
        while Instant::now() < deadline {
            if self.inflight.load(Ordering::Relaxed) <= 0 {
                let q = quiet_since.get_or_insert_with(Instant::now);
                if q.elapsed() >= Duration::from_millis(500) {
                    return;
                }
            } else {
                quiet_since = None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // ----------------------------------------------------------------------- page
    pub async fn open(&self, url: &str, sink: &Sink) -> Result<Page> {
        let page = self.browser.lock().await.new_page("about:blank").await?;
        page.execute(SetUserAgentOverrideParams::new(DEFAULT_UA)).await?;
        page.execute(AddScriptToEvaluateOnNewDocumentParams::new(STEALTH_JS)).await?;
        if !self.cfg.cookies.is_empty() {
            match page.set_cookies(self.cfg.cookies.clone()).await {
                Ok(_) => sink.info(format!("Đã nạp {} cookies", self.cfg.cookies.len())),
                Err(e) => sink.warn(format!("Không nạp được cookies ({e}) - tiếp tục không cookies")),
            }
        }
        self.start_capture(&page).await?;

        let mut last: Option<anyhow::Error> = None;
        for attempt in 1..=3u64 {
            match tokio::time::timeout(Duration::from_secs(60), page.goto(url)).await {
                Ok(Ok(_)) => {
                    last = None;
                    break;
                }
                Ok(Err(e)) => last = Some(anyhow!(e)),
                Err(_) => last = Some(anyhow!("quá thời gian chờ 60s")),
            }
            sink.warn(format!(
                "Mở trang lần {attempt}/3 thất bại: {}",
                last.as_ref().map(|e| e.to_string()).unwrap_or_default()
            ));
            tokio::time::sleep(Duration::from_secs(2 * attempt)).await;
        }
        if let Some(e) = last {
            return Err(e.context(format!("Không mở được {url}")));
        }
        self.wait_network_idle(Duration::from_secs(10)).await;
        Ok(page)
    }

    /// Tải ảnh bằng reqwest (client dùng chung, connection pool) với cookies + Referer của phiên.
    pub async fn download(&self, page: &Page, url: &str, referer: &str) -> Option<Vec<u8>> {
        let mut params = GetCookiesParams::default();
        params.urls = Some(vec![url.to_string()]);
        let cookie_header = page
            .execute(params)
            .await
            .map(|r| {
                r.result
                    .cookies
                    .iter()
                    .map(|c| format!("{}={}", c.name, c.value))
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .unwrap_or_default();

        for attempt in 0..3u64 {
            let mut rq = self
                .http
                .get(url)
                .header("User-Agent", DEFAULT_UA)
                .header("Referer", referer)
                .header("Accept", "image/avif,image/webp,image/*,*/*;q=0.8");
            if !cookie_header.is_empty() {
                rq = rq.header("Cookie", &cookie_header);
            }
            if let Ok(resp) = rq.send().await.and_then(|r| r.error_for_status()) {
                if let Ok(bytes) = resp.bytes().await {
                    return Some(bytes.to_vec());
                }
            }
            tokio::time::sleep(Duration::from_millis(1500 * (attempt + 1))).await;
        }
        None
    }
}

// ------------------------------------------------------------------ CDP helpers
/// Chạy một biểu thức JS (có thể trả Promise) và trả về giá trị JSON.
pub async fn eval(page: &Page, expr: &str) -> Result<serde_json::Value> {
    let params = EvaluateParams::builder()
        .expression(expr)
        .await_promise(true)
        .return_by_value(true)
        .build()
        .map_err(|e| anyhow!(e))?;
    let res = page.execute(params).await?;
    if let Some(ex) = &res.result.exception_details {
        bail!("Lỗi JS: {}", ex.text);
    }
    Ok(res.result.result.value.clone().unwrap_or(serde_json::Value::Null))
}

/// Chạy một hàm JS (arrow/async function) không tham số.
pub async fn eval_fn(page: &Page, func: &str) -> Result<serde_json::Value> {
    eval(page, &format!("({func})()")).await
}

pub async fn press_key(page: &Page, key: &str, vk: i64) -> Result<()> {
    for ty in [DispatchKeyEventType::RawKeyDown, DispatchKeyEventType::KeyUp] {
        let ev = DispatchKeyEventParams::builder()
            .r#type(ty)
            .key(key)
            .code(key)
            .windows_virtual_key_code(vk)
            .native_virtual_key_code(vk)
            .build()
            .map_err(|e| anyhow!(e))?;
        page.execute(ev).await?;
    }
    Ok(())
}

pub async fn click(page: &Page, x: f64, y: f64) -> Result<()> {
    let steps = [
        (DispatchMouseEventType::MouseMoved, false),
        (DispatchMouseEventType::MousePressed, true),
        (DispatchMouseEventType::MouseReleased, true),
    ];
    for (ty, with_button) in steps {
        let mut b = DispatchMouseEventParams::builder().r#type(ty).x(x).y(y);
        if with_button {
            b = b.button(MouseButton::Left).click_count(1);
        }
        page.execute(b.build().map_err(|e| anyhow!(e))?).await?;
    }
    Ok(())
}
