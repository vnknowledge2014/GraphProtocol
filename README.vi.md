# GAP — Graph Agent Protocol

> **Ngôn ngữ:** [English (README.md)](README.md) · **Tiếng Việt**

**Graph Agent Protocol (GAP)** là cổng gateway định kiểu chặt chẽ theo profile GraphQL kèm tầng quản trị an toàn (giấy phép Apache-2.0, `Copyright 2026 Mike (vnknowledge2014)`), giúp hợp nhất các backend AI Agent không đồng nhất (OpenAPI REST, GraphQL subgraph, gRPC, SQL, CLI tool, AST code/doc graph, MCP server ngoài, tác vụ ngang hàng A2A và giao diện AG-UI v1.0) phía sau **khám phá lũy tiến (progressive disclosure)**, **liên kết gom lô (batched federation)**, **tầng truyền tải dual-stack HTTP/2 + HTTP/3 (QUIC)**, **mô hình quyết định System-1 đa nhà cung cấp (`Laya`, `Jev`, `gemini-decision-1`)**, **phân quyền tất định Cedar**, và **bản chụp snapshot bất biến không khóa**.

**Trạng thái (`0.1.0-rc.1` · `2026-11-17`):** **87 / 87 work packages hoàn tất (`main`)**, đã kiểm chứng offline đầu-cuối (`362` yêu cầu đặc tả / `89` MUST / `2.511` test markers, `135/136` conformance tests, `32` crates Rust trong workspace, `0` lỗ hổng `RUSTSEC` với cấu trúc AVL `OrdMap` tự sở hữu). Kho mã nguồn chính thức: [`github.com/vnknowledge2014/GraphProtocol`](https://github.com/vnknowledge2014/GraphProtocol) · Cổng học tập, video & bài báo trực tuyến: [`vnknowledge2014.github.io/GraphProtocol`](https://vnknowledge2014.github.io/GraphProtocol/) ([`/paper/`](https://vnknowledge2014.github.io/GraphProtocol/paper/) · [Video Giới thiệu Tiếng Việt](web/media/gap-promo-vi.mp4) · [Video Giới thiệu Tiếng Anh](web/media/gap-promo-en.mp4)).

---

## Kết quả Thực nghiệm Nổi bật (`0.1.0-rc.1`, `ADR-021`)

| Hạng mục đo đạc | Giá trị thực nghiệm | Bằng chứng đối chiếu |
|---|---|---|
| Chi phí schema MCP tĩnh `2026-07-28` | Cố định **`651` token** cho `N trong {10..1.000}` (so với `203.421` của Direct-MCP ở `N=1.000`) | [`docs/benchmarks/REPORT.md`](docs/benchmarks/REPORT.md) |
| Giảm token khi join liên nhánh (`N=1.000`) | **`99,0%`** không cache (`95,34x`) · **`93,5%`** có cache (`15,42x`) · số lượt `4 -> 2` | [`bench/results/gapbench/baseline.json`](bench/results/gapbench/baseline.json) |
| Duyệt đồ thị mã nguồn (`24` symbols) | **`119,32x`** không cache · **`208,68x`** có cache · số lượt `11 -> 1` | [`bench/results/gapbench/baseline.json`](bench/results/gapbench/baseline.json) |
| Ngưỡng đối chứng trung thực (`ADR-017`) | Direct-MCP vẫn ít token hơn ở tập phẳng nhỏ có cache: `N=10` (`547` vs `4.692`) và `N=50` (`2.385` vs `4.728`) | [`docs/benchmarks/REPORT.md`](docs/benchmarks/REPORT.md) |
| Bộ định tuyến tất định so với System-1 (`G-S1`) | Baseline đạt **`95,24%`** tập test / **`97,22%`** tập held-out top-1 ở **`0,887 ms`** p95 (dự phòng `INV-17`) | [`docs/decisions/G-S1.md`](docs/decisions/G-S1.md) |
| Binary release `--locked` tái lập từng bit | **`26,9 MB`** (`26.902.360` B), SHA-256 hai lần build giống hệt (`62224cb5...89d308acf`), **`0` lỗ hổng RUSTSEC** | [`bench/results/ga_local/report.json`](bench/results/ga_local/report.json) |
| Cold start / RSS / TCP + HTTP/3 QUIC / `SIGKILL` | **`314 ms`** khởi động · **`9,24 MiB`** idle RSS · **`2.228 req/s`** TCP (`5,11 ms` p95) · **`7.870 req/s`** HTTP/3 QUIC (`1,45 ms` p95, tắt `0-RTT`, `0` lỗi) · **`12,96 ms`** phục hồi sau `kill -9` | [`bench/results/ga_local/report.json`](bench/results/ga_local/report.json) |

---

## Khởi chạy Nhanh & Kiểm chứng Cục bộ

```bash
# 1. Chạy thử Walking Skeleton 5 phút (REST shop + MCP notes + gateway GraphQL/MCP)
./target/dist/gap config check examples/walking-skeleton/gap.toml
python3 examples/walking-skeleton/fixtures/rest_fixture.py --port 9101 &
python3 examples/walking-skeleton/fixtures/mcp_fixture.py --port 9102 &
./target/dist/gap serve --config examples/walking-skeleton/gap.toml

# 2. Tái lập GAP-Bench & các bài kiểm tra runtime / SIGKILL / reproducible build cục bộ
./bench/results/gapbench/reproduce.sh
python3 dist/verify-ga-local.py

# 3. Kiểm tra đặc tả, phân tầng crate workspace, ma trận truy vết và gói bài báo
python3 tools/wp.py validate && python3 tools/check_layers.py && python3 tools/spec_trace.py --strict
./paper/build-arxiv-bundle.sh
```

---

## Tài liệu, Đặc tả & Bài báo Khoa học

| Hạng mục | Đường dẫn |
|---|---|
| **Website Học tập Tương tác & Video Demo EN/VI** | [`web/README.md`](web/README.md) · [`web/index.html`](web/index.html) · [`gap-promo-vi.mp4`](web/media/gap-promo-vi.mp4) · [`gap-promo-en.mp4`](web/media/gap-promo-en.mp4) · `python3 web/serve.py --port 8787` |
| **Bài báo Khoa học (LaTeX / HTML / PDF / Markdown)** | [`paper/main.tex`](paper/main.tex) · [`paper/gap-paper.pdf`](paper/gap-paper.pdf) · [`docs/paper/GAP-PAPER.md`](docs/paper/GAP-PAPER.md) |
| **Sổ tay Kỹ thuật Chuẩn ASD-STE100 (Kèm Linter)** | [`docs/ste100/README.md`](docs/ste100/README.md) · `python3 dist/check-ste100.py` |
| **Hướng dẫn Nộp arXiv / Zenodo DOI & Chạy GA Linux** | [`docs/paper/PUBLICATION-AND-GA-PLAYBOOK.md`](docs/paper/PUBLICATION-AND-GA-PLAYBOOK.md) · [`dist/set-author.py`](dist/set-author.py) |
| **Cổng Kiểm định Phát hành `1.0-RC` & Ma trận Truy vết** | [`docs/gates/RC.md`](docs/gates/RC.md) · [`docs/traceability.md`](docs/traceability.md) |
| **Hướng dẫn Bắt đầu Nhanh, Khái niệm, Connector & Bảo mật** | [`docs/guides/quickstart.md`](docs/guides/quickstart.md) · [`docs/guides/concepts.md`](docs/guides/concepts.md) · [`docs/guides/connector-authoring.md`](docs/guides/connector-authoring.md) · [`docs/guides/security-hardening.md`](docs/guides/security-hardening.md) |
| **Đặc tả Chuẩn (`00`–`09`), Quyết định Kiến trúc (ADRs) & Rà soát** | [`spec/`](spec/) · [`docs/01-DECISIONS.md`](docs/01-DECISIONS.md) · [`docs/00-REVIEW.md`](docs/00-REVIEW.md) |
| **Hợp đồng Wire Proto & Gói Công việc (`87/87` hoàn tất)** | [`proto/`](proto/) · [`work/work-packages.toml`](work/work-packages.toml) · [`work/status.toml`](work/status.toml) |

---

## Trích dẫn Học thuật

Siêu dữ liệu trích dẫn chuẩn cho phần mềm và bài báo nằm trong [`CITATION.cff`](CITATION.cff) và [`.zenodo.json`](.zenodo.json). Có thể kiểm tra tính đồng nhất thông tin tác giả và URL kho mã nguồn bằng lệnh:

```bash
python3 dist/set-author.py --check
```

`Copyright 2026 Mike (vnknowledge2014)` · Giấy phép: [Apache-2.0](LICENSE) · Thông báo Bản quyền & Thành phần Bên thứ ba: [`NOTICE`](NOTICE)
