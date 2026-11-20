# Graph Agent Protocol Multi-Page Documentation & Interactive Portal (English · [Tiếng Việt](README.vi.md))

Topic-focused **26-page bilingual documentation site** (13 English pages + 13 Vietnamese mirror pages under [`vi/`](vi/index.html)) structured for step-by-step clarity:

- **7 topic groups / 13 guides per language:** Start Here (What is GAP?, 10-Min Quickstart), Core Concepts (Three-Plane Architecture, Progressive Disclosure, GAP-QL & Joins), Native Graph Connectors (NGC), AI Agent Hosts & Routers (MCP/A2A/AG-UI + System-1 & HTTP/3 QUIC), Zero-Trust Governance, Interactive Lab (Crossover Calculator, Live `gap serve`, 6-Level Checklist, Quiz), and Reference (15 CLI Commands, 9 Runbooks, 32 Crates, 20 Invariants, ASD-STE100, Academic Paper).
- **Clean static output + agent index:** Built from [`web/site_src/`](site_src/nav.py) via `python3 web/build_site.py`, including [`llms.txt`](llms.txt), cross-page search (`search-index.en.js` / `search-index.vi.js`), copyable Markdown & code blocks, responsive 3-column shell with right-hand scrollspy, and [`vi.html`](vi.html) redirecting to [`vi/index.html`](vi/index.html).

Live portals on GitHub Pages:
- **English:** [https://vnknowledge2014.github.io/GraphProtocol/](https://vnknowledge2014.github.io/GraphProtocol/) (`web/index.html` + 12 topic subpages)
- **Tiếng Việt:** [https://vnknowledge2014.github.io/GraphProtocol/vi/](https://vnknowledge2014.github.io/GraphProtocol/vi/) (also [`/vi.html`](https://vnknowledge2014.github.io/GraphProtocol/vi.html))

## 1. Rebuild & Validate Static Site

```bash
python3 web/build_site.py
python3 web/build_site.py --check
```

## 2. Run the Live Interactive Lab (`gap serve` + Real MCP/GraphQL Requests)

From the repository root:

```bash
python3 web/serve.py --port 8787
```

Then open `http://127.0.0.1:8787/` (English) or `http://127.0.0.1:8787/vi/` (Vietnamese), or browse directly to `/lab/playground.html` (`/vi/lab/playground.html`) and click **Start Live Lab (`gap serve`)** to spawn the local Walking Skeleton gateway and send live GraphQL/MCP `2026-07-28` requests or inspect `--help` from `./target/dist/gap`.
