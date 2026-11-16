# Hướng dẫn Xuất bản Khoa học, Xác lập Quyền Tác giả Tiên phong & Vận hành GA `v1.0.0`

Tài liệu này hướng dẫn từng bước cụ thể để bạn:
1. Gắn tên thật + **ORCID** của bạn vào toàn bộ repo và bài báo.
2. Đóng dấu ưu tiên toàn cầu (bằng chứng ai làm đầu tiên: **GitHub + CERN Zenodo DOI + Software Heritage + Sigstore Rekor + arXiv**).
3. Chạy nốt cổng cloud trên Linux để đóng dấu phát hành chính thức **`v1.0.0`**.
4. Nộp bài báo và hồ sơ *Artifact Evaluation* lên hội nghị hạng A*.

---

## Bước 1 — Gắn danh tính tác giả thật & ORCID (Làm đầu tiên)

1. Đăng ký miễn phí mã định danh nhà nghiên cứu toàn cầu tại [https://orcid.org](https://orcid.org) (mất ~2 phút, nhận mã dạng `0000-000X-XXXX-XXXX`).
2. Chạy đúng 1 lệnh từ thư mục gốc của repo để đồng bộ tên thật, email, đơn vị công tác và ORCID vào cả `CITATION.cff`, `.zenodo.json`, `NOTICE`, `paper/author_config.tex` và `docs/paper/GAP-PAPER.md`:
   ```bash
   python3 dist/set-author.py \
     --given-names "Ten Cua Ban" \
     --family-names "Ho Cua Ban" \
     --affiliation "Don vi / Independent Researcher" \
     --email "email-chinh-thuc@domain.com" \
     --orcid "0000-000X-XXXX-XXXX" \
     --repo-url "https://github.com/<username>/GraphProtocol"
   ```
3. Kiểm tra lại cấu hình git trước khi commit & push:
   ```bash
   git config user.name "Ten Day Du"
   git config user.email "email-chinh-thuc@domain.com"
   git add CITATION.cff .zenodo.json NOTICE paper/author_config.tex docs/paper/GAP-PAPER.md
   git commit -m "chore(citation): set canonical author metadata and ORCID"
   ```

---

## Bước 2 — Khóa Dấu Thời gian Toàn cầu (Prior Art Proof)

Ngay sau khi đẩy repo lên GitHub ở chế độ Public:

1. **CERN Zenodo DOI (Miễn phí, vĩnh viễn):**
   - Đăng nhập [https://zenodo.org](https://zenodo.org) bằng tài khoản GitHub → mục **GitHub** → gạt bật repository `GraphProtocol` (file `.zenodo.json` đã tạo sẵn metadata chuẩn).
   - Khi tạo Release đầu tiên trên GitHub, Zenodo tự động lưu trữ mã nguồn và cấp **DOI khoa học chính thức**.
2. **Sigstore Rekor Transparency Log (Bất biến bằng mật mã học):**
   - Khi bạn chạy:
     ```bash
     git tag v0.1.0-rc.1 && git push origin v0.1.0-rc.1
     ```
     Workflow [`.github/workflows/release.yml`](../../.github/workflows/release.yml) sẽ chạy **OIDC keyless `cosign sign-blob`** và **SLSA v1.0 attestation**, ghi cố định danh tính GitHub + mã băm SHA-256 của GAP lên sổ cái công khai Rekor của Linux Foundation.
3. **Software Heritage Archive:**
   - Gửi URL repository vào [https://archive.softwareheritage.org/save](https://archive.softwareheritage.org/save) để lưu trữ mã nguồn dài hạn ở châu Âu.

---

## Bước 3 — Công bố Preprint lên arXiv

Kho bài báo LaTeX hoàn chỉnh đã được đóng gói sẵn tại:
- Mã nguồn LaTeX: [`paper/main.tex`](../../paper/main.tex) + [`paper/references.bib`](../../paper/references.bib) + 4 hình vector TikZ tại `paper/figures/*.tex`
- Bản xem trước Markdown/PDF/HTML: [`GAP-PAPER.md`](GAP-PAPER.md), [`paper/gap-paper.pdf`](../../paper/gap-paper.pdf), [`paper/gap-paper.html`](../../paper/gap-paper.html)

1. Xuất gói tar chuẩn arXiv AutoTeX:
   ```bash
   ./paper/build-arxiv-bundle.sh
   # Tạo file: target/gap-arxiv-submission.tar.gz
   ```
2. Đăng nhập [https://arxiv.org/submit](https://arxiv.org/submit):
   - **Primary category:** `cs.DC` (*Distributed, Parallel, and Cluster Computing*) hoặc `cs.SE` (*Software Engineering*).
   - **Cross-list:** `cs.AI` (*Artificial Intelligence*) và `cs.CR` (*Cryptography and Security*).
   - Upload trực tiếp file `target/gap-arxiv-submission.tar.gz`. Nếu tài khoản mới cần *endorsement*, bạn có thể nộp song song ngay trong ngày lên **Zenodo** (dùng DOI đã cấp ở Bước 2) hoặc **TechRxiv (IEEE)** để giữ mốc ngày ưu tiên ngay lập tức trong lúc xin endorsement arXiv.
3. Sau khi có mã `arXiv:2611.xxxxx` và DOI Zenodo, cập nhật ngược lại vào [`CITATION.cff`](../../CITATION.cff) để bất kỳ ai trích dẫn GAP đều dẫn thẳng về bài báo của bạn.

---

## Bước 4 — Chạy Nốt Cổng GA `v1.0.0` trên Máy Linux Cloud (`OPS-030`, `VER-053`)

Mọi kiểm tra trên máy macOS local đã được chạy và lưu bằng chứng tại [`bench/results/ga_local/report.json`](../../bench/results/ga_local/report.json). Để đóng nốt các cổng hạ tầng Linux/CI (`G-4`, `G-5`, `G-10`, `G-11`, `G-14`, `G-15`, `G-20`):

1. Thuê 1 máy Linux Ubuntu 24.04 (tối thiểu 8 vCPU, 32 GB RAM) và cài Docker, `promtool`, `cargo-llvm-cov`, toolchain `nightly` + `cargo-fuzz`.
2. Chạy kiểm tra nhanh trên Linux trước:
   ```bash
   ./dist/run-linux-ga-gates.sh --quick-check
   ```
3. Chạy bộ kiểm chứng phát hành GA đầy đủ (`llvm-cov`, `cargo-mutants`, soak `72 h`, fuzz `24 h` / target):
   ```bash
   ./dist/run-linux-ga-gates.sh --full-release
   ```
4. Chạy benchmark live LLM nếu muốn công bố thêm bảng đo với model frontier thật (`G-9`):
   ```bash
   cargo run --release -p gapbench -- --llm-endpoint <URL> --api-key-env <ENV> --out bench/results/gapbench-live
   ```
5. Cập nhật kết quả vào [`docs/gates/RC.md`](../gates/RC.md), chuyển tag sang `v1.0.0` và phát hành Release GA.

---

## Bước 5 — Mục tiêu Hội nghị Quốc tế & Artifact Evaluation

Dựa trên thế mạnh bài toán hệ thống + benchmark tái lập + mô hình bảo mật trong repo, thứ tự mục tiêu phù hợp nhất là:
1. **Systems & Distributed Infrastructure:** *USENIX ATC*, *EuroSys*, *VLDB (Industrial/Systems)* hoặc *SoCC*.
2. **Security & Zero-Trust Agent Isolation:** *USENIX Security*, *ACM CCS* hoặc *NDSS*.
3. **Benchmarks & Open Datasets (`GAP-Bench` + `RouteBench`):** *NeurIPS Datasets & Benchmarks Track*.

Khi nộp bài, đính kèm script tái lập nguyên vẹn chỉ bằng 1 lệnh (`./bench/results/gapbench/reproduce.sh` và `python3 dist/verify-ga-local.py`) để đạt đủ bộ huy hiệu *Artifacts Available + Artifacts Functional + Results Reproduced*.
