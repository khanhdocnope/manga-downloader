"""Khởi chạy Manga DL Desktop / Web UI.

Cách dùng:
  - Mặc định: mở cửa sổ Native Desktop App (Edge/Chrome App Mode)
  - Mở tab trình duyệt: python run_ui.py --browser
  - Chạy nền server: python run_ui.py --no-browser
"""
from __future__ import annotations

import argparse
import os
import socket
import subprocess
import sys
import threading
import time
import webbrowser
from pathlib import Path
from typing import Optional

# Khi chạy từ PyInstaller --windowed (không mở cửa sổ console đen),
# sys.stdout, sys.stderr, sys.stdin mặc định là None trên Windows.
# Cần gán về os.devnull để tránh AttributeError 'isatty' khi uvicorn/logging khởi tạo.
if sys.stdout is None:
    sys.stdout = open(os.devnull, "w", encoding="utf-8")
if sys.stderr is None:
    sys.stderr = open(os.devnull, "w", encoding="utf-8")
if sys.stdin is None:
    sys.stdin = open(os.devnull, "r", encoding="utf-8")

if getattr(sys, "frozen", False):
    BUNDLE_DIR = Path(sys._MEIPASS)
    BASE_DIR = Path.cwd()
else:
    BUNDLE_DIR = Path(__file__).resolve().parent
    BASE_DIR = BUNDLE_DIR

os.chdir(BASE_DIR)
sys.path.insert(0, str(BUNDLE_DIR))

import uvicorn  # noqa: E402


def _port_busy(host: str, port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.settimeout(0.3)
        return s.connect_ex((host, port)) == 0


def _find_free_port(host: str, start_port: int = 8000) -> int:
    port = start_port
    while _port_busy(host, port):
        port += 1
    return port


def _find_app_browser() -> Optional[str]:
    candidates = [
        # Microsoft Edge (mặc định có trên 100% máy Windows 10/11)
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
        os.path.expandvars(r"%ProgramFiles(x86)%\Microsoft\Edge\Application\msedge.exe"),
        os.path.expandvars(r"%ProgramFiles%\Microsoft\Edge\Application\msedge.exe"),
        # Google Chrome
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        os.path.expandvars(r"%LocalAppData%\Google\Chrome\Application\chrome.exe"),
        os.path.expandvars(r"%ProgramFiles%\Google\Chrome\Application\chrome.exe"),
        # macOS
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        # Linux
        "/usr/bin/google-chrome",
        "/usr/bin/microsoft-edge",
        "/usr/bin/chromium-browser",
    ]
    for c in candidates:
        if c and os.path.isfile(c):
            return c
    return None


def _launch_ui(host: str, port: int, server: uvicorn.Server, mode: str) -> None:
    for _ in range(60):
        if _port_busy(host, port):
            break
        time.sleep(0.15)

    url = f"http://{host}:{port}"

    if mode == "app":
        browser_bin = _find_app_browser()
        if browser_bin:
            try:
                # Khởi chạy cửa sổ ứng dụng độc lập, không có thanh địa chỉ duyệt web
                cmd = [browser_bin, f"--app={url}", "--window-size=1260,840"]
                proc = subprocess.Popen(cmd)
                proc.wait()  # Khi người dùng đóng cửa sổ app, tự động tắt server
                server.should_exit = True
                return
            except Exception:
                pass

    # Fallback mở tab trình duyệt thông thường
    webbrowser.open(url)


def main() -> int:
    parser = argparse.ArgumentParser(description="Manga DL Desktop")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8000)
    parser.add_argument("--browser", action="store_true", help="Mở tab trình duyệt thay vì cửa sổ desktop")
    parser.add_argument("--no-browser", action="store_true", help="Không tự mở giao diện")
    args = parser.parse_args()

    port = args.port
    if _port_busy(args.host, port):
        port = _find_free_port(args.host, port + 1)

    from server import app

    config = uvicorn.Config(
        app,
        host=args.host,
        port=port,
        log_config=None,
        log_level="warning",
        loop="asyncio",
    )
    server = uvicorn.Server(config)

    if not args.no_browser:
        mode = "browser" if args.browser else "app"
        threading.Thread(target=_launch_ui, args=(args.host, port, server, mode), daemon=True).start()

    print(f"⛩ Manga DL đang chạy tại: http://{args.host}:{port}")
    server.run()
    return 0


if __name__ == "__main__":
    sys.exit(main())
