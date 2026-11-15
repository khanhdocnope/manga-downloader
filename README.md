# Manga DL (Rust, GUI desktop)

Bản port sang Rust của `manga_downloader` (Python). Tải ảnh gốc của một chapter manga dạng lật trang / trang đôi / cuộn dọc.

## Yêu cầu
- Rust ≥ 1.80 (`rustup`)
- Google Chrome, Chromium hoặc Edge cài sẵn (thay cho `playwright install chromium`).
  Chrome ở đường dẫn lạ: đặt biến môi trường `CHROME=/đường/dẫn/chrome`.
- Linux: cần các thư viện GUI/GTK cho `rfd`/`eframe` (vd Debian/Ubuntu: `libgtk-3-dev libxkbcommon-dev libssl-dev`).

## Chạy
```bash
cargo run --release
```
Nhập URL chapter → chọn chế độ / độ trễ / cookies / thư mục lưu → **Bắt đầu tải**.
- Ảnh hiện trên lưới ngay khi được lưu; bấm vào ảnh để mở cỡ đầy đủ bằng trình xem ảnh của hệ điều hành.
- **Dừng** kết thúc êm sau bước hiện tại và giữ các trang đã lưu (quá 20 giây sẽ huỷ cưỡng bức).
- **Mở thư mục** và **Tải file .ZIP** như bản web cũ.
- `MANGA_DL_DEBUG=1` in log chi tiết ra stderr.

## Ánh xạ Python → Rust
| Python | Rust |
|---|---|
| Playwright | `chromiumoxide` (Chrome DevTools Protocol) |
| httpx | `reqwest` (một client dùng chung, có connection pool) |
| Pillow (đọc header) | `imagesize` (không cần giải mã ảnh) |
| FastAPI + WebSocket + `web/index.html` | `egui/eframe` + kênh `mpsc` |
| asyncio | `tokio` |
| `scraper/{browser,detector,extractor}.py` | `src/scraper/*.rs` |
| `utils/file_manager.py` | `src/files.rs`, `src/naming.rs` |
| `server.py` (Job, stop, zip, open-folder) | `src/job.rs`, `src/app.rs` |

## Cải thiện tốc độ / đồng thời
- Lấy body ảnh từ mạng trong task riêng cho từng response (không xếp hàng trong event handler).
- `wait_captured` dùng `Notify` thay vì polling mỗi 150 ms.
- Trang đôi: lấy dữ liệu các ảnh song song (vẫn lưu theo đúng thứ tự đọc).
- Băm SHA-256 và đọc kích thước ảnh trên thread pool; thumbnail tạo nền, không chặn vòng lật trang.
- Client HTTP dùng chung thay vì tạo mới cho mỗi ảnh.

## Khác biệt so với bản Python
- Bản Python có regex `\\b` bị escape nhầm trong `LAYOUT_PROBE_JS` nên `ltrHint`/`hasCounter` không bao giờ khớp. Bản Rust dùng `\b` đúng, nên trang có class `ltr` giờ được nhận là LTR ngay từ đầu (extractor vẫn tự đảo hướng nếu sai).
- Cookies của tầng tải dự phòng lấy qua CDP `Network.getCookies` theo URL ảnh (tương đương `context.cookies(url)`).
- Chưa có CLI (`main.py`); GUI-only theo yêu cầu.

## Lưu ý
Mã chưa được biên dịch/chạy thử trong môi trường tạo ra nó (không có Rust toolchain/mạng). Lần đầu hãy chạy `cargo check` và sửa các sai khác nhỏ về API (đặc biệt `chromiumoxide`, `eframe`, `imagesize` nếu phiên bản crate khác).
