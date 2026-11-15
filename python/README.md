# manga-dl

CLI tải ảnh gốc của một chapter manga (reader lật trang / trang đôi). Tự từ chối Webtoon cuộn dọc.

## Giao diện Web (tuỳ chọn)
```bash
python run_ui.py            # tự mở http://127.0.0.1:8000
python run_ui.py --port 8080 --no-browser
```
- Nhập URL, chỉnh Headless / độ trễ / cookies / thư mục lưu rồi bấm **Bắt đầu tải**. Ảnh hiện trên lưới ngay khi được lưu; bấm vào ảnh để xem cỡ đầy đủ.
- **Dừng** kết thúc êm sau bước hiện tại và giữ lại các trang đã lưu.
- Server chỉ nhận truy cập từ `localhost`/`127.0.0.1` (kiểm tra Host và Origin).
- Giao diện cần Internet để tải Tailwind, Lucide và Google Fonts từ CDN.
- CLI (`main.py`) vẫn dùng như cũ.

## Cài đặt
```bash
python -m venv .venv && source .venv/bin/activate   # Windows: .venv\Scripts\activate
pip install -r requirements.txt
playwright install chromium
```

## Chạy thử
```bash
python main.py "https://example.com/manga/ten-truyen/chapter-12"
python main.py "<url>" --output ./truyen --delay 1.5 --no-headless
python main.py "<url>" --cookies cookies.json --manga "Ten Truyen" --chapter "Chapter 12"
```
Kết quả: `downloads/<ten_truyen>/<chapter>/001.jpg, 002.png, ...`

## Tham số
| Tham số | Ý nghĩa |
|---|---|
| `url` | URL chapter |
| `--output, -o` | Thư mục gốc (mặc định `downloads`) |
| `--cookies` | File JSON cookies (Playwright hoặc export từ extension trình duyệt) |
| `--headless / --no-headless` | Ẩn/hiện trình duyệt (mặc định ẩn) |
| `--delay` | Giây chờ sau mỗi lần lật trang |
| `--manga`, `--chapter` | Ghi đè tên thư mục |
| `--max-pages` | Giới hạn số lần lật (mặc định 500) |
| `--strict` | Bỏ ảnh preload không gắn với trang đã lật |
| `-v` | Log chi tiết |

Mã thoát: `0` thành công, `1` không lưu được trang nào/lỗi, `2` phát hiện Webtoon, `130` bị Ctrl+C.

Chỉ dùng cho nội dung bạn có quyền tải và tôn trọng điều khoản của trang nguồn.
