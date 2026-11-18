# Hướng dẫn Xuất bản Khoa học, Xác lập Quyền Tác giả Tiên phong & Vận hành GA `v1.0.0`

Tài liệu này hướng dẫn từng bước cụ thể để bạn:
1. Gắn tên thật + **ORCID** của bạn vào toàn bộ repo và bài báo.
2. Đóng dấu ưu tiên toàn cầu (bằng chứng ai làm đầu tiên: **GitHub + CERN Zenodo DOI + Software Heritage + Sigstore Rekor + arXiv**).
3. Chạy nốt cổng cloud trên Linux để đóng dấu phát hành chính thức **`v1.0.0`**.
4. Nộp bài báo và hồ sơ *Artifact Evaluation* lên hội nghị hạng A*.

---

## Bước 1 — Gắn danh tính tác giả thật & ORCID (ĐÃ HOÀN TẤT)

Đã gắn danh tính chính thức (`Nguyễn Duy Đức` / `Infinite Agent Labs` / `ORCID: 0009-0009-4741-5637` / `office@infiniteagentlabs.com`) đồng bộ vào `CITATION.cff`, `.zenodo.json`, `NOTICE`, `LICENSE`, `paper/author_config.tex`, `paper/main.tex` và `docs/paper/GAP-PAPER.md` bằng lệnh:

```bash
python3 dist/set-author.py \
  --given-names "Duy Đức" \
  --family-names "Nguyễn" \
  --affiliation "Infinite Agent Labs" \
  --email "office@infiniteagentlabs.com" \
  --orcid "0009-0009-4741-5637" \
  --repo-url "https://github.com/vnknowledge2014/GraphProtocol"
```

Nếu cần cập nhật lại trong tương lai, chạy lại `dist/set-author.py` và kiểm tra bằng `python3 dist/set-author.py --check`.

---

## Bước 2 — Khóa Dấu Thời gian Toàn cầu (Prior Art Proof)

Ngay sau khi đẩy repo lên GitHub ở chế độ Public:

1. **CERN Zenodo DOI (Miễn phí, vĩnh viễn):**
   - Đăng nhập [https://zenodo.org](https://zenodo.org) bằng tài khoản GitHub → mục **GitHub** → gạt bật repository `GraphProtocol` (file `.zenodo.json` đã tạo sẵn metadata chuẩn).
   - Khi tạo Release đầu tiên trên GitHub, Zenodo tự động lưu trữ mã nguồn và cấp **DOI khoa học chính thức**.
2. **Sigstore Rekor Transparency Log & SLSA v1.0 (Bất biến bằng mật mã học — ĐÃ HOÀN TẤT trên `v0.1.0-rc.1`):**
   - Workflow [`.github/workflows/release.yml`](../../.github/workflows/release.yml) ([Run `44233037780`](https://github.com/vnknowledge2014/GraphProtocol/actions/runs/44233037780)) đã tự động ký **OIDC keyless `cosign sign-blob --bundle`** (tạo file `.sigstore.json` với log indices Rekor `3960581407`, `3960581618`, `3960581821`) và cấp chứng thực **SLSA v1.0 provenance** (`https://github.com/vnknowledge2014/GraphProtocol/attestations/68950887`, Rekor `3960582357`) gắn vĩnh viễn danh tính repo + mã SHA-256 trên sổ cái công khai Rekor của Linux Foundation. Khi chốt `v1.0.0` chỉ cần đẩy tag mới `git tag v1.0.0 && git push origin v1.0.0`.
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

## Bước 4 — Chạy Nốt Cổng GA `v1.0.0` ngay trên Máy 64 GB (`Colima`) + GitHub Actions Miễn phí (`OPS-030`, `VER-053`)

Không cần thuê máy ảo cloud trả phí. Toàn bộ các cổng Linux/CI có thể xác minh tự động đã chạy xanh qua **GitHub Actions miễn phí** (`CI` [`44233030991`](https://github.com/vnknowledge2014/GraphProtocol/actions/runs/44233030991) với coverage `81.26%` regions / `80.08%` lines và `136/136` conformance, `Release` [`44233037780`](https://github.com/vnknowledge2014/GraphProtocol/actions/runs/44233037780) với Distroless `nonroot` `19.4 MB` + `Cosign` + `SLSA v1.0`, `Nightly` [`44233039093`](https://github.com/vnknowledge2014/GraphProtocol/actions/runs/44233039093) với `10/10` fuzzers `libFuzzer` + `ASan`) kết hợp với **Colima Linux Ubuntu 24.04 (`x86_64`)** ngay trên máy Mac 64 GB (`Bubblewrap 0.9.0`, `promtool 16 rules`, `graphql-js 16.13.2`, container `gap:ci`).

Nếu muốn chạy trọn vẹn pha thử bền kéo dài theo thời gian thực (`72 h` soak `G-11` = `259200 s` và `24 h`/target fuzzing `G-14` = `86400 s` + `cargo-mutants`) trước khi đổi tên tag từ `v0.1.0-rc.1` sang `v1.0.0`:

1. Bật VM Linux trên máy Mac 64 GB qua Colima:
   ```bash
   colima start --cpu 8 --memory 24 --disk 80
   ```
2. Chạy kiểm tra nhanh trên Linux trước (hoặc trong container Ubuntu 24.04):
   ```bash
   ./dist/run-linux-ga-gates.sh --quick-check
   ```
3. Chạy bộ thử bền đầy đủ (`llvm-cov`, `cargo-mutants`, soak `72 h`, song song `10` fuzzer `24 h`/target):
   ```bash
   ./dist/run-linux-ga-gates.sh --full-release
   ```
4. Chạy benchmark live LLM nếu muốn công bố thêm bảng đo với model frontier thật (`G-9`):
   ```bash
   cargo run --release -p gapbench -- --llm-endpoint <URL> --api-key-env <ENV> --out bench/results/gapbench-live
   ```
5. Dừng Colima để giải phóng 24 GB RAM khi xong (`colima stop`), cập nhật log vào [`docs/gates/RC.md`](../gates/RC.md), chuyển tag sang `v1.0.0` và phát hành Release GA.

---

## Bước 5 — Mục tiêu Hội nghị Quốc tế & Artifact Evaluation

Dựa trên thế mạnh bài toán hệ thống + benchmark tái lập + mô hình bảo mật trong repo, thứ tự mục tiêu phù hợp nhất là:
1. **Systems & Distributed Infrastructure:** *USENIX ATC*, *EuroSys*, *VLDB (Industrial/Systems)* hoặc *SoCC*.
2. **Security & Zero-Trust Agent Isolation:** *USENIX Security*, *ACM CCS* hoặc *NDSS*.
3. **Benchmarks & Open Datasets (`GAP-Bench` + `RouteBench`):** *NeurIPS Datasets & Benchmarks Track*.

Khi nộp bài, đính kèm script tái lập nguyên vẹn chỉ bằng 1 lệnh (`./bench/results/gapbench/reproduce.sh` và `python3 dist/verify-ga-local.py`) để đạt đủ bộ huy hiệu *Artifacts Available + Artifacts Functional + Results Reproduced*.
