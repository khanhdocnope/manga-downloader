"""Lưu ảnh bất đồng bộ, lọc kích thước, khử trùng lặp bằng SHA-256."""
from __future__ import annotations

import asyncio
import hashlib
import io
import logging
import re
import unicodedata
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

import aiofiles
from PIL import Image, UnidentifiedImageError

try:  # AVIF cho Pillow cũ (Pillow >= 11.3 đã hỗ trợ sẵn)
    import pillow_avif  # type: ignore  # noqa: F401
except ImportError:
    pass

log = logging.getLogger("manga_dl.files")

MIN_WIDTH = 450
MIN_HEIGHT = 600

_EXT_BY_FORMAT: dict[str, str] = {
    "JPEG": ".jpg",
    "PNG": ".png",
    "WEBP": ".webp",
    "AVIF": ".avif",
    "GIF": ".gif",
}


def slugify(text: str, fallback: str = "untitled") -> str:
    """Chuyển chuỗi (kể cả tiếng Việt) thành slug ASCII an toàn cho tên thư mục."""
    text = text.replace("đ", "d").replace("Đ", "D")
    text = unicodedata.normalize("NFKD", text).encode("ascii", "ignore").decode("ascii")
    text = re.sub(r"[^a-zA-Z0-9]+", "-", text).strip("-").lower()
    return text[:80] or fallback


@dataclass(frozen=True)
class SavedImage:
    index: int
    path: Path
    sha256: str
    width: int
    height: int
    source: str
    size: int = 0

    @property
    def name(self) -> str:
        return self.path.name


class FileManager:
    def __init__(
        self,
        out_dir: Path,
        min_width: int = MIN_WIDTH,
        min_height: int = MIN_HEIGHT,
    ) -> None:
        self.out_dir = out_dir
        self.min_width = min_width
        self.min_height = min_height
        self._saved: list[SavedImage] = []
        self._hashes: set[str] = set()
        self._rejected: set[str] = set()
        self._lock = asyncio.Lock()
        self.stats: dict[str, int] = {"small": 0, "duplicate": 0, "invalid": 0}

    @property
    def saved(self) -> list[SavedImage]:
        return list(self._saved)

    @property
    def count(self) -> int:
        return len(self._saved)

    def average_aspect_ratio(self) -> float:
        """Trung bình height/width của các ảnh đã chấp nhận (dùng cho Webtoon Guard)."""
        if not self._saved:
            return 0.0
        return sum(s.height / s.width for s in self._saved) / len(self._saved)

    @staticmethod
    def sha256(data: bytes) -> str:
        return hashlib.sha256(data).hexdigest()

    @staticmethod
    def _inspect(data: bytes) -> Optional[tuple[int, int, str]]:
        try:
            with Image.open(io.BytesIO(data)) as img:
                fmt = (img.format or "").upper()
                return img.width, img.height, fmt
        except (UnidentifiedImageError, OSError, ValueError):
            return None

    async def save(
        self,
        data: bytes,
        source: str = "",
        force_ext: Optional[str] = None,
        custom_index: Optional[int] = None,
    ) -> Optional[SavedImage]:
        """Lưu ảnh nếu hợp lệ, đủ lớn và chưa trùng. Trả về None nếu bị loại."""
        digest = self.sha256(data)
        async with self._lock:
            if digest in self._hashes or digest in self._rejected:
                self.stats["duplicate"] += 1
                return None

            info = await asyncio.to_thread(self._inspect, data)
            if info is None:
                self._rejected.add(digest)
                self.stats["invalid"] += 1
                log.debug("Bỏ qua (không phải ảnh hợp lệ): %s", source)
                return None

            width, height, fmt = info
            if width < self.min_width or height < self.min_height:
                self._rejected.add(digest)
                self.stats["small"] += 1
                log.debug("Bỏ qua (%dx%d quá nhỏ): %s", width, height, source)
                return None

            ext = force_ext or _EXT_BY_FORMAT.get(fmt)
            if ext is None:
                self._rejected.add(digest)
                self.stats["invalid"] += 1
                return None

            index = custom_index if custom_index is not None else (len(self._saved) + 1)
            path = self.out_dir / f"{index:03d}{ext}"
            try:
                self.out_dir.mkdir(parents=True, exist_ok=True)
                async with aiofiles.open(path, "wb") as fh:
                    await fh.write(data)
            except OSError as exc:
                log.error("Không ghi được %s: %s", path, exc)
                return None

            self._hashes.add(digest)
            item = SavedImage(index, path, digest, width, height, source, len(data))
            self._saved.append(item)
            self._saved.sort(key=lambda x: x.index)
            return item
