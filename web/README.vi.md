# Website Học Tập Tương Tác Graph Agent Protocol (Tiếng Việt · [English](README.md))

Website song ngữ Anh/Việt hướng dẫn chi tiết cách sử dụng và làm chủ kiến trúc **Graph Agent Protocol (`GAP`)** — bao gồm **dual-stack HTTP/2 + HTTP/3 QUIC (RFC 9114)**, **AG-UI v1.0 (`/ag-ui`)**, **System-1 (`Laya` / `Jev` / `Gemini Decision 1`)**, **video giới thiệu EN/VI**, **0 lỗ hổng `RUSTSEC`**, chế độ Sáng/Tối, lộ trình học 6 cấp độ có lưu tiến độ chung giữa hai ngôn ngữ, bộ tính Crossover từ dữ liệu thật `GAP-Bench`, bản đồ 32 crates (`L0`–`L6`), 15 lệnh CLI, 9 runbooks, 20 bất biến (`INV-01`–`INV-20`), từ điển chuẩn ASD-STE100 và trắc nghiệm.

Truy cập bản công khai trực tuyến cùng bài báo HTML/PDF tại:
- **Tiếng Việt:** [https://vnknowledge2014.github.io/GraphProtocol/vi.html](https://vnknowledge2014.github.io/GraphProtocol/vi.html) (`web/vi.html` + `web/data.vi.js`)
- **English:** [https://vnknowledge2014.github.io/GraphProtocol/](https://vnknowledge2014.github.io/GraphProtocol/) (`web/index.html` + `web/data.en.js`)

## 1. Mở nhanh ở chế độ Tĩnh / Mô phỏng

Mở trực tiếp [`vi.html`](vi.html) (Tiếng Việt) hoặc [`index.html`](index.html) (English) bằng bất kỳ trình duyệt hiện đại nào.

## 2. Chạy chế độ Lab Thực hành Sống (`gap serve` + MCP/GraphQL thật)

Từ thư mục gốc của repository:

```bash
python3 web/serve.py --port 8787
```

Sau đó mở `http://127.0.0.1:8787/vi.html` (hoặc `http://127.0.0.1:8787/` cho bản Tiếng Anh) và bấm nút **Bật Lab Thực Hành (`gap serve`)** để máy chủ tự động khởi chạy Walking Skeleton cục bộ trên cổng nội bộ và cho phép gửi request GraphQL, MCP `2026-07-28` hoặc xem `--help` trực tiếp từ binary `./target/dist/gap`.
