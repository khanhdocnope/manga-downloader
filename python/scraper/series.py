"""Trích xuất danh sách chapter từ trang thông tin bộ truyện (Series Info)."""
from __future__ import annotations

import logging
import re
from typing import Any, Optional
from urllib.parse import urljoin, urlparse

import httpx
from bs4 import BeautifulSoup

from scraper.browser import DEFAULT_UA

log = logging.getLogger("manga_dl.series")

CHAPTER_PATTERNS = [
    re.compile(r"/(?:chap|chapter|chuong|tap)[-_/]?(\d+(?:[.-]\d+)?)", re.IGNORECASE),
    re.compile(r"[-_/](\d+(?:[.-]\d+)?)$", re.IGNORECASE),
]


def extract_chapter_number(text: str, href: str) -> float:
    """Rút trích số thứ tự chapter từ text hoặc URL để sắp xếp chính xác."""
    # Thử từ text trước: "Chương 18.5", "Chapter 1", "Chap 02"
    m = re.search(r"(?:chap|chapter|chương|tập)\s*(\d+(?:[.-]\d+)?)", text, re.IGNORECASE)
    if m:
        try:
            return float(m.group(1).replace("-", "."))
        except ValueError:
            pass

    # Thử từ URL
    for pat in CHAPTER_PATTERNS:
        m = pat.search(href)
        if m:
            try:
                return float(m.group(1).replace("-", "."))
            except ValueError:
                pass
    return 999999.0


def parse_series_html(html: str, base_url: str) -> dict[str, Any]:
    soup = BeautifulSoup(html, "html.parser")

    # 1. Tên truyện
    title = ""
    for selector in ["h1.title-manga", "h1.book_title", "h1.manga-name", "h1.post-title", "h1", ".title"]:
        el = soup.select_one(selector)
        if el and el.get_text(strip=True):
            title = el.get_text(strip=True)
            break
    if not title:
        og_title = soup.find("meta", property="og:title")
        if og_title and og_title.get("content"):
            title = str(og_title["content"]).split("|")[0].split("-")[0].strip()

    # 2. Ảnh bìa
    cover = ""
    og_image = soup.find("meta", property="og:image")
    if og_image and og_image.get("content"):
        cover = urljoin(base_url, str(og_image["content"]))
    else:
        for selector in [".book_avatar img", ".manga-cover img", ".summary_image img", "img.cover"]:
            el = soup.select_one(selector)
            if el:
                src = el.get("src") or el.get("data-src") or el.get("data-original")
                if src:
                    cover = urljoin(base_url, str(src))
                    break

    # 3. Danh sách chapters
    # Tìm kiếm các container danh sách chapter phổ biến
    container = None
    for c_sel in [
        ".works-chapter-list", ".list-chapter", ".chapter-list", "#list-chapter",
        ".listing-chapters_wrap", ".chapters-list", ".row-content-chapters",
        ".chapter_list", ".list-item", ".episodes-list"
    ]:
        found = soup.select_one(c_sel)
        if found:
            container = found
            break

    search_scope = container if container else soup

    seen_urls: set[str] = set()
    raw_chapters: list[dict[str, Any]] = []

    for a in search_scope.find_all("a"):
        href = a.get("href", "").strip()
        if not href or href.startswith("javascript:") or href == "#":
            continue
        full_url = urljoin(base_url, href)
        if full_url in seen_urls:
            continue

        text = a.get_text(strip=True)
        # Kiểm tra xem link có phải là chapter không
        is_chapter = False
        text_lower = text.lower()
        href_lower = href.lower()

        if any(k in text_lower for k in ["chap", "chapter", "chương", "tập", "ep."]):
            is_chapter = True
        elif any(k in href_lower for k in ["/chap", "/chapter", "-chap-", "-chapter-", "/c-"]):
            is_chapter = True

        if is_chapter:
            seen_urls.add(full_url)
            num = extract_chapter_number(text, href)
            raw_chapters.append({
                "name": text if text else f"Chapter {num if num != 999999.0 else len(raw_chapters)+1}",
                "url": full_url,
                "num": num,
            })

    # Sắp xếp theo số thứ tự chapter (1 -> N)
    raw_chapters.sort(key=lambda c: c["num"])

    # Đánh lại index chuẩn
    chapters: list[dict[str, Any]] = []
    for idx, c in enumerate(raw_chapters, start=1):
        chapters.append({
            "index": idx,
            "name": c["name"],
            "url": c["url"],
            "num": c["num"] if c["num"] != 999999.0 else idx,
        })

    return {
        "title": title if title else urlparse(base_url).path.strip("/").split("/")[-1],
        "cover": cover,
        "url": base_url,
        "total_chapters": len(chapters),
        "chapters": chapters,
    }


async def fetch_series_info(url: str, cookies: Optional[list[dict[str, Any]]] = None) -> dict[str, Any]:
    """Lấy danh sách chapter từ URL truyện, tự động fallback sang Playwright nếu web chặn bot."""
    headers = {
        "User-Agent": DEFAULT_UA,
        "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,image/*,*/*;q=0.8",
        "Accept-Language": "vi-VN,vi;q=0.9,en-US;q=0.8,en;q=0.7",
    }
    
    # 1. Thử tải trực tiếp với httpx siêu nhanh
    try:
        async with httpx.AsyncClient(headers=headers, follow_redirects=True, timeout=12.0) as client:
            resp = await client.get(url)
            if resp.status_code == 200:
                res = parse_series_html(resp.text, str(resp.url))
                if res["total_chapters"] > 0:
                    log.info("Lấy thành công %d chapters qua HTTP trực tiếp", res["total_chapters"])
                    return res
    except Exception as exc:
        log.info("HTTP trực tiếp không thành công (%s), chuyển sang browser...", exc)

    # 2. Fallback sang Playwright Headless
    from scraper.browser import BrowserSession
    async with BrowserSession(headless=True) as session:
        page = await session.context.new_page()
        try:
            await page.goto(url, wait_until="domcontentloaded", timeout=30000)
            # Chờ một chút để script render danh sách chapter
            try:
                await page.wait_for_load_state("networkidle", timeout=6000)
            except Exception:
                pass
            content = await page.content()
            res = parse_series_html(content, page.url)
            return res
        finally:
            await page.close()
