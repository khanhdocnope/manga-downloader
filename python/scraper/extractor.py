"""Vòng lặp lật trang + trích xuất ảnh (network buffer -> DOM/httpx -> canvas).

Callback (đều có thể là hàm thường hoặc coroutine):
    on_saved(item: SavedImage)            - khi một trang được lưu thành công
    on_log(level: str, message: str)      - thay thế logging mặc định (Web UI dùng cái này)
    on_status(phase: str, info: dict)     - phase: "guard" | "running" (info["turn"] = số lần đã lật)
Không truyền callback nào -> hành vi y hệt bản CLI cũ (ghi log qua module logging).
"""
from __future__ import annotations

import asyncio
import base64
import hashlib
import inspect
import logging
from dataclasses import dataclass
from typing import Any, Callable, Optional

from playwright.async_api import Error as PlaywrightError
from playwright.async_api import Page
from playwright.async_api import TimeoutError as PlaywrightTimeoutError

from scraper.browser import BrowserSession
from scraper.detector import ReadingDirection, WebtoonDetected, detect_reader
from utils.file_manager import FileManager, SavedImage

log = logging.getLogger("manga_dl.extractor")

VISIBLE_MEDIA_JS = """
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
"""

WAIT_IMAGES_JS = """
() => {
  const vis = Array.from(document.querySelectorAll('img')).filter(i => {
    const r = i.getBoundingClientRect();
    return r.width >= 200 && r.height >= 200 && r.bottom > 0 && r.top < innerHeight;
  });
  return vis.every(i => i.complete && i.naturalWidth > 0);
}
"""


@dataclass(frozen=True)
class Media:
    kind: str  # "img" | "canvas"
    left: float
    top: float
    width: float
    height: float
    src: Optional[str]
    b64: Optional[str]

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "Media":
        return cls(d["kind"], d["left"], d["top"], d["width"], d["height"], d.get("src"), d.get("b64"))

    def key(self) -> str:
        if self.src and not self.src.startswith(("blob:", "data:")):
            return self.src
        if self.b64:
            return "h:" + hashlib.sha1(self.b64.encode()).hexdigest()
        return f"{self.kind}:{self.src}"


Signature = tuple[str, ...]
SavedCallback = Callable[[SavedImage], Any]
LogCallback = Callable[[str, str], Any]
StatusCallback = Callable[[str, dict[str, Any]], Any]


class ChapterExtractor:
    def __init__(
        self,
        session: BrowserSession,
        page: Page,
        files: FileManager,
        delay: float = 1.0,
        max_pages: int = 500,
        stagnation_limit: int = 3,
        strict: bool = False,
        mode: str = "auto",
        on_saved: Optional[SavedCallback] = None,
        on_log: Optional[LogCallback] = None,
        on_status: Optional[StatusCallback] = None,
    ) -> None:
        self.session = session
        self.page = page
        self.files = files
        self.delay = delay
        self.max_pages = max_pages
        self.stagnation_limit = stagnation_limit
        self.strict = strict
        self.mode = mode.lower()
        self.initial_url = page.url
        self.on_saved = on_saved
        self.on_log = on_log
        self.on_status = on_status
        self.direction = ReadingDirection.RTL
        self.turn_action: Optional[str] = "scroll" if self.mode == "webtoon" else None
        self._consumed: set[str] = set()
        self._stop = False

    # --------------------------------------------------------------- callbacks
    @staticmethod
    async def _emit(callback: Optional[Callable[..., Any]], *args: Any) -> None:
        if callback is None:
            return
        try:
            result = callback(*args)
            if inspect.isawaitable(result):
                await result
        except Exception:  # noqa: BLE001 - callback lỗi không được làm hỏng việc tải
            log.exception("Callback gây lỗi (bỏ qua)")

    async def _log(self, level: str, message: str) -> None:
        if self.on_log is not None:
            if level != "debug":
                await self._emit(self.on_log, level, message)
        else:
            getattr(log, level)(message)

    async def _status(self, phase: str, **info: Any) -> None:
        await self._emit(self.on_status, phase, info)

    def request_stop(self) -> None:
        """Yêu cầu dừng êm: kết thúc sau bước hiện tại, giữ nguyên các trang đã lưu."""
        self._stop = True

    # ------------------------------------------------------------------ public
    async def run(self) -> int:
        """Trả về số ảnh đã lưu. WebtoonDetected được ném ra cho caller xử lý."""
        await self._status("guard")
        info = await detect_reader(self.page)
        self.direction = info.direction

        try:
            await self._traverse()
        except WebtoonDetected:
            raise
        except PlaywrightError as exc:
            await self._log(
                "error", f"Gián đoạn giữa chừng ({exc}) - giữ lại {self.files.count} trang đã lưu"
            )
        except Exception as exc:  # noqa: BLE001 - không để mất dữ liệu đã tải
            log.exception("Lỗi không mong đợi")
            await self._log(
                "error", f"Lỗi không mong đợi: {exc} - giữ lại {self.files.count} trang đã lưu"
            )

        if not self.strict and not self._stop:
            await self._flush_orphans()
        return self.files.count

    # --------------------------------------------------------------- traversal
    async def _traverse(self) -> None:
        await self._status("running", turn=0)
        await self._wait_render()
        media = await self._read_visible()
        sig = self._signature(media)
        seen: set[Signature] = {sig}
        await self._harvest(media)

        confirmed = False  # đã xác nhận được hướng lật chưa
        fails = 0
        turns = 0
        while not self._stop and turns < self.max_pages and fails < self.stagnation_limit:
            # Khóa URL: ngăn chặn việc trang web tự động nhảy sang chapter kế tiếp!
            curr_url_base = self.page.url.split("?")[0].split("#")[0].rstrip("/")
            init_url_base = self.initial_url.split("?")[0].split("#")[0].rstrip("/")
            if curr_url_base != init_url_base:
                await self._log("info", "Đã đến cuối chapter (trang web chuyển hướng sang chapter mới). Dừng tải để giữ đúng chapter hiện tại.")
                break

            moved = await self._turn(media, sig, seen, confirmed)
            if moved is None:
                fails += 1
                await self._log("debug", f"Lật trang không đổi ({fails}/{self.stagnation_limit})")
                media = await self._read_visible()  # có thể trang vừa tải xong
                continue
            media, sig, flipped = moved
            if flipped:
                self.direction = (
                    ReadingDirection.LTR if self.direction == ReadingDirection.RTL else ReadingDirection.RTL
                )
                await self._log("info", f"Đảo hướng lật trang -> {self.direction.value.upper()}")
            confirmed = True
            fails = 0
            turns += 1
            seen.add(sig)
            await self._status("running", turn=turns)
            await self._harvest(media)

        if self._stop:
            await self._log("info", f"Đã nhận lệnh dừng sau {turns} lần lật")
        else:
            await self._log("info", f"Dừng lật trang sau {turns} lần lật (thất bại liên tiếp: {fails})")

    async def _turn(
        self,
        media: list[Media],
        sig: Signature,
        seen: set[Signature],
        confirmed: bool,
    ) -> Optional[tuple[list[Media], Signature, bool]]:
        """Thử các cách lật trang / cuộn trang; trả về (media mới, signature mới, có đảo hướng không)."""
        opposite = (
            ReadingDirection.LTR if self.direction == ReadingDirection.RTL else ReadingDirection.RTL
        )

        if self.mode == "webtoon":
            # Chế độ Webtoon: chỉ cuộn trang, tuyệt đối KHÔNG bấm phím mũi tên tránh nhảy chapter
            plan: list[tuple[str, ReadingDirection, bool]] = [("scroll", self.direction, False)]
        elif self.mode == "manga":
            # Chế độ Manga: chỉ dùng phím và click mép
            plan = [
                ("key", self.direction, False),
                ("click", self.direction, False),
            ]
            if not confirmed:
                plan += [("key", opposite, True), ("click", opposite, True)]
        else:
            # Chế độ Auto: ưu tiên action đã thành công, fallback đầy đủ
            plan = []
            if self.turn_action:
                plan.append((self.turn_action, self.direction, False))

            base_plan: list[tuple[str, ReadingDirection, bool]] = [
                ("key", self.direction, False),
                ("click", self.direction, False),
            ]
            if not confirmed:
                base_plan += [
                    ("key", opposite, True),
                    ("click", opposite, True),
                    ("scroll", self.direction, False),
                ]
            elif self.turn_action != "scroll":
                base_plan.append(("scroll", self.direction, False))

            for item in base_plan:
                if item not in plan:
                    plan.append(item)

        for action, direction, flipped in plan:
            if self._stop:
                return None
            try:
                if action == "key":
                    await self.page.keyboard.press(
                        "ArrowLeft" if direction == ReadingDirection.RTL else "ArrowRight"
                    )
                elif action == "click":
                    await self._click_edge(media, left=direction == ReadingDirection.RTL)
                elif action == "scroll":
                    at_bottom = await self.page.evaluate(
                        """() => {
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
                        }"""
                    )
                    if at_bottom:
                        continue
                await self._wait_render()
                new_media = await self._read_visible()
            except PlaywrightError as exc:
                await self._log("debug", f"Lật bằng {action} lỗi: {exc}")
                continue
            new_sig = self._signature(new_media)
            if new_sig and new_sig != sig and new_sig not in seen:
                if not self.turn_action and action == "scroll":
                    await self._log("info", "Phát hiện reader cuộn dọc -> chuyển sang chế độ cuộn trang")
                self.turn_action = action
                return new_media, new_sig, flipped
        return None

    async def _click_edge(self, media: list[Media], left: bool) -> None:
        if not media:
            vp = self.page.viewport_size or {"width": 1920, "height": 1080}
            x0, y0, w, h = 0.0, 0.0, float(vp["width"]), float(vp["height"])
        else:
            x0 = min(m.left for m in media)
            y0 = min(m.top for m in media)
            w = max(m.left + m.width for m in media) - x0
            h = max(m.top + m.height for m in media) - y0
        x = x0 + w * (0.10 if left else 0.90)  # nằm trong vùng 20% mép
        y = y0 + h * 0.5
        await self.page.mouse.click(x, y)

    async def _wait_render(self) -> None:
        try:
            await self.page.wait_for_load_state("networkidle", timeout=4_000)
        except PlaywrightTimeoutError:
            pass
        try:
            await self.page.wait_for_function(WAIT_IMAGES_JS, timeout=6_000)
        except PlaywrightTimeoutError:
            await self._log("debug", "Ảnh chưa render xong trong 6s")
        await asyncio.sleep(self.delay)

    # -------------------------------------------------------------- extraction
    async def _read_visible(self) -> list[Media]:
        raw: list[dict[str, Any]] = await self.page.evaluate(VISIBLE_MEDIA_JS)
        return [Media.from_dict(d) for d in raw]

    @staticmethod
    def _signature(media: list[Media]) -> Signature:
        return tuple(m.key() for m in sorted(media, key=lambda m: (round(m.top, -2), round(m.left, -2))))

    async def _harvest(self, media: list[Media]) -> None:
        if getattr(self, "turn_action", None) == "scroll":
            ordered = sorted(media, key=lambda m: m.top)
        else:
            # Trang đôi: RTL đọc ảnh bên phải trước, LTR đọc ảnh bên trái trước.
            ordered = sorted(
                media, key=lambda m: m.left, reverse=self.direction == ReadingDirection.RTL
            )
        for m in ordered:
            data, ext = await self._obtain_bytes(m)
            if data is None:
                await self._log("warning", f"Không lấy được dữ liệu ảnh: {(m.src or m.kind)[:90]}")
                continue
            await self._save(data, m.src or m.kind, ext)

    async def _obtain_bytes(self, m: Media) -> tuple[Optional[bytes], Optional[str]]:
        # Tầng 1: buffer bắt từ network (không dính 403 hotlink / blob hết hạn)
        if m.src and m.src.startswith("http"):
            cap = await self._wait_captured(m.src)
            if cap is not None:
                self._consumed.add(cap.url)
                return cap.data, None
        # Tầng 2: blob/data URL đã đọc trong trang, hoặc tải bằng httpx + cookies
        if m.kind == "img":
            if m.b64:
                return self._decode(m.b64), None
            if m.src and m.src.startswith("http"):
                data = await self.session.download(m.src, referer=self.page.url)
                if data:
                    self._consumed.add(m.src)
                    return data, None
        # Tầng 3: canvas.toDataURL
        elif m.b64:
            return self._decode(m.b64), ".png"
        return None, None

    async def _wait_captured(self, url: str, timeout: float = 1.5) -> Optional[Any]:
        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout
        while True:
            cap = self.session.get_captured(url)
            if cap is not None or loop.time() >= deadline:
                return cap
            await asyncio.sleep(0.15)

    @staticmethod
    def _decode(b64: str) -> Optional[bytes]:
        try:
            return base64.b64decode(b64)
        except (ValueError, TypeError):
            return None

    async def _save(self, data: Optional[bytes], source: str, ext: Optional[str]) -> None:
        if not data:
            return
        saved = await self.files.save(data, source=source, force_ext=ext)
        if saved is None:
            return
        await self._emit(self.on_saved, saved)
        # Webtoon Guard đã được mở khóa theo yêu cầu
        pass

    async def _flush_orphans(self) -> None:
        """Lưu nốt ảnh preload đã bắt được nhưng chưa gắn với trang nào."""
        if getattr(self, "turn_action", None) == "scroll":
            return
        for cap in self.session.orphans(self._consumed):
            await self._save(cap.data, cap.url, None)
