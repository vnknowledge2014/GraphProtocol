// Client logic tuân thủ modern-web-guidance:
// dark-mode-toggle, tabs, scrollspy, accordion, interactive lab & live gap serve hooks.
"use strict";

(function () {
  const D = window.GAP_DATA;
  if (!D) return;

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
  const osDarkQuery = window.matchMedia("(prefers-color-scheme: dark)");
  const themeBtn = document.getElementById("theme-toggle-btn");

  function currentSystemScheme() {
    return osDarkQuery.matches ? "dark" : "light";
  }

  function applyStoredScheme(val) {
    if (val === "light" || val === "dark") {
      metaScheme.content = val;
    } else {
      metaScheme.content = "light dark";
    }
  }

  function currentRenderedScheme() {
    if (metaScheme.content === "light" || metaScheme.content === "dark") {
      return metaScheme.content;
    }
    return currentSystemScheme();
  }

  if (themeBtn) {
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

  // Note: Do NOT delete localStorage override inside matchMedia change listener (dark-mode-toggle rule).
  osDarkQuery.addEventListener("change", () => {});

  // ------------------------------------------------------------------
  // 2. Scrollspy navigation (native scroll-target-group + IntersectionObserver fallback)
  // ------------------------------------------------------------------
  const tocLinks = Array.from(document.querySelectorAll("#toc-nav a"));
  const sections = Array.from(document.querySelectorAll("main > section.card"));

  if (CSS.supports("scroll-target-group: auto")) {
    const syncAriaCurrent = () => {
      const current = document.querySelector("#toc-nav a:target-current");
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
          }
        }
      },
      { rootMargin: "-35% 0px -55% 0px", threshold: 0 }
    );
    sections.forEach((s) => obs.observe(s));
    if (sections[0]) setActiveLink(sections[0].id);
  }

  // ------------------------------------------------------------------
  // 3. Interactive Three-Plane Architecture
  // ------------------------------------------------------------------
  const planesGrid = document.getElementById("planes-grid");
  const archInspector = document.getElementById("arch-inspector");
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
        <strong>Crates:</strong> <code>${esc(node.crates)}</code> ·
        <strong>Invariants:</strong> <code>${esc(node.invariants)}</code> ·
        <strong>Mã nguồn:</strong> ${node.files.map((f) => `<code>${esc(f)}</code>`).join(" ")}
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

  // ------------------------------------------------------------------
  // 4. 6-Level Tabs + LocalStorage Learning Progress
  // ------------------------------------------------------------------
  const tablist = document.getElementById("levels-tablist");
  const tabpanels = document.getElementById("levels-tabpanels");
  const progressText = document.getElementById("progress-text");
  const progressFill = document.getElementById("progress-fill");
  const progressWrap = document.getElementById("progress-bar-wrap");

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
    progressText.textContent = `${doneCount} / ${totalCheckpoints}`;
    const pct = totalCheckpoints ? Math.round((doneCount * 100) / totalCheckpoints) : 0;
    progressFill.style.inlineSize = `${pct}%`;
    progressWrap.setAttribute("aria-valuenow", String(doneCount));
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
        const checked = progressMap[key] ? "checked" : "";
        return `<li><label><input type="checkbox" data-key="${key}" ${checked} /> <span>${esc(label)}</span></label></li>`;
      })
      .join("");

    panel.innerHTML = `
      <div class="tabpanel-card">
        <div style="display:flex;flex-wrap:wrap;justify-content:space-between;gap:0.5rem;">
          <h3 style="margin:0;">${esc(lv.title)}</h3>
          <span class="pill">${esc(lv.badge)}</span>
        </div>
        <p><strong>Mục tiêu:</strong> ${esc(lv.goal)}</p>
        <ul>${lv.concepts.map((c) => `<li>${esc(c)}</li>`).join("")}</ul>
        <div><strong>Lệnh thực hành:</strong></div>
        <pre><code>${esc(lv.commands)}</code></pre>
        <div style="font-size:0.86rem;"><strong>Tệp nên đọc trong repo:</strong> ${lv.files.map((f) => `<code>${esc(f)}</code>`).join(" ")}</div>
        <h4 style="margin:0.9rem 0 0.2rem 0;">Checklist tự luyện cấp độ này:</h4>
        <ul class="step-checklist">${itemsHtml}</ul>
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
    targetTab.scrollIntoView({ block: "nearest", inline: "nearest", container: "nearest" });
  }

  tablist.addEventListener("click", (ev) => {
    const btn = ev.target.closest('[role="tab"]');
    if (btn) selectLevelTab(btn);
  });

  // Fallback arrow-key navigation when focusgroup is unsupported (Safari/Firefox)
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

  // Sync tab state automatically when browser find-in-page matches hidden="until-found" panel
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

  // ------------------------------------------------------------------
  // 5. Crossover Calculator
  // ------------------------------------------------------------------
  const calcSuite = document.getElementById("calc-suite");
  const calcN = document.getElementById("calc-n");
  const calcCached = document.getElementById("calc-cached");
  const calcOutput = document.getElementById("calc-output");

  function renderCrossover() {
    const suite = calcSuite.value;
    if (suite === "code_graph") {
      calcN.value = "1000";
      calcN.disabled = true;
    } else {
      calcN.disabled = false;
    }
    const row = suite === "code_graph" ? D.gapbench.code_graph[24] : D.gapbench[suite][Number(calcN.value)];
    const cached = calcCached.checked;
    const directTok = cached ? row.dCac : row.dUnc;
    const gapTok = cached ? row.gCac : row.gUnc;
    const maxTok = Math.max(directTok, gapTok, 1);
    const ratio = gapTok / directTok;
    const gapWins = ratio < 1.0;
    const pctSaved = ((1 - ratio) * 100).toFixed(1);

    let verdict = "";
    if (gapWins) {
      verdict = `<strong style="color:var(--ok);">GAP thắng (${(1 / ratio).toFixed(2)}x ít token hơn · tiết kiệm ${pctSaved}%)</strong> — nên dùng GAP Facade ngay.`;
    } else {
      verdict = `<strong style="color:var(--warn);">Direct-MCP tốn ít token hơn (${ratio.toFixed(2)}x)</strong> — ở cấu hình nhỏ bật Prompt Cache này, chỉ nên bật GAP vì lý do bảo mật (Cedar, chống rug-pull, SSRF guard).`;
    }

    calcOutput.innerHTML = `
      <div class="bar-row">
        <span>Direct-MCP:</span>
        <div class="bar-track"><div class="bar-fill direct" style="inline-size:${Math.round((directTok * 100) / maxTok)}%"></div></div>
        <strong>${directTok.toLocaleString("vi-VN")} tok</strong>
      </div>
      <div class="bar-row">
        <span>GAP Facade:</span>
        <div class="bar-track"><div class="bar-fill" style="inline-size:${Math.round((gapTok * 100) / maxTok)}%"></div></div>
        <strong>${gapTok.toLocaleString("vi-VN")} tok</strong>
      </div>
      <div style="font-size:0.86rem;margin-top:0.55rem;">
        <div><strong>Schema <code>tools/list</code>:</strong> Direct ${row.dTools.toLocaleString("vi-VN")} vs GAP <strong>${row.gTools} tokens</strong> · <strong>Số lượt LLM:</strong> Direct ${row.dTurns} vs GAP <strong>${row.gTurns}</strong></div>
        <div style="margin-top:0.35rem;">${verdict}</div>
      </div>
    `;
  }

  [calcSuite, calcN, calcCached].forEach((el) => el.addEventListener("change", renderCrossover));
  renderCrossover();

  // ------------------------------------------------------------------
  // 6. Playground + Live Lab Server Connection
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

  D.playgroundScenarios.forEach((sc) => {
    const opt = document.createElement("option");
    opt.value = sc.id;
    opt.textContent = sc.label;
    playSelect.appendChild(opt);
  });

  function updateScenarioView() {
    const sc = D.playgroundScenarios.find((s) => s.id === playSelect.value) || D.playgroundScenarios[0];
    playNote.textContent = sc.note;
    playEndpoint.textContent = sc.endpoint;
    playRequest.textContent = JSON.stringify(sc.requestJson, null, 2);
    playResponse.textContent = JSON.stringify(sc.simulatedResponse, null, 2);
    playMeta.textContent = demoRunning && sc.liveSupported ? "Sẵn sàng gửi tới `gap serve` thật" : "Dữ liệu mẫu chuẩn hóa";
  }
  playSelect.addEventListener("change", updateScenarioView);
  updateScenarioView();

  async function pollServerStatus() {
    if (window.location.protocol === "file:" || window.location.hostname.endsWith(".github.io")) {
      if (window.location.hostname.endsWith(".github.io")) {
        statusPillText.textContent = "GitHub Pages (Mô phỏng tương tác)";
      }
      return;
    }
    try {
      const resp = await fetch("/api/status");
      if (!resp.ok) return;
      const st = await resp.json();
      serverAvailable = true;
      demoRunning = Boolean(st.demo_running);
      statusPillDot.classList.toggle("live", demoRunning);
      statusPillText.textContent = demoRunning
        ? `Lab sống đang chạy (: ${st.gap_port})`
        : st.binary_found
          ? "Kết nối máy chủ web (Lab chưa bật)"
          : "Máy chủ web sẵn (chưa build gap)";
      demoBadge.textContent = demoRunning ? `Đang chạy port ${st.gap_port}` : "Chế độ Mô phỏng";
      btnToggleDemo.textContent = demoRunning ? "Tắt Lab Thực Hành" : "Bật Lab Thực Hành (gap serve)";
      updateScenarioView();
    } catch (_e) {}
  }
  pollServerStatus();

  btnToggleDemo.addEventListener("click", async () => {
    if (!serverAvailable) {
      playMeta.textContent = "Hãy chạy `python3 web/serve.py` để bật Lab tiến trình thật.";
      return;
    }
    btnToggleDemo.disabled = true;
    try {
      const endpoint = demoRunning ? "/api/stop-demo" : "/api/start-demo";
      const resp = await fetch(endpoint, { method: "POST" });
      const data = await resp.json();
      if (!resp.ok) {
        playMeta.textContent = `Lỗi: ${data.error || resp.status}`;
      }
      await pollServerStatus();
    } finally {
      btnToggleDemo.disabled = false;
    }
  });

  btnRunPlay.addEventListener("click", async () => {
    const sc = D.playgroundScenarios.find((s) => s.id === playSelect.value) || D.playgroundScenarios[0];
    if (demoRunning && sc.liveSupported && serverAvailable) {
      btnRunPlay.disabled = true;
      try {
        const resp = await fetch("/api/playground", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ scenario_id: sc.id })
        });
        const out = await resp.json();
        playMeta.textContent = `Phản hồi thật từ gap serve · HTTP ${out.http_status} · ${out.latency_ms} ms`;
        playResponse.textContent = JSON.stringify(out.body, null, 2);
      } catch (err) {
        playMeta.textContent = `Lỗi gọi live: ${err}`;
      } finally {
        btnRunPlay.disabled = false;
      }
    } else {
      playMeta.textContent = "Kết quả mô phỏng chuẩn xác từ fixture repo (bấm Bật Lab nếu muốn gọi socket thật).";
      playResponse.textContent = JSON.stringify(sc.simulatedResponse, null, 2);
    }
  });

  // ------------------------------------------------------------------
  // 7. 31-Crate Architecture Grid Filter
  // ------------------------------------------------------------------
  const layerChips = document.getElementById("layer-chips");
  const cratesGrid = document.getElementById("crates-grid");
  const layers = ["Tất cả", "L0", "L1", "L2", "L3", "L4", "L5", "L6", "Dev"];
  let activeLayer = "Tất cả";

  function renderCrates() {
    for (const chip of layerChips.querySelectorAll(".chip")) {
      chip.setAttribute("aria-pressed", chip.dataset.layer === activeLayer ? "true" : "false");
    }
    const items = activeLayer === "Tất cả" ? D.crates : D.crates.filter((c) => c.layer === activeLayer);
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
    btn.textContent = l;
    btn.addEventListener("click", () => {
      activeLayer = l;
      renderCrates();
    });
    layerChips.appendChild(btn);
  }
  renderCrates();

  // ------------------------------------------------------------------
  // 8. CLI Reference + 9 Runbooks Accordion (modern-web-guidance: accordion)
  // ------------------------------------------------------------------
  const cliList = document.getElementById("cli-list");
  for (const c of D.cliSubcommands) {
    const det = document.createElement("details");
    det.innerHTML = `
      <summary><code>gap ${esc(c.sub)}</code></summary>
      <div class="details-body">
        <p>${esc(c.summary)}</p>
        <button type="button" class="btn btn-sm cli-help-btn" data-sub="${esc(c.sub)}">Chạy <code>gap ${esc(c.sub)} --help</code></button>
        <pre class="cli-help-out" hidden><code></code></pre>
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
      code.textContent = `$ ./target/dist/gap ${btn.dataset.sub} --help\n(Hãy chạy qua \`python3 web/serve.py\` để thực thi trực tiếp từ binary \`gap\`)`;
      return;
    }
    code.textContent = "Đang gọi ./target/dist/gap ...";
    try {
      const resp = await fetch(`/api/cli-help?sub=${encodeURIComponent(btn.dataset.sub)}`);
      const res = await resp.json();
      code.textContent = res.output || res.error || "(trống)";
    } catch (e) {
      code.textContent = String(e);
    }
  });

  const runbooksAcc = document.getElementById("runbooks-accordion");
  const toggleAllBtn = document.getElementById("btn-toggle-all-runbooks");

  for (const rb of D.runbooks) {
    const det = document.createElement("details");
    det.innerHTML = `
      <summary><span>${esc(rb.id)}</span> <span class="pill" style="margin-inline-start:auto;">${esc(rb.severity)}</span></summary>
      <div class="details-body">
        <p><strong>Điều kiện kích hoạt:</strong> ${esc(rb.trigger)}</p>
        <p><strong>Cách xử lý:</strong> ${esc(rb.action)}</p>
      </div>
    `;
    runbooksAcc.appendChild(det);
  }

  // Invoker command + fallback + toggle sync from modern-web-guidance: accordion
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

  // ------------------------------------------------------------------
  // 9. Tables (20 Invariants + STE-100 Dictionary) & Quiz
  // ------------------------------------------------------------------
  document.getElementById("inv-tbody").innerHTML = D.invariants
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

  document.getElementById("dict-tbody").innerHTML = D.dictionary
    .map(
      (d) => `
      <tr>
        <th scope="row"><code>${esc(d.term)}</code></th>
        <td>${esc(d.def)}</td>
        <td><em>${esc(d.forbidden)}</em></td>
      </tr>`
    )
    .join("");

  const quizBox = document.getElementById("quiz-container");
  const quizScore = document.getElementById("quiz-score");
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
      ? `Chính xác! ${item.explain}`
      : `Chưa đúng. Gợi ý: ${item.explain}`;
    const totalOk = Object.values(quizState).filter(Boolean).length;
    quizScore.textContent = `Điểm hiện tại: ${totalOk} / ${D.quiz.length}`;
  });

  // ------------------------------------------------------------------
  // 10. Instant Search Overlay (`/`)
  // ------------------------------------------------------------------
  const searchInput = document.getElementById("global-search");
  const searchResults = document.getElementById("search-results");
  const searchIndex = [
    ...D.architectureNodes.map((n) => ({ label: `Kiến trúc: ${n.title}`, href: "#tong-quan", action: () => renderInspector(n.id) })),
    ...D.levels.map((l, i) => ({ label: `Lộ trình: ${l.tabLabel}`, href: "#lo-trinh-6-cap", action: () => selectLevelTab(allTabs[i]) })),
    ...D.crates.map((c) => ({ label: `Crate ${c.layer}: ${c.name} — ${c.role}`, href: "#ban-do-crates", action: () => { activeLayer = c.layer; renderCrates(); } })),
    ...D.cliSubcommands.map((c) => ({ label: `CLI: gap ${c.sub} — ${c.summary}`, href: "#cli-va-runbooks" })),
    ...D.invariants.map((i) => ({ label: `${i.id}: ${i.req}`, href: "#bat-bien-va-tu-dien" }))
  ];

  searchInput.addEventListener("input", () => {
    const q = searchInput.value.trim().toLowerCase();
    if (!q) {
      searchResults.hidden = true;
      return;
    }
    const hits = searchIndex.filter((x) => x.label.toLowerCase().includes(q)).slice(0, 8);
    if (!hits.length) {
      searchResults.innerHTML = `<div style="padding:0.5rem;color:var(--muted);font-size:0.86rem;">Không tìm thấy mục phù hợp.</div>`;
    } else {
      searchResults.innerHTML = "";
      hits.forEach((h) => {
        const b = document.createElement("button");
        b.type = "button";
        b.className = "search-hit";
        b.textContent = h.label;
        b.addEventListener("click", () => {
          searchResults.hidden = true;
          if (h.action) h.action();
          document.querySelector(h.href)?.scrollIntoView({ behavior: "smooth" });
        });
        searchResults.appendChild(b);
      });
    }
    searchResults.hidden = false;
  });

  document.addEventListener("keydown", (ev) => {
    if (ev.key === "/" && document.activeElement !== searchInput && !/INPUT|TEXTAREA|SELECT/.test(document.activeElement.tagName)) {
      ev.preventDefault();
      searchInput.focus();
    } else if (ev.key === "Escape") {
      searchResults.hidden = true;
    }
  });
})();
