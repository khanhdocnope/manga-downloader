//! slugify + suy ra tên thư mục từ URL (port của slugify/derive_names).
use percent_encoding::percent_decode_str;
use unicode_normalization::UnicodeNormalization;
use url::Url;

pub fn slugify(text: &str, fallback: &str) -> String {
    let replaced = text.replace('đ', "d").replace('Đ', "D");
    let ascii: String = replaced.nfkd().filter(|c| c.is_ascii()).collect();
    let mut out = String::new();
    let mut pending_dash = false;
    for c in ascii.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    out.truncate(80);
    if out.is_empty() {
        fallback.to_string()
    } else {
        out
    }
}

/// Trả về (manga_slug, chapter_slug) từ URL, dự phòng bằng <title>.
pub fn derive_names(url: &str, title: &str) -> (String, String) {
    let parsed = Url::parse(url).ok();
    let segments: Vec<String> = parsed
        .as_ref()
        .and_then(|u| u.path_segments())
        .map(|it| {
            it.filter(|s| !s.is_empty())
                .map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();

    let chapter = segments
        .last()
        .map(|s| slugify(s, "untitled"))
        .unwrap_or_else(|| "chapter".to_string());
    let mut manga = if segments.len() >= 2 {
        slugify(&segments[segments.len() - 2], "untitled")
    } else {
        String::new()
    };
    if manga.is_empty() || manga.chars().all(|c| c.is_ascii_digit()) {
        let head = title
            .split('|')
            .next()
            .unwrap_or("")
            .split(" - ")
            .next()
            .unwrap_or("")
            .trim();
        let host = parsed
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        manga = slugify(head, &slugify(&host, "manga"));
    }
    (manga, chapter)
}
