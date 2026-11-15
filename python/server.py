"""FastAPI server cho Web UI: /ws (WebSocket), /api/* và trang tĩnh web/index.html."""
from __future__ import annotations

import asyncio
import io
import json
import logging
import os
import re
import subprocess
import sys
import tempfile
import time
import uuid
import zipfile
from collections import deque
from contextlib import asynccontextmanager
from pathlib import Path
from typing import Any, AsyncIterator, Optional
from urllib.parse import urlparse

from fastapi import FastAPI, HTTPException, Request, WebSocket, WebSocketDisconnect
from fastapi.responses import FileResponse, JSONResponse, Response
from PIL import Image
from starlette.background import BackgroundTask

from main import derive_names
from scraper.browser import BrowserSession
from scraper.detector import WEBTOON_MESSAGE, WebtoonDetected
from scraper.extractor import ChapterExtractor
from utils.file_manager import FileManager, SavedImage

if getattr(sys, "frozen", False):
    BUNDLE_DIR = Path(sys._MEIPASS)
    BASE_DIR = Path.cwd()
else:
    BUNDLE_DIR = Path(__file__).resolve().parent
    BASE_DIR = BUNDLE_DIR

WEB_DIR = BUNDLE_DIR / "web"
ACTIVE = {"starting", "guard", "running", "stopping"}
IMAGE_NAME = re.compile(r"^\d{3,5}\.(jpg|png|webp|avif|gif)$")
LOCAL_HOSTS = {"127.0.0.1", "localhost", "::1"}

log = logging.getLogger("manga_dl.server")


# ----------------------------------------------------------------- bảo vệ localhost
def _hostname(value: str) -> str:
    value = value.strip()
    if value.startswith("["):  # IPv6: [::1]:8000
        return value[1 : value.find("]")] if "]" in value else value
    return value.rsplit(":", 1)[0] if value.count(":") == 1 else value


def _request_allowed(headers: Any) -> bool:
    """Chặn DNS-rebinding / CSRF từ trang web khác gọi vào server cục bộ."""
    if _hostname(headers.get("host", "")) not in LOCAL_HOSTS:
        return False
    origin = headers.get("origin")
    if origin and _hostname(urlparse(origin).netloc) not in LOCAL_HOSTS:
        return False
    return True


# ------------------------------------------------------------------------ job
class Job:
    def __init__(self, config: dict[str, Any]) -> None:
        self.id = uuid.uuid4().hex[:8]
        self.config = config
        self.status = "starting"
        self.message = ""
        self.turn = 0
        self.bytes = 0
        self.out_dir: Optional[Path] = None
        self.zip_name = "chapter"
        self.pages: list[dict[str, Any]] = []
        self.logs: deque[dict[str, str]] = deque(maxlen=400)
        self.thumbs: dict[str, bytes] = {}
        self.extractor: Optional[ChapterExtractor] = None
        self.task: Optional[asyncio.Task[None]] = None
        self.stop_requested = False

    @property
    def running(self) -> bool:
        return self.status in ACTIVE

    def state(self) -> dict[str, Any]:
        return {
            "type": "state",
            "job_id": self.id,
            "status": self.status,
            "message": self.message,
            "turn": self.turn,
            "count": len(self.pages),
            "bytes": self.bytes,
            "out_dir": str(self.out_dir) if self.out_dir else "",
            "running": self.running,
        }

    def snapshot(self) -> dict[str, Any]:
        return {"type": "snapshot", "state": self.state(), "logs": list(self.logs), "pages": list(self.pages)}

    def set_status(self, status: str, message: str = "", turn: Optional[int] = None) -> None:
        self.status = status
        self.message = message
        if turn is not None:
            self.turn = turn
        STATE.publish(self.state())

    def add_log(self, level: str, message: str) -> None:
        entry = {"ts": time.strftime("%H:%M:%S"), "level": level, "message": message}
        self.logs.append(entry)
        STATE.publish({"type": "log", **entry})

    def request_stop(self) -> None:
        if self.stop_requested or not self.running:
            return
        self.stop_requested = True
        self.set_status("stopping", "Đang dừng, các trang đã lưu được giữ lại")
        if self.extractor is not None:
            self.extractor.request_stop()
        elif self.task is not None:
            self.task.cancel()
        asyncio.get_running_loop().create_task(self._force_cancel())

    async def _force_cancel(self) -> None:
        await asyncio.sleep(20)
        if self.task is not None and not self.task.done():
            log.warning("Dừng êm quá 20s - huỷ cưỡng bức")
            self.task.cancel()


class AppState:
    def __init__(self) -> None:
        self.job: Optional[Job] = None
        self.clients: set[asyncio.Queue[dict[str, Any]]] = set()

    def publish(self, event: dict[str, Any]) -> None:
        for queue in list(self.clients):
            queue.put_nowait(event)

    def snapshot(self) -> dict[str, Any]:
        if self.job:
            return self.job.snapshot()
        idle = {"type": "state", "job_id": "", "status": "idle", "message": "", "turn": 0,
                "count": 0, "bytes": 0, "out_dir": "", "running": False}
        return {"type": "snapshot", "state": idle, "logs": [], "pages": []}


STATE = AppState()


class _JobLogHandler(logging.Handler):
    """Chuyển log của browser/detector/files vào luồng log của job."""

    def __init__(self, job: Job) -> None:
        super().__init__(logging.INFO)
        self.job = job

    def emit(self, record: logging.LogRecord) -> None:
        try:
            self.job.add_log(record.levelname.lower(), record.getMessage())
        except Exception:  # noqa: BLE001
            self.handleError(record)


def _parse_config(raw: dict[str, Any]) -> dict[str, Any]:
    url = str(raw.get("url", "")).strip()
    parsed = urlparse(url)
    if parsed.scheme not in ("http", "https") or not parsed.netloc:
        raise ValueError("URL không hợp lệ: cần bắt đầu bằng http:// hoặc https://")
    try:
        delay_ms = max(0, min(int(raw.get("delay_ms", 800)), 30_000))
        max_pages = max(1, min(int(raw.get("max_pages", 500)), 2000))
    except (TypeError, ValueError) as exc:
        raise ValueError("Độ trễ / số trang tối đa phải là số nguyên") from exc
    cookies = raw.get("cookies")
    if cookies:
        try:
            json.loads(cookies)
        except (TypeError, ValueError) as exc:
            raise ValueError("Cookies không phải JSON hợp lệ") from exc
    mode = str(raw.get("mode", "auto")).lower()
    if mode not in ("auto", "manga", "webtoon"):
        mode = "auto"
    out = Path(str(raw.get("output_dir") or "downloads").strip() or "downloads").expanduser()
    return {
        "url": url,
        "mode": mode,
        "headless": bool(raw.get("headless", True)),
        "delay_ms": delay_ms,
        "max_pages": max_pages,
        "cookies": cookies or None,
        "output_dir": out.resolve(),
    }


async def _run_job(job: Job) -> None:
    cfg = job.config
    handlers: list[tuple[logging.Logger, logging.Handler]] = []
    for name in ("manga_dl.browser", "manga_dl.detector", "manga_dl.files"):
        lg = logging.getLogger(name)
        handler = _JobLogHandler(job)
        lg.addHandler(handler)
        handlers.append((lg, handler))
    cookies_path: Optional[Path] = None

    async def on_saved(item: SavedImage) -> None:
        job.bytes += item.size
        page = {"index": item.index, "name": item.path.name, "width": item.width,
                "height": item.height, "size": item.size}
        job.pages.append(page)
        STATE.publish({"type": "page", **page, "count": len(job.pages), "bytes": job.bytes})

    async def on_log(level: str, message: str) -> None:
        job.add_log(level, message)

    async def on_status(phase: str, info: dict[str, Any]) -> None:
        if job.stop_requested:
            return
        job.set_status(phase, "", info.get("turn"))

    try:
        if cfg["cookies"]:
            fd, tmp = tempfile.mkstemp(prefix="mdl-cookies-", suffix=".json")
            with os.fdopen(fd, "w", encoding="utf-8") as fh:
                fh.write(cfg["cookies"])
            cookies_path = Path(tmp)

        job.set_status("starting", "Đang khởi tạo Chromium")
        job.add_log("info", f"Bắt đầu: {cfg['url']}")
        async with BrowserSession(headless=cfg["headless"], cookies_path=cookies_path) as session:
            page = await session.open(cfg["url"])
            try:
                title = await page.title()
            except Exception:  # noqa: BLE001
                title = ""
            manga, chapter = derive_names(cfg["url"], title)
            job.out_dir = cfg["output_dir"] / manga / chapter
            job.zip_name = f"{manga}-{chapter}"
            job.add_log("info", f"Thư mục lưu: {job.out_dir}")

            extractor = ChapterExtractor(
                session, page, FileManager(job.out_dir),
                delay=cfg["delay_ms"] / 1000, max_pages=cfg["max_pages"],
                mode=cfg.get("mode", "auto"),
                on_saved=on_saved, on_log=on_log, on_status=on_status,
            )
            job.extractor = extractor
            if job.stop_requested:
                extractor.request_stop()
            await extractor.run()

        count = len(job.pages)
        if job.stop_requested:
            job.set_status("stopped", f"Đã dừng, giữ lại {count} trang")
        elif count == 0:
            job.set_status("error", "Không lưu được trang nào. Thử tắt Headless để xem trang, hoặc nạp cookies.")
        else:
            job.set_status("done", f"Đã lưu {count} trang")
    except WebtoonDetected as exc:
        job.add_log("warning", str(exc))
        job.set_status("webtoon", str(exc))
    except asyncio.CancelledError:
        job.set_status("stopped", f"Đã dừng, giữ lại {len(job.pages)} trang")
    except Exception as exc:  # noqa: BLE001
        log.exception("Job lỗi")
        text = str(exc).strip().splitlines()[0][:300] if str(exc).strip() else exc.__class__.__name__
        if "playwright install" in str(exc):
            text = "Chưa cài Chromium cho Playwright. Chạy: playwright install chromium"
        job.add_log("error", text)
        job.set_status("error", text)
    finally:
        for lg, handler in handlers:
            lg.removeHandler(handler)
        if cookies_path is not None:
            cookies_path.unlink(missing_ok=True)


async def _command(message: dict[str, Any]) -> None:
    action = message.get("action")
    if action == "start":
        if STATE.job and STATE.job.running:
            STATE.publish({"type": "error", "message": "Đang có một tiến trình chạy. Dừng nó trước khi bắt đầu lại."})
            return
        try:
            cfg = _parse_config(message.get("config") or {})
        except ValueError as exc:
            STATE.publish({"type": "error", "message": str(exc)})
            return
        job = Job(cfg)
        STATE.job = job
        STATE.publish(job.snapshot())  # reset giao diện cho job mới
        job.task = asyncio.create_task(_run_job(job))
    elif action == "stop":
        if STATE.job:
            STATE.job.request_stop()
    else:
        STATE.publish({"type": "error", "message": f"Lệnh không hợp lệ: {action}"})


# ------------------------------------------------------------------------ app
@asynccontextmanager
async def lifespan(_: FastAPI) -> AsyncIterator[None]:
    yield
    job = STATE.job
    if job and job.task and not job.task.done():
        job.task.cancel()
        try:
            await asyncio.wait_for(job.task, timeout=10)
        except (asyncio.CancelledError, asyncio.TimeoutError, Exception):  # noqa: BLE001
            pass


app = FastAPI(title="Manga DL", lifespan=lifespan)


@app.middleware("http")
async def local_only(request: Request, call_next: Any) -> Response:
    if not _request_allowed(request.headers):
        return JSONResponse({"detail": "Chỉ chấp nhận truy cập từ localhost"}, status_code=403)
    return await call_next(request)


@app.get("/")
async def index() -> FileResponse:
    return FileResponse(WEB_DIR / "index.html", headers={"Cache-Control": "no-store"})


@app.websocket("/ws")
async def websocket_endpoint(ws: WebSocket) -> None:
    if not _request_allowed(ws.headers):
        await ws.close(code=1008)
        return
    await ws.accept()
    queue: asyncio.Queue[dict[str, Any]] = asyncio.Queue()
    STATE.clients.add(queue)
    await ws.send_json(STATE.snapshot())

    async def writer() -> None:
        while True:
            await ws.send_json(await queue.get())

    writer_task = asyncio.create_task(writer())
    try:
        while True:
            try:
                message = await ws.receive_json()
            except ValueError:
                queue.put_nowait({"type": "error", "message": "Tin nhắn không phải JSON hợp lệ"})
                continue
            if isinstance(message, dict):
                await _command(message)
    except WebSocketDisconnect:
        pass
    finally:
        writer_task.cancel()
        STATE.clients.discard(queue)


def _current_job(job_id: str) -> Job:
    job = STATE.job
    if job is None or job.id != job_id:
        raise HTTPException(404, "Không tìm thấy tiến trình này")
    return job


def _page_path(job: Job, name: str) -> Path:
    if not IMAGE_NAME.match(name) or job.out_dir is None:
        raise HTTPException(404, "Tên file không hợp lệ")
    path = job.out_dir / name
    if not path.is_file():
        raise HTTPException(404, "Không có ảnh này")
    return path


def _make_thumb(path: Path) -> bytes:
    with Image.open(path) as im:
        w, h = im.size
        target_w = 480
        if w > target_w:
            target_h = max(1, int(h * (target_w / w)))
            im = im.resize((target_w, target_h), Image.Resampling.LANCZOS)
        im = im.convert("RGB")
        buf = io.BytesIO()
        im.save(buf, "JPEG", quality=88)
        return buf.getvalue()


@app.get("/api/thumb/{job_id}/{name}")
async def thumb(job_id: str, name: str) -> Response:
    job = _current_job(job_id)
    path = _page_path(job, name)
    data = job.thumbs.get(name)
    if data is None:
        data = await asyncio.to_thread(_make_thumb, path)
        job.thumbs[name] = data
    return Response(data, media_type="image/jpeg", headers={"Cache-Control": "private, max-age=3600"})


@app.get("/api/image/{job_id}/{name}")
async def image(job_id: str, name: str) -> FileResponse:
    job = _current_job(job_id)
    return FileResponse(_page_path(job, name), headers={"Cache-Control": "private, max-age=3600"})


@app.post("/api/open-folder")
async def open_folder() -> dict[str, Any]:
    job = STATE.job
    if job is None or job.out_dir is None or not job.out_dir.exists():
        raise HTTPException(404, "Chưa có thư mục nào để mở")
    target = str(job.out_dir)
    try:
        if sys.platform.startswith("win"):
            os.startfile(target)  # type: ignore[attr-defined]
        elif sys.platform == "darwin":
            subprocess.Popen(["open", target])
        else:
            subprocess.Popen(["xdg-open", target])
    except OSError as exc:
        raise HTTPException(500, f"Không mở được thư mục: {exc}") from exc
    return {"ok": True, "path": target}


def _build_zip(out_dir: Path, names: list[str], base: str, dest: str) -> None:
    with zipfile.ZipFile(dest, "w", zipfile.ZIP_STORED) as zf:  # ảnh đã nén sẵn
        for name in names:
            zf.write(out_dir / name, arcname=f"{base}/{name}")


@app.get("/api/download-zip")
async def download_zip() -> FileResponse:
    job = STATE.job
    if job is None or job.out_dir is None or not job.pages:
        raise HTTPException(404, "Chưa có trang nào để nén")
    names = [p["name"] for p in job.pages if (job.out_dir / p["name"]).is_file()]
    if not names:
        raise HTTPException(404, "Các file ảnh không còn trên đĩa")
    fd, tmp = tempfile.mkstemp(prefix="mdl-", suffix=".zip")
    os.close(fd)
    await asyncio.to_thread(_build_zip, job.out_dir, names, job.zip_name, tmp)
    return FileResponse(
        tmp, media_type="application/zip", filename=f"{job.zip_name}.zip",
        background=BackgroundTask(os.unlink, tmp),
    )
