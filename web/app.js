// Shared client logic compliant with modern-web-guidance (accessibility, html,
// dark-mode-toggle, navigation-drawer, copy-button, scrollspy, tabs, accordion,
// cross-page full-site search, and live gap serve hooks).
"use strict";

(function () {
  const D = window.GAP_DATA || null;
  const pageLang = document.body?.dataset.lang === "vi" ? "vi" : "en";
  const webRoot = document.body?.dataset.webRoot || "";

  const UI_CHROME = {
    en: {
      copied: "Copied!",
      copyFailed: "Copy failed",
      searchEmpty: "No matching guide or section found.",
      githubPagesStatus: "GitHub Pages (Static Docs)",
      cliOfflineHint: "Run python3 web/serve.py locally to inspect live CLI output.",
      statusNeedLocal: "Start local server with: python3 web/serve.py",
      qbStats: (calls) => `1 GAP request · vs. ${calls} Direct call${calls > 1 ? "s" : ""}`,
      chalEmpty: "Type an answer or click a hint chip first.",
      chalCorrect: "Correct! ",
      chalWrong: "Not quite — check the hint chips or lesson snippet and try again.",
      pracCorrect: "Correct! ",
      pracWrong: "Not quite. ",
      markComplete: "Mark Lesson Complete",
      completedLabel: "Completed ✓",
      courseUnit: "lessons",
      courseCompletedSuffix: "completed",
    },
    vi: {
      copied: "Đã sao chép!",
      copyFailed: "Lỗi sao chép",
      searchEmpty: "Không tìm thấy hướng dẫn hoặc mục phù hợp.",
      githubPagesStatus: "GitHub Pages (Chế độ Tĩnh)",
      cliOfflineHint: "Chạy python3 web/serve.py tại máy để xem trợ giúp CLI sống.",
      statusNeedLocal: "Khởi động máy chủ cục bộ: python3 web/serve.py",
      qbStats: (calls) => `1 request GAP · thay vì ${calls} lượt Direct`,
      chalEmpty: "Hãy nhập đáp án hoặc bấm vào gợi ý trước khi kiểm tra.",
      chalCorrect: "Chính xác! ",
      chalWrong: "Chưa đúng — hãy xem lại gợi ý hoặc đoạn mã trong bài và thử lại.",
      pracCorrect: "Chính xác! ",
      pracWrong: "Chưa đúng. ",
      markComplete: "Đánh dấu Hoàn thành Bài học",
      completedLabel: "Đã hoàn thành ✓",
      courseUnit: "bài",
      courseCompletedSuffix: "hoàn thành",
    },
  }[pageLang];

  const esc = (s) =>
    String(s)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");

  // ------------------------------------------------------------------
  // 1. Dark mode toggle (two-state override persisting only on diverge)
  // ------------------------------------------------------------------
  const metaScheme = document.querySelector('meta[name="color-scheme"]');
  const osDarkQuery = window.matchMedia?.("(prefers-color-scheme: dark)");
  const themeBtn = document.getElementById("theme-toggle-btn");

  function currentSystemScheme() {
    return osDarkQuery?.matches ? "dark" : "light";
  }

  function applyStoredScheme(val) {
    if (!metaScheme) return;
    if (val === "light" || val === "dark") {
      metaScheme.content = val;
    } else {
      metaScheme.content = "light dark";
    }
  }

  function currentRenderedScheme() {
    if (metaScheme && (metaScheme.content === "light" || metaScheme.content === "dark")) {
      return metaScheme.content;
    }
    return currentSystemScheme();
  }

  if (themeBtn && metaScheme) {
    themeBtn.addEventListener("click", () => {
      const target = currentRenderedScheme() === "dark" ? "light" : "dark";
      try {
        if (target === currentSystemScheme()) {
          localStorage.removeItem("gap-color-scheme");
          applyStoredScheme(null);
        } else {
          localStorage.setItem("gap-color-scheme", target);
          applyStoredScheme(target);
        }
      } catch (_e) {
        applyStoredScheme(target);
      }
    });
  }

  window.addEventListener("storage", (ev) => {
    if (ev.key === "gap-color-scheme") {
      applyStoredScheme(ev.newValue);
    }
  });

  // Do NOT delete localStorage override inside matchMedia change listener (dark-mode-toggle rule).
  osDarkQuery?.addEventListener?.("change", () => {});

  // ------------------------------------------------------------------
  // 2. Responsive Navigation Drawer (<dialog class="Drawer">)
  // ------------------------------------------------------------------
  const drawer = document.getElementById("docs-drawer");
  const drawerOpenBtn = document.getElementById("drawer-open");
  const drawerCloseBtn = document.getElementById("drawer-close");
  const drawerBackdropBtn = document.getElementById("drawer-backdrop-close");

  if (drawer) {
    const closeDrawer = () => {
      if (drawer.open) drawer.close();
    };
    drawerOpenBtn?.addEventListener("click", () => {
      if (!drawer.open) drawer.showModal();
      drawer.scrollLeft = 0;
    });
    drawerCloseBtn?.addEventListener("click", closeDrawer);
    drawerBackdropBtn?.addEventListener("click", closeDrawer);
    for (const navLink of drawer.querySelectorAll("a[href]")) {
      navLink.addEventListener("click", closeDrawer);
    }
    if ("IntersectionObserver" in window && drawerBackdropBtn) {
      const drawerObs = new IntersectionObserver(
        (entries) => {
          for (const entry of entries) {
            if (entry.isIntersecting && drawer.open && drawer.scrollLeft > 20) {
              drawer.close();
            }
          }
        },
        { root: drawer, threshold: 0.99 }
      );
      drawerObs.observe(drawerBackdropBtn);
    }
  }

  // ------------------------------------------------------------------
  // 3. Copy Buttons (`command="--copy"` + `commandfor`, single-path fallback)
  // ------------------------------------------------------------------
  const copyTimers = new WeakMap();
  let lastCopyMessage = "";

  function legacyCopy(text) {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.cssText = "position:fixed;inset-inline-start:-9999px;opacity:0;";
    document.body.appendChild(ta);
    ta.select();
    let ok = false;
    try {
      ok = document.execCommand("copy");
    } catch (_e) {
      ok = false;
    }
    ta.remove();
    return ok;
  }

  async function writeClipboard(text) {
    if (navigator.clipboard?.writeText) {
      try {
        await navigator.clipboard.writeText(text);
        return true;
      } catch (_e) {
        return legacyCopy(text);
      }
    }
    return legacyCopy(text);
  }

  function showCopyFeedback(btn, ok) {
    const wrap = btn.closest(".copy");
    const tip = wrap?.querySelector('.tip[role="status"]');
    let msg = ok ? UI_CHROME.copied : UI_CHROME.copyFailed;
    if (msg === lastCopyMessage) msg += "\u00A0";
    lastCopyMessage = msg;

    delete btn.dataset.copied;
    delete btn.dataset.failed;
    btn.dataset[ok ? "copied" : "failed"] = "";
    if (tip) {
      tip.textContent = "";
      const setTip = () => {
        tip.textContent = msg;
        tip.dataset.visible = "true";
      };
      if (typeof requestAnimationFrame === "function") {
        requestAnimationFrame(setTip);
      } else {
        setTip();
      }
    }
    const prevTimer = copyTimers.get(btn);
    if (prevTimer) clearTimeout(prevTimer);
    copyTimers.set(
      btn,
      setTimeout(() => {
        delete btn.dataset.copied;
        delete btn.dataset.failed;
        if (tip) {
          tip.dataset.visible = "false";
          tip.textContent = "";
        }
      }, 2000)
    );
  }

  async function handleCopyCommand(btn, target) {
    if (!btn || !target) return;
    const text = target.matches("input, textarea")
      ? target.value
      : (target.innerText ?? target.textContent ?? "").trim();
    const ok = text ? await writeClipboard(text) : false;
    showCopyFeedback(btn, ok);
  }

  document.addEventListener(
    "command",
    (event) => {
      if (!event.command?.startsWith("--copy") || !(event.source instanceof HTMLButtonElement)) {
        return;
      }
      handleCopyCommand(event.source, event.target);
    },
    { capture: true }
  );

  // Register ONLY ONE handler path cleanly per modern-web-guidance: copy-button
  if (!("CommandEvent" in window)) {
    document.addEventListener("click", (ev) => {
      const btn = ev.target.closest('button[command^="--copy"][commandfor]');
      if (!btn) return;
      const target = document.getElementById(btn.getAttribute("commandfor") || "");
      if (target) handleCopyCommand(btn, target);
    });
  }

  // ------------------------------------------------------------------
  // 4. Right-Side Scrollspy ("On this page")
  // ------------------------------------------------------------------
  const tocLinks = Array.from(document.querySelectorAll("#on-this-page-nav a"));
  const sections = Array.from(document.querySelectorAll("section.doc-section[id]"));

  if (tocLinks.length && sections.length) {
    if (typeof CSS !== "undefined" && CSS.supports?.("scroll-target-group: auto")) {
      const syncAriaCurrent = () => {
        const current = document.querySelector("#on-this-page-nav a:target-current");
        for (const link of tocLinks) {
          link.setAttribute("aria-current", link === current ? "true" : "false");
        }
      };
      syncAriaCurrent();
      document.addEventListener("scrollend", syncAriaCurrent);
    } else if ("IntersectionObserver" in window) {
      const setActiveLink = (id) => {
        for (const link of tocLinks) {
          const active = link.getAttribute("href") === `#${id}`;
          link.classList.toggle("target-current", active);
          link.setAttribute("aria-current", active ? "true" : "false");
        }
      };
      const obs = new IntersectionObserver(
        (entries) => {
          for (const entry of entries) {
            if (entry.isIntersecting) {
              setActiveLink(entry.target.id);
              break;
            }
          }
        },
        { rootMargin: "-25% 0px -65% 0px", threshold: 0 }
      );
      sections.forEach((s) => obs.observe(s));
      setActiveLink(sections[0].id);
    }
  }

  // ------------------------------------------------------------------
  // 5. Full-Site Cross-Page Instant Search (`/` or Cmd/Ctrl+K)
  // ------------------------------------------------------------------
  const searchInput = document.getElementById("global-search");
  const searchResults = document.getElementById("search-results");
  const searchIndex = Array.isArray(window.GAP_SEARCH_INDEX) ? window.GAP_SEARCH_INDEX : [];

  function renderSearchHits() {
    if (!searchInput || !searchResults) return;
    const raw = searchInput.value.trim().toLowerCase();
    if (!raw) {
      searchResults.hidden = true;
      searchResults.innerHTML = "";
      return;
    }
    const tokens = raw.split(/\s+/).filter(Boolean);
    const hits = searchIndex
      .filter((entry) => {
        const hay = `${entry.title} ${entry.section} ${entry.keywords || ""}`.toLowerCase();
        return tokens.every((t) => hay.includes(t));
      })
      .slice(0, 10);

    if (!hits.length) {
      searchResults.innerHTML = `<p class="search-empty">${esc(UI_CHROME.searchEmpty)}</p>`;
    } else {
      searchResults.innerHTML = hits
        .map(
          (h) => `
          <a class="search-hit" href="${esc(webRoot + h.href)}">
            <span class="search-hit-sec">${esc(h.section)}</span>
            <strong>${esc(h.title)}</strong>
            ${h.snippet ? `<span class="search-hit-snip">${esc(h.snippet)}</span>` : ""}
          </a>`
        )
        .join("");
    }
    searchResults.hidden = false;
  }

  if (searchInput && searchResults) {
    searchInput.addEventListener("input", renderSearchHits);
    searchInput.addEventListener("focus", () => {
      if (searchInput.value.trim()) renderSearchHits();
    });
    searchResults.addEventListener("click", (ev) => {
      if (ev.target.closest("a.search-hit")) {
        searchResults.hidden = true;
      }
    });
    document.addEventListener("click", (ev) => {
      if (!ev.target.closest(".search-wrap")) {
        searchResults.hidden = true;
      }
    });
    document.addEventListener("keydown", (ev) => {
      const inEditable =
        document.activeElement &&
        /INPUT|TEXTAREA|SELECT/.test(document.activeElement.tagName);
      if ((ev.key === "/" && !inEditable) || ((ev.metaKey || ev.ctrlKey) && ev.key.toLowerCase() === "k")) {
        ev.preventDefault();
        searchInput.focus();
        searchInput.select();
      } else if (ev.key === "Escape") {
        searchResults.hidden = true;
      } else if (
        ev.key === "ArrowDown" &&
        !searchResults.hidden &&
        (document.activeElement === searchInput || searchResults.contains(document.activeElement))
      ) {
        ev.preventDefault();
        const links = Array.from(searchResults.querySelectorAll("a.search-hit"));
        const cur = links.indexOf(document.activeElement);
        links[(cur + 1) % Math.max(links.length, 1)]?.focus();
      } else if (
        ev.key === "ArrowUp" &&
        !searchResults.hidden &&
        searchResults.contains(document.activeElement)
      ) {
        ev.preventDefault();
        const links = Array.from(searchResults.querySelectorAll("a.search-hit"));
        const cur = links.indexOf(document.activeElement);
        if (cur <= 0) searchInput.focus();
        else links[cur - 1]?.focus();
      }
    });
  }

  // ------------------------------------------------------------------
  // 6. Local `web/serve.py` Status Probe & Live Lab Controller
  // ------------------------------------------------------------------
  const statusPillDot = document.getElementById("server-status-dot");
  const statusPillText = document.getElementById("server-status-text");
  const btnToggleDemo = document.getElementById("btn-toggle-demo");
  const demoBadge = document.getElementById("demo-badge");
  const playSelect = document.getElementById("play-scenario");
  const playNote = document.getElementById("play-note");
  const playEndpoint = document.getElementById("play-endpoint");
  const playRequest = document.getElementById("play-request");
  const playResponse = document.getElementById("play-response");
  const playMeta = document.getElementById("play-meta");
  const btnRunPlay = document.getElementById("btn-run-play");

  let serverAvailable = false;
  let demoRunning = false;

  const isLocalHost =
    window.location.hostname === "127.0.0.1" || window.location.hostname === "localhost";

  function updateScenarioView() {
    if (!D || !playSelect) return;
    const sc = D.playgroundScenarios.find((s) => s.id === playSelect.value) || D.playgroundScenarios[0];
    if (!sc) return;
    if (playNote) playNote.textContent = sc.note;
    if (playEndpoint) playEndpoint.textContent = sc.endpoint;
    if (playRequest) playRequest.textContent = JSON.stringify(sc.requestJson, null, 2);
    if (playResponse) playResponse.textContent = JSON.stringify(sc.simulatedResponse, null, 2);
    if (playMeta) {
      playMeta.textContent =
        demoRunning && sc.liveSupported ? D.ui.playMetaReadyLive : D.ui.playMetaSimulated;
    }
  }

  async function pollServerStatus() {
    if (!isLocalHost) {
      if (statusPillText && window.location.hostname.endsWith(".github.io")) {
        statusPillText.textContent = D ? D.ui.statusGithubPages : UI_CHROME.githubPagesStatus;
      }
      return;
    }
    try {
      const resp = await fetch("/api/status");
      if (!resp.ok) return;
      const st = await resp.json();
      serverAvailable = true;
      demoRunning = Boolean(st.demo_running);
      statusPillDot?.classList.toggle("live", demoRunning);
      if (statusPillText) {
        if (D) {
          statusPillText.textContent = demoRunning
            ? `${D.ui.statusLiveRunningPrefix}${st.gap_port})`
            : st.binary_found
              ? D.ui.statusWebReadyIdle
              : D.ui.statusWebNoBin;
        } else {
          statusPillText.textContent = demoRunning
            ? `gap serve :${st.gap_port}`
            : st.binary_found
              ? "serve.py Ready"
              : "Static / serve.py";
        }
      }
      if (demoBadge && D) {
        demoBadge.textContent = demoRunning
          ? `${D.ui.demoBadgeRunningPrefix}${st.gap_port}`
          : D.ui.demoBadgeSimulated;
      }
      if (btnToggleDemo && D) {
        btnToggleDemo.textContent = demoRunning ? D.ui.btnStopLab : D.ui.btnStartLab;
      }
      updateScenarioView();
    } catch (_e) {}
  }
  pollServerStatus();

  if (D && playSelect) {
    D.playgroundScenarios.forEach((sc) => {
      const opt = document.createElement("option");
      opt.value = sc.id;
      opt.textContent = sc.label;
      playSelect.appendChild(opt);
    });
    playSelect.addEventListener("change", updateScenarioView);
    updateScenarioView();
  }

  if (btnToggleDemo) {
    btnToggleDemo.addEventListener("click", async () => {
      if (!serverAvailable) {
        if (playMeta) playMeta.textContent = D ? D.ui.playNeedServePy : UI_CHROME.statusNeedLocal;
        return;
      }
      btnToggleDemo.disabled = true;
      try {
        const endpoint = demoRunning ? "/api/stop-demo" : "/api/start-demo";
        const resp = await fetch(endpoint, { method: "POST" });
        const data = await resp.json();
        if (!resp.ok && playMeta && D) {
          playMeta.textContent = `${D.ui.playErrorPrefix}${data.error || resp.status}`;
        }
        await pollServerStatus();
      } finally {
        btnToggleDemo.disabled = false;
      }
    });
  }

  if (D && btnRunPlay && playSelect) {
    btnRunPlay.addEventListener("click", async () => {
      const sc = D.playgroundScenarios.find((s) => s.id === playSelect.value) || D.playgroundScenarios[0];
      if (demoRunning && sc.liveSupported && serverAvailable) {
        btnRunPlay.disabled = true;
        try {
          const resp = await fetch("/api/playground", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ scenario_id: sc.id }),
          });
          const out = await resp.json();
          if (playMeta) {
            playMeta.textContent = `${D.ui.playLivePrefix} · HTTP ${out.http_status} · ${out.latency_ms} ms`;
          }
          if (playResponse) playResponse.textContent = JSON.stringify(out.body, null, 2);
        } catch (err) {
          if (playMeta) playMeta.textContent = `${D.ui.playLiveErrorPrefix}${err}`;
        } finally {
          btnRunPlay.disabled = false;
        }
      } else {
        if (playMeta) playMeta.textContent = D.ui.playSimulatedNote;
        if (playResponse) playResponse.textContent = JSON.stringify(sc.simulatedResponse, null, 2);
      }
    });
  }

  // ------------------------------------------------------------------
  // 6.5 Learn Interactive Query Builder, Challenges & GAP Academy Progress
  // ------------------------------------------------------------------
  const qbWidget = document.getElementById("query-builder-widget");
  if (qbWidget) {
    const qbName = document.getElementById("qb-name");
    const qbTier = document.getElementById("qb-tier");
    const qbOrders = document.getElementById("qb-orders");
    const qbNotes = document.getElementById("qb-notes");
    const qbGql = document.getElementById("qb-graphql-code");
    const qbJson = document.getElementById("qb-json-code");
    const qbStats = document.getElementById("qb-stats-pill");

    function updateQueryBuilder() {
      const custFields = ["id"];
      const custObj = { id: 1 };
      if (qbName?.checked) {
        custFields.push("name");
        custObj.name = "Ada Lovelace";
      }
      if (qbTier?.checked) {
        custFields.push("tier");
        custObj.tier = "vip";
      }

      const blocks = [
        `  customer(id: 1) {\n${custFields.map((f) => `    ${f}`).join("\n")}\n  }`,
      ];
      const dataObj = { customer: custObj };
      let directCalls = 1;

      if (qbOrders?.checked) {
        blocks.push("  orders(customer_id: 1) {\n    id\n    status\n    total_cents\n  }");
        dataObj.orders = [
          { id: 101, status: "paid", total_cents: 4200 },
          { id: 103, status: "pending", total_cents: 8500 },
        ];
        directCalls += 1;
      }

      if (qbNotes?.checked) {
        blocks.push("  customer_notes(customer_id: 1)");
        dataObj.customer_notes = {
          customer_id: 1,
          notes: [
            {
              note_id: "n-1",
              summary: "Prefers consolidated monthly invoice",
            },
          ],
        };
        directCalls += 1;
      }

      if (qbGql) qbGql.textContent = `query {\n${blocks.join("\n")}\n}`;
      if (qbJson) qbJson.textContent = JSON.stringify({ data: dataObj }, null, 2);
      if (qbStats) qbStats.textContent = UI_CHROME.qbStats(directCalls);
    }

    [qbName, qbTier, qbOrders, qbNotes].forEach((el) =>
      el?.addEventListener("change", updateQueryBuilder)
    );
    updateQueryBuilder();
  }

  for (const box of document.querySelectorAll(".challenge-box")) {
    const input = box.querySelector(".chal-input");
    const btn = box.querySelector(".chal-check-btn");
    const feedback = box.querySelector(".chal-feedback");
    const expectedList = (box.dataset.answers || "")
      .split("|")
      .map((s) => s.trim().toLowerCase())
      .filter(Boolean);
    const explain = box.dataset.explain || "";

    function evaluateChallenge() {
      if (!input || !feedback) return;
      const val = input.value.trim().toLowerCase();
      if (!val) {
        feedback.className = "chal-feedback is-err";
        feedback.textContent = UI_CHROME.chalEmpty;
        return;
      }
      if (expectedList.includes(val)) {
        feedback.className = "chal-feedback is-ok";
        feedback.textContent = UI_CHROME.chalCorrect + explain;
      } else {
        feedback.className = "chal-feedback is-err";
        feedback.textContent = UI_CHROME.chalWrong;
      }
    }

    btn?.addEventListener("click", evaluateChallenge);
    input?.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter") {
        ev.preventDefault();
        evaluateChallenge();
      }
    });
    for (const chip of box.querySelectorAll(".chal-hint-chip")) {
      chip.addEventListener("click", () => {
        if (input) {
          input.value = chip.dataset.fill || "";
          input.focus();
        }
      });
    }
  }

  for (const pbox of document.querySelectorAll(".practice-box")) {
    const correctIdx = Number(pbox.dataset.correct);
    const explain = pbox.dataset.explain || "";
    const feedback = pbox.querySelector(".practice-feedback");
    const opts = Array.from(pbox.querySelectorAll(".practice-opt"));
    opts.forEach((btn) => {
      btn.addEventListener("click", () => {
        const chosen = Number(btn.dataset.idx);
        opts.forEach((o) => o.classList.remove("correct", "wrong"));
        const isOk = chosen === correctIdx;
        btn.classList.add(isOk ? "correct" : "wrong");
        if (feedback) {
          feedback.className = `practice-feedback ${isOk ? "is-ok" : "is-err"}`;
          feedback.textContent = (isOk ? UI_CHROME.pracCorrect : UI_CHROME.pracWrong) + explain;
        }
      });
    });
  }

  const ACADEMY_COURSES = {
    liftoff: ["liftoff-1", "liftoff-2", "liftoff-3", "liftoff-4"],
    mcp: ["mcp-1", "mcp-2", "mcp-3", "mcp-4"],
    batch: ["batch-1", "batch-2", "batch-3", "batch-4"],
    sec: ["sec-1", "sec-2", "sec-3", "sec-4"],
  };
  const ALL_ACADEMY_LESSONS = Object.values(ACADEMY_COURSES).flat();

  function loadAcademyProgress() {
    try {
      const parsed = JSON.parse(localStorage.getItem("gap-academy-progress") || "{}");
      return parsed && typeof parsed === "object" ? parsed : {};
    } catch (_e) {
      return {};
    }
  }

  function saveAcademyProgress(map) {
    try {
      localStorage.setItem("gap-academy-progress", JSON.stringify(map));
    } catch (_e) {}
  }

  function refreshAcademyUI() {
    const map = loadAcademyProgress();
    for (const btn of document.querySelectorAll(".lesson-complete-btn[data-lesson-key]")) {
      const key = btn.dataset.lessonKey;
      const done = Boolean(map[key]);
      btn.setAttribute("aria-pressed", done ? "true" : "false");
      btn.textContent = done ? UI_CHROME.completedLabel : UI_CHROME.markComplete;
    }
    for (const pill of document.querySelectorAll(".lesson-step-pill[data-lesson-pill]")) {
      pill.classList.toggle("is-done", Boolean(map[pill.dataset.lessonPill]));
    }
    for (const badge of document.querySelectorAll("[data-course-progress]")) {
      const cid = badge.dataset.courseProgress;
      const keys = ACADEMY_COURSES[cid] || [];
      const doneCount = keys.filter((k) => Boolean(map[k])).length;
      const inBanner = Boolean(badge.closest(".course-banner"));
      badge.textContent = inBanner
        ? `${doneCount} / ${keys.length} ${UI_CHROME.courseCompletedSuffix}`
        : `${doneCount} / ${keys.length} ${UI_CHROME.courseUnit}`;
    }
    const totalDone = ALL_ACADEMY_LESSONS.filter((k) => Boolean(map[k])).length;
    const totalCountEl = document.getElementById("academy-total-count");
    const totalFillEl = document.getElementById("academy-total-fill");
    if (totalCountEl) totalCountEl.textContent = `${totalDone} / ${ALL_ACADEMY_LESSONS.length}`;
    if (totalFillEl) {
      const pct = ALL_ACADEMY_LESSONS.length
        ? Math.round((totalDone * 100) / ALL_ACADEMY_LESSONS.length)
        : 0;
      totalFillEl.style.inlineSize = `${pct}%`;
    }
  }

  for (const btn of document.querySelectorAll(".lesson-complete-btn[data-lesson-key]")) {
    btn.addEventListener("click", () => {
      const map = loadAcademyProgress();
      const key = btn.dataset.lessonKey;
      if (map[key]) delete map[key];
      else map[key] = true;
      saveAcademyProgress(map);
      refreshAcademyUI();
    });
  }

  const academyResetBtn = document.getElementById("academy-reset-btn");
  if (academyResetBtn) {
    academyResetBtn.addEventListener("click", () => {
      saveAcademyProgress({});
      refreshAcademyUI();
    });
  }
  refreshAcademyUI();

  // Stop here cleanly on non-widget documentation pages when GAP_DATA is absent.
  if (!D) return;

  // ------------------------------------------------------------------
  // 7. Interactive Three-Plane Architecture (`concepts/architecture.html`)
  // ------------------------------------------------------------------
  const planesGrid = document.getElementById("planes-grid");
  const archInspector = document.getElementById("arch-inspector");
  if (planesGrid && archInspector && Array.isArray(D.architectureNodes) && D.architectureNodes.length) {
    const planeNames = Array.from(new Set(D.architectureNodes.map((n) => n.plane)));
    let selectedNodeId = D.architectureNodes[0].id;

    function renderInspector(nodeId) {
      selectedNodeId = nodeId;
      const node = D.architectureNodes.find((n) => n.id === nodeId) || D.architectureNodes[0];
      for (const btn of planesGrid.querySelectorAll(".node-btn")) {
        btn.setAttribute("aria-pressed", btn.dataset.id === node.id ? "true" : "false");
      }
      archInspector.innerHTML = `
        <div style="display:flex;flex-wrap:wrap;justify-content:space-between;gap:0.5rem;margin-bottom:0.45rem;">
          <strong>${esc(node.title)}</strong>
          <span class="pill">${esc(node.latency)}</span>
        </div>
        <p style="margin:0.35rem 0;">${esc(node.summary)}</p>
        <p style="margin:0.35rem 0;color:var(--muted);">${esc(node.details)}</p>
        <div style="font-size:0.84rem;margin-top:0.5rem;">
          <strong>${esc(D.ui.cratesLabel)}:</strong> <code>${esc(node.crates)}</code> ·
          <strong>${esc(D.ui.invariantsLabel)}:</strong> <code>${esc(node.invariants)}</code> ·
          <strong>${esc(D.ui.sourceFilesLabel)}:</strong> ${node.files.map((f) => `<code>${esc(f)}</code>`).join(" ")}
        </div>
      `;
    }

    for (const pName of planeNames) {
      const box = document.createElement("div");
      box.className = "plane-box";
      const h3 = document.createElement("h3");
      h3.textContent = pName;
      const list = document.createElement("div");
      list.className = "node-list";
      for (const node of D.architectureNodes.filter((x) => x.plane === pName)) {
        const btn = document.createElement("button");
        btn.type = "button";
        btn.className = "node-btn";
        btn.dataset.id = node.id;
        btn.textContent = node.title;
        btn.addEventListener("click", () => renderInspector(node.id));
        list.appendChild(btn);
      }
      box.append(h3, list);
      planesGrid.appendChild(box);
    }
    renderInspector(selectedNodeId);
  }

  // ------------------------------------------------------------------
  // 8. 6-Level Tabs + LocalStorage Progress (`lab/playground.html`)
  // ------------------------------------------------------------------
  const tablist = document.getElementById("levels-tablist");
  const tabpanels = document.getElementById("levels-tabpanels");
  const progressText = document.getElementById("progress-text");
  const progressFill = document.getElementById("progress-fill");
  const progressWrap = document.getElementById("progress-bar-wrap");

  if (tablist && tabpanels && Array.isArray(D.levels) && D.levels.length) {
    function loadProgress() {
      try {
        return JSON.parse(localStorage.getItem("gap-learn-progress") || "{}");
      } catch (_e) {
        return {};
      }
    }

    function saveProgress(obj) {
      try {
        localStorage.setItem("gap-learn-progress", JSON.stringify(obj));
      } catch (_e) {}
    }

    const progressMap = loadProgress();
    const totalCheckpoints = D.levels.reduce((acc, lv) => acc + lv.checklist.length, 0);

    function refreshProgressBar() {
      let doneCount = 0;
      for (const lv of D.levels) {
        lv.checklist.forEach((_, idx) => {
          if (progressMap[`${lv.id}:${idx}`]) doneCount += 1;
        });
      }
      if (progressText) progressText.textContent = `${doneCount} / ${totalCheckpoints}`;
      const pct = totalCheckpoints ? Math.round((doneCount * 100) / totalCheckpoints) : 0;
      if (progressFill) progressFill.style.inlineSize = `${pct}%`;
      if (progressWrap) progressWrap.setAttribute("aria-valuenow", String(doneCount));
    }

    D.levels.forEach((lv, idx) => {
      const tabId = `tab-${lv.id}`;
      const selected = idx === 0;
      const tabBtn = document.createElement("button");
      tabBtn.type = "button";
      tabBtn.role = "tab";
      tabBtn.id = tabId;
      tabBtn.textContent = lv.tabLabel;
      tabBtn.setAttribute("aria-selected", selected ? "true" : "false");
      if (selected) tabBtn.setAttribute("focusgroupstart", "");
      tablist.appendChild(tabBtn);

      const panel = document.createElement("div");
      panel.role = "tabpanel";
      panel.id = `panel-${lv.id}`;
      panel.setAttribute("aria-labelledby", tabId);
      if (selected) {
        panel.setAttribute("tabindex", "0");
      } else {
        panel.setAttribute("hidden", "until-found");
      }

      const itemsHtml = lv.checklist
        .map((label, cIdx) => {
          const key = `${lv.id}:${cIdx}`;
          const cid = `chk-${lv.id}-${cIdx}`;
          const checked = progressMap[key] ? "checked" : "";
          return `<div class="check-row"><label for="${esc(cid)}"><input id="${esc(cid)}" type="checkbox" data-key="${esc(key)}" ${checked} /> <span>${esc(label)}</span></label></div>`;
        })
        .join("");

      panel.innerHTML = `
        <div class="tabpanel-card">
          <div style="display:flex;flex-wrap:wrap;justify-content:space-between;gap:0.5rem;">
            <h3 style="margin:0;">${esc(lv.title)}</h3>
            <span class="pill">${esc(lv.badge)}</span>
          </div>
          <p><strong>${esc(D.ui.levelGoalLabel)}:</strong> ${esc(lv.goal)}</p>
          <ul>${lv.concepts.map((c) => `<li>${esc(c)}</li>`).join("")}</ul>
          <div><strong>${esc(D.ui.levelCommandsLabel)}:</strong></div>
          <figure class="code-figure">
            <div class="code-toolbar">
              <div class="code-meta">
                <span class="code-lang">terminal</span>
                <span class="code-title">${esc(D.ui.levelCommandsAria)} ${esc(lv.tabLabel)}</span>
              </div>
              <span class="copy">
                <button type="button" class="copy-button" command="--copy" commandfor="lv-cmd-${esc(lv.id)}" aria-label="${esc(D.ui.levelCommandsAria)} ${esc(lv.tabLabel)}">
                  <span class="icon" aria-hidden="true"></span>
                </button>
                <span class="tip" role="status"></span>
              </span>
            </div>
            <pre role="region" aria-label="${esc(D.ui.levelCommandsAria)} ${esc(lv.tabLabel)}" tabindex="0"><code id="lv-cmd-${esc(lv.id)}" class="language-terminal">${esc(lv.commands)}</code></pre>
          </figure>
          <div style="font-size:0.86rem;"><strong>${esc(D.ui.levelFilesLabel)}:</strong> ${lv.files.map((f) => `<code>${esc(f)}</code>`).join(" ")}</div>
          <fieldset class="step-checklist">
            <legend style="font-weight:700;margin:0.9rem 0 0.35rem 0;">${esc(D.ui.levelChecklistHeading)}:</legend>
            ${itemsHtml}
          </fieldset>
        </div>
      `;
      tabpanels.appendChild(panel);
    });

    const allTabs = Array.from(tablist.querySelectorAll('[role="tab"]'));
    const allPanels = Array.from(tabpanels.querySelectorAll('[role="tabpanel"]'));

    function selectLevelTab(targetTab) {
      allTabs.forEach((tab, i) => {
        const isTarget = tab === targetTab;
        tab.setAttribute("aria-selected", isTarget ? "true" : "false");
        tab.toggleAttribute("focusgroupstart", isTarget);
        const panel = allPanels[i];
        if (isTarget) {
          panel.removeAttribute("hidden");
          panel.setAttribute("tabindex", "0");
        } else {
          panel.setAttribute("hidden", "until-found");
          panel.removeAttribute("tabindex");
        }
      });
      targetTab.scrollIntoView?.({ block: "nearest", inline: "nearest", container: "nearest" });
    }

    tablist.addEventListener("click", (ev) => {
      const btn = ev.target.closest('[role="tab"]');
      if (btn) selectLevelTab(btn);
    });

    if (!Object.hasOwn(HTMLElement.prototype, "focusGroup")) {
      tablist.addEventListener("keydown", (ev) => {
        const current = ev.target.closest('[role="tab"]');
        if (!current) return;
        const idx = allTabs.indexOf(current);
        let nextIdx = -1;
        if (ev.key === "ArrowRight") nextIdx = (idx + 1) % allTabs.length;
        else if (ev.key === "ArrowLeft") nextIdx = (idx - 1 + allTabs.length) % allTabs.length;
        else if (ev.key === "Home") nextIdx = 0;
        else if (ev.key === "End") nextIdx = allTabs.length - 1;
        if (nextIdx >= 0) {
          ev.preventDefault();
          allTabs[nextIdx].focus();
          selectLevelTab(allTabs[nextIdx]);
        }
      });
    }

    allPanels.forEach((panel, i) => {
      panel.addEventListener("beforematch", () => {
        selectLevelTab(allTabs[i]);
      });
    });

    tabpanels.addEventListener("change", (ev) => {
      const cb = ev.target.closest('input[type="checkbox"][data-key]');
      if (!cb) return;
      progressMap[cb.dataset.key] = cb.checked;
      saveProgress(progressMap);
      refreshProgressBar();
    });
    refreshProgressBar();
  }

  // ------------------------------------------------------------------
  // 9. Crossover Calculator (`lab/playground.html`)
  // ------------------------------------------------------------------
  const calcSuite = document.getElementById("calc-suite");
  const calcN = document.getElementById("calc-n");
  const calcCached = document.getElementById("calc-cached");
  const calcOutput = document.getElementById("calc-output");

  if (calcSuite && calcN && calcCached && calcOutput && D.gapbench) {
    function renderCrossover() {
      const suite = calcSuite.value;
      if (suite === "code_graph") {
        calcN.value = "1000";
        calcN.disabled = true;
      } else {
        calcN.disabled = false;
      }
      const row =
        suite === "code_graph" ? D.gapbench.code_graph[24] : D.gapbench[suite][Number(calcN.value)];
      const cached = calcCached.checked;
      const directTok = cached ? row.dCac : row.dUnc;
      const gapTok = cached ? row.gCac : row.gUnc;
      const maxTok = Math.max(directTok, gapTok, 1);
      const pct = (val) =>
        val <= 0 ? 0 : Math.max(2.5, Number(((val * 100) / maxTok).toFixed(1)));
      const ratio = gapTok / directTok;
      const gapWins = ratio < 1.0;
      const pctSaved = ((1 - ratio) * 100).toFixed(1);

      const verdict = gapWins
        ? `<strong style="color:var(--ok);">${esc(D.ui.gapWinsPrefix)} (${(1 / ratio).toFixed(2)}x ${esc(D.ui.gapWinsMiddle)} ${pctSaved}%)</strong> — ${esc(D.ui.gapWinsSuffix)}`
        : `<strong style="color:var(--warn);">${esc(D.ui.directWinsPrefix)} (${ratio.toFixed(2)}x)</strong> — ${esc(D.ui.directWinsSuffix)}`;

      calcOutput.innerHTML = `
        <div class="bar-row">
          <span>Direct-MCP:</span>
          <div class="bar-track"><div class="bar-fill direct" style="inline-size:${pct(directTok)}%"></div></div>
          <strong>${directTok.toLocaleString(D.ui.locale)} tok</strong>
        </div>
        <div class="bar-row">
          <span>GAP Facade:</span>
          <div class="bar-track"><div class="bar-fill" style="inline-size:${pct(gapTok)}%"></div></div>
          <strong>${gapTok.toLocaleString(D.ui.locale)} tok</strong>
        </div>
        <div style="font-size:0.86rem;margin-top:0.55rem;">
          <div><strong>${esc(D.ui.schemaCostLabel)}:</strong> Direct ${row.dTools.toLocaleString(D.ui.locale)} vs GAP <strong>${row.gTools} tokens</strong> · <strong>${esc(D.ui.turnsLabel)}:</strong> Direct ${row.dTurns} vs GAP <strong>${row.gTurns}</strong></div>
          <div style="margin-top:0.35rem;">${verdict}</div>
        </div>
      `;
    }

    [calcSuite, calcN, calcCached].forEach((el) => el.addEventListener("change", renderCrossover));
    renderCrossover();
  }

  // ------------------------------------------------------------------
  // 10. 32-Crate Grid Filter (`reference/cli-and-ops.html`)
  // ------------------------------------------------------------------
  const layerChips = document.getElementById("layer-chips");
  const cratesGrid = document.getElementById("crates-grid");
  if (layerChips && cratesGrid && Array.isArray(D.crates)) {
    const layers = ["ALL", "L0", "L1", "L2", "L3", "L4", "L5", "L6", "Dev"];
    let activeLayer = "ALL";

    function renderCrates() {
      for (const chip of layerChips.querySelectorAll(".chip")) {
        chip.setAttribute("aria-pressed", chip.dataset.layer === activeLayer ? "true" : "false");
      }
      const items = activeLayer === "ALL" ? D.crates : D.crates.filter((c) => c.layer === activeLayer);
      cratesGrid.innerHTML = items
        .map(
          (c) => `
        <div class="crate-card">
          <span class="layer-tag">${esc(c.layer)}</span>
          <div><strong><code>${esc(c.name)}</code></strong></div>
          <div style="color:var(--muted);margin-top:0.2rem;">${esc(c.role)}</div>
        </div>`
        )
        .join("");
    }

    for (const l of layers) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "chip";
      btn.dataset.layer = l;
      btn.textContent = l === "ALL" ? D.ui.allLayersLabel : l;
      btn.addEventListener("click", () => {
        activeLayer = l;
        renderCrates();
      });
      layerChips.appendChild(btn);
    }
    renderCrates();
  }

  // ------------------------------------------------------------------
  // 11. CLI 15 Subcommands & 9 Runbooks Accordion (`reference/cli-and-ops.html`)
  // ------------------------------------------------------------------
  const cliList = document.getElementById("cli-list");
  if (cliList && Array.isArray(D.cliSubcommands)) {
    for (const c of D.cliSubcommands) {
      const det = document.createElement("details");
      det.innerHTML = `
        <summary><code>gap ${esc(c.sub)}</code></summary>
        <div class="details-body">
          <p>${esc(c.summary)}</p>
          <button type="button" class="btn btn-sm cli-help-btn" data-sub="${esc(c.sub)}">${esc(D.ui.cliRunPrefix)} <code>gap ${esc(c.sub)} --help</code></button>
          <pre class="cli-help-out" role="region" aria-label="${esc(D.ui.cliHelpAriaPrefix)} gap ${esc(c.sub)}" tabindex="0" hidden><code></code></pre>
        </div>
      `;
      cliList.appendChild(det);
    }

    cliList.addEventListener("click", async (ev) => {
      const btn = ev.target.closest(".cli-help-btn");
      if (!btn) return;
      const pre = btn.nextElementSibling;
      const code = pre.querySelector("code");
      pre.hidden = false;
      if (!serverAvailable) {
        code.textContent = `$ ./target/dist/gap ${btn.dataset.sub} --help\n(${D.ui.cliOfflineHint || UI_CHROME.cliOfflineHint})`;
        return;
      }
      code.textContent = D.ui.cliLoading;
      try {
        const resp = await fetch(`/api/cli-help?sub=${encodeURIComponent(btn.dataset.sub)}`);
        const res = await resp.json();
        code.textContent = res.output || res.error || D.ui.cliEmpty;
      } catch (e) {
        code.textContent = String(e);
      }
    });
  }

  const runbooksAcc = document.getElementById("runbooks-accordion");
  const toggleAllBtn = document.getElementById("btn-toggle-all-runbooks");
  if (runbooksAcc && toggleAllBtn && Array.isArray(D.runbooks)) {
    for (const rb of D.runbooks) {
      const det = document.createElement("details");
      det.innerHTML = `
        <summary><span>${esc(rb.id)}</span> <span class="pill" style="margin-inline-start:auto;">${esc(rb.severity)}</span></summary>
        <div class="details-body">
          <p><strong>${esc(D.ui.runbookTriggerLabel)}:</strong> ${esc(rb.trigger)}</p>
          <p><strong>${esc(D.ui.runbookActionLabel)}:</strong> ${esc(rb.action)}</p>
        </div>
      `;
      runbooksAcc.appendChild(det);
    }

    function handleToggleAll(expand) {
      for (const details of runbooksAcc.querySelectorAll(":scope > details")) {
        details.open = expand;
      }
    }

    document.addEventListener(
      "command",
      (event) => {
        if (event.command !== "--toggle-all" || event.target !== runbooksAcc) return;
        const expand = event.source.getAttribute("aria-expanded") !== "true";
        handleToggleAll(expand);
      },
      { capture: true }
    );

    if (!Object.hasOwn(HTMLButtonElement.prototype, "commandForElement")) {
      toggleAllBtn.addEventListener("click", () => {
        const expand = toggleAllBtn.getAttribute("aria-expanded") !== "true";
        handleToggleAll(expand);
      });
    }

    runbooksAcc.addEventListener(
      "toggle",
      (event) => {
        if (!event.target.matches("#runbooks-accordion > details")) return;
        const items = runbooksAcc.querySelectorAll(":scope > details");
        const allOpen = Array.prototype.every.call(items, (d) => d.open);
        toggleAllBtn.setAttribute("aria-expanded", String(allOpen));
      },
      { capture: true }
    );
  }

  // ------------------------------------------------------------------
  // 12. Invariants & STE-100 Tables (`reference/invariants-and-glossary.html`)
  // ------------------------------------------------------------------
  const invTbody = document.getElementById("inv-tbody");
  if (invTbody && Array.isArray(D.invariants)) {
    invTbody.innerHTML = D.invariants
      .map(
        (i) => `
        <tr>
          <th scope="row"><code>${esc(i.id)}</code></th>
          <td>${esc(i.req)}</td>
          <td><code>${esc(i.loc)}</code></td>
          <td>${esc(i.status)}</td>
        </tr>`
      )
      .join("");
  }

  const dictTbody = document.getElementById("dict-tbody");
  if (dictTbody && Array.isArray(D.dictionary)) {
    dictTbody.innerHTML = D.dictionary
      .map(
        (d) => `
        <tr>
          <th scope="row"><code>${esc(d.term)}</code></th>
          <td>${esc(d.def)}</td>
          <td><em>${esc(d.forbidden)}</em></td>
        </tr>`
      )
      .join("");
  }

  // ------------------------------------------------------------------
  // 13. Verification Quiz (`lab/playground.html`)
  // ------------------------------------------------------------------
  const quizBox = document.getElementById("quiz-container");
  const quizScore = document.getElementById("quiz-score");
  if (quizBox && quizScore && Array.isArray(D.quiz)) {
    const quizState = {};

    D.quiz.forEach((item, qIdx) => {
      const qEl = document.createElement("div");
      qEl.className = "quiz-q";
      const optsHtml = item.options
        .map(
          (opt, oIdx) =>
            `<button type="button" class="quiz-opt" data-q="${qIdx}" data-o="${oIdx}">${esc(opt)}</button>`
        )
        .join("");
      qEl.innerHTML = `
        <div><strong>${esc(item.q)}</strong></div>
        <div class="quiz-options">${optsHtml}</div>
        <div class="quiz-explain" style="margin-top:0.45rem;font-size:0.86rem;color:var(--muted);"></div>
      `;
      quizBox.appendChild(qEl);
    });

    quizBox.addEventListener("click", (ev) => {
      const btn = ev.target.closest(".quiz-opt");
      if (!btn) return;
      const qIdx = Number(btn.dataset.q);
      const oIdx = Number(btn.dataset.o);
      const item = D.quiz[qIdx];
      const qEl = btn.closest(".quiz-q");
      for (const ob of qEl.querySelectorAll(".quiz-opt")) {
        ob.classList.remove("correct", "wrong");
      }
      const isOk = oIdx === item.correct;
      btn.classList.add(isOk ? "correct" : "wrong");
      quizState[qIdx] = isOk;
      qEl.querySelector(".quiz-explain").textContent = isOk
        ? `${D.ui.quizCorrectPrefix} ${item.explain}`
        : `${D.ui.quizWrongPrefix} ${item.explain}`;
      const totalOk = Object.values(quizState).filter(Boolean).length;
      quizScore.textContent = `${D.ui.quizScorePrefix}: ${totalOk} / ${D.quiz.length}`;
    });
  }
})();
