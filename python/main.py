"""Manga Chapter Downloader - CLI entry point."""
from __future__ import annotations

import argparse
import asyncio
import logging
import sys
from pathlib import Path
from typing import Optional
from urllib.parse import unquote, urlparse

from rich.console import Console
from rich.logging import RichHandler
from rich.progress import Progress, SpinnerColumn, TextColumn, TimeElapsedColumn

from scraper.browser import BrowserSession
from scraper.detector import WebtoonDetected
from scraper.extractor import ChapterExtractor
from utils.file_manager import FileManager, SavedImage, slugify

console = Console()
log = logging.getLogger("manga_dl")


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="manga-dl",
        description="Tải toàn bộ ảnh gốc của một chapter manga (reader lật trang). Từ chối Webtoon.",
    )
    p.add_argument("url", help="URL của chapter")
    p.add_argument("--output", "-o", type=Path, default=Path("downloads"), help="Thư mục gốc (mặc định: downloads)")
    p.add_argument("--cookies", type=Path, default=None, help="File JSON cookies (đăng nhập / Cloudflare)")
    p.add_argument("--headless", action=argparse.BooleanOptionalAction, default=True,
                   help="Chạy ẩn trình duyệt (mặc định bật; tắt bằng --no-headless)")
    p.add_argument("--delay", type=float, default=1.0, help="Giây chờ sau mỗi lần lật trang (mặc định 1.0)")
    p.add_argument("--manga", default=None, help="Ghi đè tên truyện dùng cho thư mục")
    p.add_argument("--chapter", default=None, help="Ghi đè tên chapter dùng cho thư mục")
    p.add_argument("--max-pages", type=int, default=500, help="Giới hạn số lần lật trang (mặc định 500)")
    p.add_argument(
        "--mode",
        choices=["auto", "manga", "webtoon"],
        default="auto",
        help="Chế độ tải: manga (lật trang), webtoon (cuộn dọc), auto (tự động)",
    )
    p.add_argument("--strict", action="store_true",
                   help="Chỉ lưu ảnh gắn với trang đã lật tới, bỏ ảnh preload không khớp")
    p.add_argument("--verbose", "-v", action="store_true", help="Log chi tiết")
    return p


def derive_names(url: str, title: str) -> tuple[str, str]:
    """Suy ra (manga_slug, chapter_slug) từ URL, dự phòng bằng <title>."""
    segments = [unquote(s) for s in urlparse(url).path.split("/") if s]
    chapter = slugify(segments[-1]) if segments else "chapter"
    manga = slugify(segments[-2]) if len(segments) >= 2 else ""
    if not manga or manga.isdigit():
        head = title.split("|")[0].split(" - ")[0].strip()
        manga = slugify(head, fallback=slugify(urlparse(url).netloc, "manga"))
    return manga, chapter


async def run(args: argparse.Namespace) -> int:
    async with BrowserSession(headless=args.headless, cookies_path=args.cookies) as session:
        console.print(f"[bold]Mở:[/] {args.url}")
        page = await session.open(args.url)
        try:
            title = await page.title()
        except Exception:  # noqa: BLE001
            title = ""
        manga, chapter = derive_names(args.url, title)
        manga = slugify(args.manga) if args.manga else manga
        chapter = slugify(args.chapter) if args.chapter else chapter
        out_dir = args.output / manga / chapter
        console.print(f"[bold]Thư mục đầu ra:[/] {out_dir}")

        files = FileManager(out_dir)
        with Progress(
            SpinnerColumn(), TextColumn("{task.description}"), TimeElapsedColumn(),
            console=console, transient=False,
        ) as progress:
            task = progress.add_task("Đang tải... 0 trang", total=None)

            def on_saved(item: SavedImage) -> None:
                progress.update(task, description=f"Đang tải... {item.index} trang (mới: {item.path.name})")

            extractor = ChapterExtractor(
                session, page, files,
                delay=args.delay, max_pages=args.max_pages,
                mode=args.mode,
                strict=args.strict, on_saved=on_saved,
            )
            try:
                await extractor.run()
            except WebtoonDetected as exc:
                progress.stop()
                console.print(f"[bold red]Lỗi:[/] {exc}")
                return 2

        stats = files.stats
        console.print(
            f"[bold green]Hoàn tất:[/] {files.count} trang -> {out_dir}  "
            f"[dim](trùng: {stats['duplicate']}, nhỏ: {stats['small']}, lỗi: {stats['invalid']})[/]"
        )
        return 0 if files.count else 1


def main(argv: Optional[list[str]] = None) -> int:
    args = build_parser().parse_args(argv)
    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(message)s",
        handlers=[RichHandler(console=console, show_path=False, rich_tracebacks=True)],
    )
    logging.getLogger("httpx").setLevel(logging.WARNING)
    try:
        return asyncio.run(run(args))
    except KeyboardInterrupt:
        console.print("[yellow]Đã dừng bởi người dùng - các trang đã lưu được giữ nguyên.[/]")
        return 130
    except RuntimeError as exc:
        console.print(f"[bold red]Lỗi:[/] {exc}")
        return 1


if __name__ == "__main__":
    sys.exit(main())
