"""Vòng đời Playwright, nạp cookies và chặn (intercept) response ảnh."""
from __future__ import annotations

import asyncio
import json
import logging
from dataclasses import dataclass
from pathlib import Path
from types import TracebackType
from typing import Any, Optional

import httpx
from playwright.async_api import (
    Browser,
    BrowserContext,
    Page,
    Playwright,
    Response,
    async_playwright,
)
from playwright.async_api import Error as PlaywrightError
from playwright.async_api import TimeoutError as PlaywrightTimeoutError

log = logging.getLogger("manga_dl.browser")

DEFAULT_UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36"
)
IMAGE_TYPES = {"image/jpeg", "image/jpg", "image/png", "image/webp", "image/avif"}
BINARY_TYPES = {"application/octet-stream", "binary/octet-stream"}
MIN_CAPTURE_BYTES = 50 * 1024

STEALTH_JS = """
Object.defineProperty(navigator, 'webdriver', {get: () => undefined});
Object.defineProperty(navigator, 'languages', {get: () => ['en-US', 'en', 'vi']});
Object.defineProperty(navigator, 'plugins', {get: () => [1, 2, 3, 4, 5]});
window.chrome = window.chrome || {runtime: {}};
"""

_SAMESITE = {"strict": "Strict", "lax": "Lax", "no_restriction": "None", "none": "None"}


@dataclass(frozen=True)
class CapturedImage:
    url: str
    data: bytes
    content_type: str
    seq: int


def load_cookies(path: Path) -> list[dict[str, Any]]:
    """Đọc cookies JSON (định dạng Playwright hoặc export từ extension trình duyệt)."""
    raw = json.loads(path.read_text(encoding="utf-8"))
    if isinstance(raw, dict):
        raw = raw.get("cookies", [])
    cookies: list[dict[str, Any]] = []
    for c in raw:
        name, value = c.get("name"), c.get("value")
        if not name or value is None:
            continue
        item: dict[str, Any] = {"name": name, "value": str(value)}
        if c.get("url"):
            item["url"] = c["url"]
        elif c.get("domain"):
            item["domain"] = c["domain"]
            item["path"] = c.get("path", "/")
        else:
            continue
        exp = c.get("expires", c.get("expirationDate"))
        if isinstance(exp, (int, float)) and exp > 0:
            item["expires"] = float(exp)
        if "httpOnly" in c:
            item["httpOnly"] = bool(c["httpOnly"])
        if "secure" in c:
            item["secure"] = bool(c["secure"])
        same_site = _SAMESITE.get(str(c.get("sameSite", "")).lower())
        if same_site:
            item["sameSite"] = same_site
        cookies.append(item)
    return cookies


class BrowserSession:
    """Async context manager quản lý Chromium + bắt response ảnh ngay khi tải về."""

    def __init__(
        self,
        headless: bool = True,
        cookies_path: Optional[Path] = None,
        user_agent: str = DEFAULT_UA,
        min_capture_bytes: int = MIN_CAPTURE_BYTES,
    ) -> None:
        self.headless = headless
        self.cookies_path = cookies_path
        self.user_agent = user_agent
        self.min_capture_bytes = min_capture_bytes
        self._pw: Optional[Playwright] = None
        self._browser: Optional[Browser] = None
        self._context: Optional[BrowserContext] = None
        self._captured: dict[str, CapturedImage] = {}
        self._seq = 0

    @property
    def context(self) -> Optional[BrowserContext]:
        return self._context

    async def __aenter__(self) -> "BrowserSession":
        self._pw = await async_playwright().start()
        launch_args = ["--disable-blink-features=AutomationControlled"]
        try:
            self._browser = await self._pw.chromium.launch(
                headless=self.headless,
                args=launch_args,
            )
        except Exception as exc:
            log.info("Không mở được Chromium mặc định (%s), thử kênh trình duyệt hệ thống...", exc)
            for ch in ("chrome", "msedge"):
                try:
                    self._browser = await self._pw.chromium.launch(
                        channel=ch,
                        headless=self.headless,
                        args=launch_args,
                    )
                    log.info("Khởi chạy thành công qua %s", ch)
                    break
                except Exception:
                    continue
            if self._browser is None:
                raise exc
        self._context = await self._browser.new_context(
            user_agent=self.user_agent,
            viewport={"width": 1920, "height": 1080},
            locale="en-US",
            ignore_https_errors=True,
        )
        await self._context.add_init_script(STEALTH_JS)
        if self.cookies_path:
            try:
                cookies = load_cookies(self.cookies_path)
                await self._context.add_cookies(cookies)  # type: ignore[arg-type]
                log.info("Đã nạp %d cookies từ %s", len(cookies), self.cookies_path)
            except (OSError, ValueError, PlaywrightError) as exc:
                log.warning("Không nạp được cookies (%s) - tiếp tục không cookies", exc)
        return self

    async def __aexit__(
        self,
        exc_type: Optional[type[BaseException]],
        exc: Optional[BaseException],
        tb: Optional[TracebackType],
    ) -> None:
        for closer in (
            self._context.close if self._context else None,
            self._browser.close if self._browser else None,
            self._pw.stop if self._pw else None,
        ):
            if closer is None:
                continue
            try:
                await closer()
            except Exception as err:  # noqa: BLE001 - dọn dẹp không được làm crash
                log.debug("Lỗi khi đóng: %s", err)

    # ---------------------------------------------------------------- network
    async def _on_response(self, response: Response) -> None:
        try:
            if response.status != 200:
                return
            headers = response.headers
            ctype = headers.get("content-type", "").split(";")[0].strip().lower()
            if ctype not in IMAGE_TYPES and ctype not in BINARY_TYPES:
                return
            length = headers.get("content-length", "")
            if length.isdigit() and int(length) < self.min_capture_bytes:
                return
            body = await response.body()
            if len(body) < self.min_capture_bytes:
                return
            self._seq += 1
            self._captured[response.url] = CapturedImage(response.url, body, ctype, self._seq)
        except PlaywrightError as exc:
            log.debug("Không đọc được body %s: %s", response.url, exc)

    def get_captured(self, url: str) -> Optional[CapturedImage]:
        hit = self._captured.get(url)
        if hit is None and "#" in url:
            hit = self._captured.get(url.split("#", 1)[0])
        return hit

    def orphans(self, consumed: set[str]) -> list[CapturedImage]:
        """Ảnh đã bắt được nhưng chưa dùng (vd: preload), theo thứ tự tải về."""
        items = [c for u, c in self._captured.items() if u not in consumed]
        return sorted(items, key=lambda c: c.seq)

    # ------------------------------------------------------------------- page
    async def open(self, url: str, timeout_ms: int = 60_000) -> Page:
        assert self._context is not None, "BrowserSession chưa được khởi tạo"
        page = await self._context.new_page()
        page.on("response", self._on_response)
        last_exc: Optional[Exception] = None
        for attempt in range(1, 4):
            try:
                await page.goto(url, wait_until="domcontentloaded", timeout=timeout_ms)
                break
            except (PlaywrightTimeoutError, PlaywrightError) as exc:
                last_exc = exc
                log.warning("Mở trang lần %d/3 thất bại: %s", attempt, exc)
                await asyncio.sleep(2 * attempt)
        else:
            raise RuntimeError(f"Không mở được {url}") from last_exc
        try:
            await page.wait_for_load_state("networkidle", timeout=10_000)
        except PlaywrightTimeoutError:
            log.debug("networkidle timeout - tiếp tục")
        return page

    async def download(self, url: str, referer: str) -> Optional[bytes]:
        """Tải ảnh bằng httpx với cookies + Referer của phiên trình duyệt."""
        assert self._context is not None
        try:
            jar = {c["name"]: c["value"] for c in await self._context.cookies(url)}
        except PlaywrightError:
            jar = {}
        headers = {
            "User-Agent": self.user_agent,
            "Referer": referer,
            "Accept": "image/avif,image/webp,image/*,*/*;q=0.8",
        }
        async with httpx.AsyncClient(
            headers=headers, cookies=jar, follow_redirects=True, timeout=30.0
        ) as client:
            for attempt in range(3):
                try:
                    resp = await client.get(url)
                    resp.raise_for_status()
                    return resp.content
                except httpx.HTTPError as exc:
                    log.debug("httpx lần %d lỗi %s: %s", attempt + 1, url, exc)
                    await asyncio.sleep(1.5 * (attempt + 1))
        return None
