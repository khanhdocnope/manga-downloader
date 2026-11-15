"""Nhận diện reader manga, loại trừ Webtoon, xác định hướng lật trang."""
from __future__ import annotations

import asyncio
import logging
import time
from dataclasses import dataclass
from enum import Enum
from typing import Any

from playwright.async_api import Error as PlaywrightError
from playwright.async_api import Page

log = logging.getLogger("manga_dl.detector")

WEBTOON_MESSAGE = "URL này sử dụng giao diện đọc Webtoon, không thuộc phạm vi hỗ trợ của tool."


class WebtoonDetected(Exception):
    """Reader dạng cuộn dọc - không thuộc phạm vi hỗ trợ."""

    def __init__(self, message: str = WEBTOON_MESSAGE) -> None:
        super().__init__(message)


class ReadingDirection(str, Enum):
    RTL = "rtl"  # manga Nhật: trang kế tiếp nằm bên trái
    LTR = "ltr"


@dataclass(frozen=True)
class ReaderInfo:
    kind: str  # "image" | "canvas" | "unknown"
    direction: ReadingDirection
    probe: dict[str, Any]


LAYOUT_PROBE_JS = """
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
  const hasCounter = /\\b\\d+\\s*(\\/|of|trên)\\s*\\d+\\b/i.test(text);
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
    ltrHint: /\\bltr\\b|left-to-right|left_to_right/.test(cls),
  };
}
"""


async def probe_layout(page: Page) -> dict[str, Any]:
    result: dict[str, Any] = await page.evaluate(LAYOUT_PROBE_JS)
    return result


def is_webtoon(probe: dict[str, Any]) -> bool:
    """Đã mở khóa hỗ trợ Webtoon/Manhwa/Manhua cuộn dọc."""
    return False


async def wait_for_reader(page: Page, timeout: float = 20.0) -> dict[str, Any]:
    """Chờ tới khi trang có ảnh/canvas nội dung, trả về kết quả probe cuối cùng."""
    deadline = time.monotonic() + timeout
    probe: dict[str, Any] = {}
    while time.monotonic() < deadline:
        try:
            probe = await probe_layout(page)
        except PlaywrightError as exc:  # trang đang điều hướng
            log.debug("probe lỗi: %s", exc)
            probe = {}
        if probe.get("imgCount", 0) > 0 or probe.get("canvasCount", 0) > 0:
            return probe
        await asyncio.sleep(0.8)
    return probe


async def detect_reader(page: Page) -> ReaderInfo:
    probe = await wait_for_reader(page)
    if not probe:
        return ReaderInfo("unknown", ReadingDirection.RTL, {})

    if is_webtoon(probe):
        raise WebtoonDetected()

    if probe.get("canvasCount", 0) > 0 and probe.get("imgCount", 0) == 0:
        kind = "canvas"
    elif probe.get("imgCount", 0) > 0:
        kind = "image"
    else:
        kind = "unknown"

    # Mặc định RTL (manga); extractor sẽ tự đảo nếu phím/click ngược hướng mới có tác dụng.
    if probe.get("ltrHint") and not probe.get("rtlHint"):
        direction = ReadingDirection.LTR
    else:
        direction = ReadingDirection.RTL
    log.info("Reader: %s | hướng lật: %s", kind, direction.value.upper())
    return ReaderInfo(kind, direction, probe)
