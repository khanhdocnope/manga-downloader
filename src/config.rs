//! Cấu hình một lần tải + đọc cookies (port của _parse_config / load_cookies).
use std::path::PathBuf;

use chromiumoxide::cdp::browser_protocol::network::{CookieParam, CookieSameSite, TimeSinceEpoch};
use serde_json::Value;
use url::Url;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Auto,
    Manga,
    Webtoon,
}

impl Mode {
    #[allow(dead_code)]
    pub const ALL: [Mode; 3] = [Mode::Auto, Mode::Manga, Mode::Webtoon];
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Mode::Auto => "Tự động",
            Mode::Manga => "Manga (lật trang)",
            Mode::Webtoon => "Webtoon (cuộn dọc)",
        }
    }
}

#[derive(Clone)]
pub struct Config {
    pub url: String,
    pub mode: Mode,
    pub headless: bool,
    pub delay_ms: u64,
    pub max_pages: u32,
    pub cookies: Vec<CookieParam>,
    pub output_dir: PathBuf,
}

impl Config {
    pub fn from_form(
        url: &str,
        mode: Mode,
        headless: bool,
        delay_ms: u64,
        max_pages: u32,
        cookies_text: &str,
        output_dir: &str,
    ) -> Result<Config, String> {
        let url = url.trim().to_string();
        match Url::parse(&url) {
            Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => {}
            _ => return Err("URL không hợp lệ: cần bắt đầu bằng http:// hoặc https://".into()),
        }
        let cookies = if cookies_text.trim().is_empty() {
            Vec::new()
        } else {
            parse_cookies(cookies_text)?
        };
        let out = output_dir.trim();
        let out = PathBuf::from(if out.is_empty() { "downloads" } else { out });
        let output_dir = std::path::absolute(&out).unwrap_or(out);
        Ok(Config {
            url,
            mode,
            headless,
            delay_ms: delay_ms.min(30_000),
            max_pages: max_pages.clamp(1, 2000),
            cookies,
            output_dir,
        })
    }
}

/// Đọc cookies JSON (định dạng Playwright hoặc export từ extension trình duyệt).
pub fn parse_cookies(text: &str) -> Result<Vec<CookieParam>, String> {
    let raw: Value =
        serde_json::from_str(text).map_err(|_| "Cookies không phải JSON hợp lệ".to_string())?;
    let list = match raw {
        Value::Array(a) => a,
        Value::Object(mut m) => match m.remove("cookies") {
            Some(Value::Array(a)) => a,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };

    let mut out = Vec::new();
    for c in list {
        let Some(name) = c.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
            continue;
        };
        let value = match c.get("value") {
            Some(Value::String(s)) => s.clone(),
            None | Some(Value::Null) => continue,
            Some(v) => v.to_string(),
        };
        let mut cp = CookieParam::new(name, value);

        let non_empty = |key: &str| {
            c.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
        };
        if let Some(u) = non_empty("url") {
            cp.url = Some(u);
        } else if let Some(d) = non_empty("domain") {
            cp.domain = Some(d);
            cp.path = Some(non_empty("path").unwrap_or_else(|| "/".into()));
        } else {
            continue;
        }

        let exp = c.get("expires").or_else(|| c.get("expirationDate")).and_then(Value::as_f64);
        if let Some(e) = exp.filter(|e| *e > 0.0) {
            cp.expires = Some(TimeSinceEpoch::new(e));
        }
        if let Some(b) = c.get("httpOnly").and_then(Value::as_bool) {
            cp.http_only = Some(b);
        }
        if let Some(b) = c.get("secure").and_then(Value::as_bool) {
            cp.secure = Some(b);
        }
        match c.get("sameSite").and_then(Value::as_str).map(str::to_lowercase).as_deref() {
            Some("strict") => cp.same_site = Some(CookieSameSite::Strict),
            Some("lax") => cp.same_site = Some(CookieSameSite::Lax),
            Some("no_restriction") | Some("none") => cp.same_site = Some(CookieSameSite::None),
            _ => {}
        }
        out.push(cp);
    }
    Ok(out)
}
