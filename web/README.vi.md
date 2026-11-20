# Cổng Tài liệu Đa trang & Lab Tương tác Graph Agent Protocol (Tiếng Việt · [English](README.md))

Bộ **website tài liệu song ngữ 26 trang theo từng chủ đề riêng biệt** (13 trang Tiếng Anh + 13 trang Tiếng Việt đồng bộ 1-1 tại [`vi/`](vi/index.html)), thiết kế trực quan theo mạch giải thích khái niệm – sơ đồ tư duy – từng bước thực hành – bảng đánh đổi minh bạch:

- **7 nhóm chuyên mục / 13 trang mỗi ngôn ngữ:** Bắt đầu (`GAP là gì?`, `Khởi chạy nhanh 10 phút`), Kiến trúc Cốt lõi (3 Mặt phẳng, Khám phá lũy tiến 651 tokens, GAP-QL & DataLoader), Kết nối Dữ liệu (NGC), Tích hợp AI Agent (MCP/A2A/AG-UI + System-1 & HTTP/3 QUIC), Bảo mật Zero-Trust & Cedar, Phòng Lab Thực hành (Bộ tính Crossover GAP-Bench, `gap serve` sống, Checklist 6 cấp độ, Trắc nghiệm), và Tra cứu & Vận hành (15 lệnh CLI, 9 Runbook, 32 Crates, 20 Bất biến `INV-01..20`, ASD-STE100, Bài báo PDF/HTML).
- **Biên dịch & kiểm định tự động:** Sinh từ [`web/site_src/`](site_src/nav.py) qua `python3 web/build_site.py`, kèm danh mục [`llms.txt`](llms.txt), tìm kiếm toàn site (`search-index.vi.js` / `search-index.en.js`), nút **Sao chép Markdown** & copy snippet, mục lục Scrollspy bên phải và [`vi.html`](vi.html) chuyển hướng mượt sang [`vi/index.html`](vi/index.html).

Truy cập bản công khai trực tuyến cùng bài báo HTML/PDF tại:
- **Tiếng Việt:** [https://vnknowledge2014.github.io/GraphProtocol/vi/](https://vnknowledge2014.github.io/GraphProtocol/vi/) (hoặc [`/vi.html`](https://vnknowledge2014.github.io/GraphProtocol/vi.html))
- **English:** [https://vnknowledge2014.github.io/GraphProtocol/](https://vnknowledge2014.github.io/GraphProtocol/)

## 1. Tạo lại & Kiểm tra Cổng Tài liệu Tĩnh

```bash
python3 web/build_site.py
python3 web/build_site.py --check
```

## 2. Chạy chế độ Lab Thực hành Sống (`gap serve` + MCP/GraphQL thật)

Từ thư mục gốc của repository:

```bash
python3 web/serve.py --port 8787
```

Sau đó mở `http://127.0.0.1:8787/vi/` (hoặc `http://127.0.0.1:8787/` cho bản Tiếng Anh), vào `/vi/lab/playground.html` và bấm nút **Bật Lab Thực Hành (`gap serve`)** để máy chủ tự động khởi chạy Walking Skeleton cục bộ trên cổng nội bộ, gửi request GraphQL, MCP `2026-07-28` thật hoặc xem `--help` từ binary `./target/dist/gap`.
