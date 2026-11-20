# Graph Agent Protocol Interactive Learning Portal (English · [Tiếng Việt](README.vi.md))

Interactive bilingual web portal covering the complete architecture of **Graph Agent Protocol (`GAP`)** — including **dual-stack HTTP/2 + HTTP/3 QUIC (RFC 9114)**, **AG-UI v1.0 (`/ag-ui`)**, **multi-provider System-1 routers (`Laya` / `Jev` / `Gemini Decision 1`)**, **EN/VI product videos**, **zero `RUSTSEC` advisories**, Light/Dark modes, a 6-level learning path with browser progress tracking, an interactive `GAP-Bench` Crossover Calculator, all 32 Rust crates (`L0`–`L6`), 15 CLI commands, 9 incident runbooks, 20 invariants (`INV-01`–`INV-20`), ASD-STE100 controlled glossary, and a self-check quiz.

Live portals on GitHub Pages:
- **English:** [https://vnknowledge2014.github.io/GraphProtocol/](https://vnknowledge2014.github.io/GraphProtocol/) (`web/index.html` + `web/data.en.js`)
- **Tiếng Việt:** [https://vnknowledge2014.github.io/GraphProtocol/vi.html](https://vnknowledge2014.github.io/GraphProtocol/vi.html) (`web/vi.html` + `web/data.vi.js`)

## 1. Quick Open in Static / Simulation Mode

Open [`index.html`](index.html) (English) or [`vi.html`](vi.html) (Vietnamese) in any modern browser.

## 2. Run the Live Interactive Lab (`gap serve` + Real MCP/GraphQL Requests)

From the repository root:

```bash
python3 web/serve.py --port 8787
```

Then open `http://127.0.0.1:8787/` (English) or `http://127.0.0.1:8787/vi.html` (Vietnamese) and click **Start Live Lab (`gap serve`)** to spawn the local Walking Skeleton gateway and send live GraphQL/MCP `2026-07-28` requests or run `--help` on the local `./target/dist/gap` binary.
