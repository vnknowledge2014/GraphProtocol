# Graph Agent Protocol Multi-Page Documentation & Interactive Portal (English · [Tiếng Việt](README.vi.md))

Topic-focused **40-page bilingual documentation, Learn Basics & GAP Academy portal** (20 English pages + 20 Vietnamese mirror pages under [`vi/`](vi/index.html)) structured for step-by-step clarity:

- **9 topic groups / 20 pages per language:** Start Here (What is GAP?, 10-Min Quickstart), **Learn Basics** ([GraphQL & GAP Basics](learn/graphql-and-gap-basics.html) with interactive Query-to-JSON Builder, Arguments/Mutations/Joins), **GAP Academy** ([Academy Hub](academy/index.html) + 4 multi-lesson courses / 16 lessons covering Lift-Off, MCP/AG-UI, REST/SQL N+1 Batching, and Enterprise Zero-Trust), Core Concepts, Native Graph Connectors (NGC), AI Agent Hosts & Routers, Zero-Trust Governance, Interactive Lab (Crossover Calculator, Live `gap serve`, 6-Level Checklist, Quiz), and Reference (15 CLI Commands, 9 Runbooks, 32 Crates, 20 Invariants, ASD-STE100, Academic Paper).
- **Clean static output + agent index:** Built from [`web/site_src/`](site_src/nav.py) via `python3 web/build_site.py` (checked with `python3 web/build_site.py --check` including SVG text bounding-box checks), generating [`llms.txt`](llms.txt), cross-page search (`search-index.en.js` / `search-index.vi.js`), copyable Markdown & code blocks, responsive 3-column shell with right-hand scrollspy, and [`vi.html`](vi.html) redirecting to [`vi/index.html`](vi/index.html).

Live portals on GitHub Pages:
- **English:** [https://vnknowledge2014.github.io/GraphProtocol/](https://vnknowledge2014.github.io/GraphProtocol/) (`web/index.html` + 19 topic, Learn & Academy subpages)
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
