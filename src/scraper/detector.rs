//! Nhận diện reader manga và hướng lật trang.
use std::time::{Duration, Instant};

use chromiumoxide::Page;
use serde::Deserialize;

use crate::events::Sink;
use crate::scraper::browser::eval_fn;

pub const WEBTOON_MESSAGE: &str =
    "URL này sử dụng giao diện đọc Webtoon, không thuộc phạm vi hỗ trợ của tool.";

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct WebtoonDetected(pub String);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadingDirection {
    Rtl,
    Ltr,
}

impl ReadingDirection {
    pub fn opposite(self) -> Self {
        match self {
            Self::Rtl => Self::Ltr,
            Self::Ltr => Self::Rtl,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Rtl => "RTL",
            Self::Ltr => "LTR",
        }
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
#[allow(dead_code)]
pub struct Probe {
    pub stacked_rows: u32,
    pub avg_ratio: f64,
    pub img_count: u32,
    pub canvas_count: u32,
    pub has_counter: bool,
    pub has_page_select: bool,
    pub rtl_hint: bool,
    pub ltr_hint: bool,
}

#[allow(dead_code)]
pub struct ReaderInfo {
    pub kind: &'static str, // "image" | "canvas" | "unknown"
    pub direction: ReadingDirection,
    pub probe: Probe,
}

// Lưu ý: regex dùng \b thật (bản Python escape nhầm thành \\b nên không bao giờ khớp).
const LAYOUT_PROBE_JS: &str = r##"
() => {
  const imgs = Array.from(document.images)
    .filter(i => i.naturalWidth >= 300 && i.naturalHeight >= 300);
  const shown = imgs.map(i => {
    const r = i.getBoundingClientRect();
    return {top: r.top + window.scrollY, w: r.width, h: r.height};
  }).filter(o => o.w >= 200 && o.h >= 200);
  shown.sort((a, b) => a.top - b.top);
  let rows = 0, last = -Infinity, lastH = 0;
  for (const o of shown) {
    if (o.top - last >= Math.max(50, lastH * 0.5)) { rows++; last = o.top; lastH = o.h; }
  }
  const ratios = imgs.map(i => i.naturalHeight / i.naturalWidth);
  const avg = ratios.length ? ratios.reduce((a, b) => a + b, 0) / ratios.length : 0;
  const text = document.body ? document.body.innerText.slice(0, 6000) : '';
  const hasCounter = /\b\d+\s*(\/|of|trên)\s*\d+\b/i.test(text);
  const hasPageSelect = Array.from(document.querySelectorAll('select'))
    .some(s => s.options.length >= 5);
  const canvasCount = Array.from(document.querySelectorAll('canvas')).filter(c => {
    const r = c.getBoundingClientRect(); return r.width >= 300 && r.height >= 300;
  }).length;
  const cls = Array.from(document.querySelectorAll('[class],[dir],[data-direction],[data-dir]'))
    .slice(0, 3000)
    .map(e => {
      const c = (e.className && e.className.baseVal !== undefined) ? e.className.baseVal : e.className;
      return c + ' ' + (e.getAttribute('dir') || '') + ' ' + (e.getAttribute('data-direction') || '');
    }).join(' ').toLowerCase();
  return {
    stackedRows: rows, avgRatio: avg, imgCount: imgs.length, canvasCount,
    hasCounter, hasPageSelect,
    rtlHint: /rtl|right-to-left|right_to_left/.test(cls),
    ltrHint: /\bltr\b|left-to-right|left_to_right/.test(cls),
  };
}
"##;

async fn probe_layout(page: &Page) -> anyhow::Result<Probe> {
    Ok(serde_json::from_value(eval_fn(page, LAYOUT_PROBE_JS).await?)?)
}

/// Đã mở khóa hỗ trợ Webtoon/Manhwa/Manhua cuộn dọc.
fn is_webtoon(_probe: &Probe) -> bool {
    false
}

async fn wait_for_reader(page: &Page, sink: &Sink, timeout: Duration) -> Option<Probe> {
    let deadline = Instant::now() + timeout;
    let mut last: Option<Probe> = None;
    while Instant::now() < deadline {
        match probe_layout(page).await {
            Ok(p) => {
                let ready = p.img_count > 0 || p.canvas_count > 0;
                last = Some(p);
                if ready {
                    return last;
                }
            }
            Err(e) => {
                sink.debug(format!("probe lỗi: {e}"));
                last = None;
            }
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    last
}

pub async fn detect_reader(page: &Page, sink: &Sink) -> Result<ReaderInfo, WebtoonDetected> {
    let Some(probe) = wait_for_reader(page, sink, Duration::from_secs(20)).await else {
        return Ok(ReaderInfo { kind: "unknown", direction: ReadingDirection::Rtl, probe: Probe::default() });
    };
    if is_webtoon(&probe) {
        return Err(WebtoonDetected(WEBTOON_MESSAGE.to_string()));
    }
    let kind = if probe.canvas_count > 0 && probe.img_count == 0 {
        "canvas"
    } else if probe.img_count > 0 {
        "image"
    } else {
        "unknown"
    };
    // Mặc định RTL (manga); extractor tự đảo nếu phím/click ngược hướng mới có tác dụng.
    let direction = if probe.ltr_hint && !probe.rtl_hint {
        ReadingDirection::Ltr
    } else {
        ReadingDirection::Rtl
    };
    sink.info(format!("Reader: {kind} | hướng lật: {}", direction.label()));
    Ok(ReaderInfo { kind, direction, probe })
}
