// omp-ntfy PWA: two views (Dashboard + Settings), hash-routed.
(async () => {
  const $ = (id) => document.getElementById(id);
  const btn = $("toggle");
  let sw = null;
  let vapid = null;
  let sub = null;
  let deviceName = "";
  let settings = null;
  let dashTimer = null;

  // ------------------------------------------------------------ helpers

  const urlBase64ToUint8Array = (b64) => {
    const pad = "=".repeat((4 - (b64.length % 4)) % 4);
    const raw = atob((b64 + pad).replace(/-/g, "+").replace(/_/g, "/"));
    return Uint8Array.from([...raw].map((c) => c.charCodeAt(0)));
  };

  const esc = (s) =>
    String(s ?? "")
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");

  const clip = (s, n) => (s.length > n ? s.slice(0, n) + "…" : s);

  const ago = (ts) => {
    if (!ts) return "";
    const s = Math.max(0, Math.floor(Date.now() / 1000 - ts));
    if (s < 60) return `${s}s ago`;
    if (s < 3600) return `${Math.floor(s / 60)}m ago`;
    if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
    return `${Math.floor(s / 86400)}d ago`;
  };

  const fmtDuration = (sec) => {
    if (sec == null) return "";
    if (sec < 60) return `${sec}s`;
    if (sec < 3600) return `${Math.floor(sec / 60)}m`;
    return `${Math.floor(sec / 3600)}h ${Math.floor((sec % 3600) / 60)}m`;
  };

  async function api(path, opts) {
    const r = await fetch(path, opts);
    if (!r.ok) throw new Error(`${path} ${r.status}`);
    return r.json();
  }

  function setServer(ok) {
    $("server-state").textContent = ok ? "server ok" : "offline";
    $("server-state").classList.toggle("bad", !ok);
  }

  // ------------------------------------------------------------ routing

  function route() {
    const view = location.hash.startsWith("#/settings") ? "settings" : "dashboard";
    for (const v of ["dashboard", "settings"]) {
      $(`view-${v}`).classList.toggle("hidden", v !== view);
    }
    for (const a of document.querySelectorAll("nav a")) {
      a.classList.toggle("active", a.dataset.view === view);
    }
    if (view === "dashboard") startPoller();
    else stopPoller();
  }

  window.addEventListener("hashchange", route);
  window.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") startPoller();
    else stopPoller();
  });

  function startPoller() {
    if (!location.hash.startsWith("#/settings") && !dashTimer) {
      renderDashboard();
      dashTimer = setInterval(renderDashboard, 5000);
    }
  }
  function stopPoller() {
    if (dashTimer) {
      clearInterval(dashTimer);
      dashTimer = null;
    }
  }

  // ---------------------------------------------------------- dashboard

  async function renderDashboard() {
    if (document.visibilityState === "hidden") return;
    try {
      const [sessions, stats, binding] = await Promise.all([
        api("/api/sessions"),
        api("/api/stats"),
        api("/api/omp-binding"),
      ]);
      setServer(true);
      renderStats(stats);
      renderBinding(binding);
      renderSessions(sessions);
      $("last").textContent = `server time ${new Date(sessions.server_time * 1000).toLocaleTimeString()}`;
    } catch (e) {
      setServer(false);
      $("sessions-body").innerHTML = `<p class="hint">offline — last snapshot ${ago(
        +(localStorage.getItem("omp-ntfy:dash-ts") || 0),
      )}</p>`;
      const cached = localStorage.getItem("omp-ntfy:dash");
      if (cached) {
        const d = JSON.parse(cached);
        renderSessions(d.sessions || []);
      }
    }
  }

  function renderStats(st) {
    const el = $("stats-body");
    if (!st.available || st.requests == null) {
      el.innerHTML = `<p class="muted">no usage records yet</p>`;
      return;
    }
    const rows = [
      ["requests", st.requests],
      ["tokens in", st.tokens_in],
      ["tokens out", st.tokens_out],
      ["cache read", st.cache_read],
      ["est. cost", `$${st.cost_usd.toFixed(2)}`],
      ["subagent reqs", st.subagent_requests],
    ];
    el.innerHTML =
      rows.map(([k, v]) => `<div class="row"><span>${k}</span><strong>${v}</strong></div>`).join("") +
      `<p class="muted">models: ${esc((st.models || []).slice(0, 4).join(", "))} · last record ${ago(st.last_record_ts)}</p>`;
    localStorage.setItem("omp-ntfy:dash", JSON.stringify({ sessions: [] }));
  }

  function renderBinding(b) {
    const el = $("binding-body");
    const rows = [
      ["agent dir", b.agent_dir || "—"],
      ["OMP home", b.home || "—"],
      ["extension", b.extension_installed ? `installed v${b.extension_version || "?"}` : "not installed"],
      ["client url", b.client_url_ok ? "ok" : "missing"],
      ["client token", b.client_token_ok ? "set" : "missing"],
    ];
    el.innerHTML = rows
      .map(([k, v]) => `<div class="row"><span>${esc(k)}</span><strong>${esc(v)}</strong></div>`)
      .join("");
  }

  function renderSessions(d) {
    const sessions = d.sessions || [];
    const runtime = d.runtime || [];
    $("session-count").textContent = `${sessions.length} · ${runtime.length} live`;
    if (!sessions.length) {
      $("sessions-body").innerHTML = `<p class="muted">no sessions yet — run an OMP agent on the host</p>`;
      return;
    }
    const liveIds = new Set(runtime.map((r) => r.session_id));
    const ds = settings?.dashboard ?? {};
    const previewLen = ds.preview_length ?? 200;

    $("sessions-body").innerHTML = sessions
      .map((s) => {
        const live = liveIds.has(s.session_id);
        const rr = runtime.find((r) => r.session_id === s.session_id);
        const state = s.state === "active" && live ? "active" : s.state;
        const stateCls = state === "error" ? "bad" : state === "active" ? "ok" : "";
        const title = s.title || s.session_id.slice(0, 8);
        const dur = s.state === "active" ? fmtDuration((d.server_time || 0) - (s.started_at || 0))
          : fmtDuration(s.ended_at && s.started_at ? s.ended_at - s.started_at : null);
        const parts = [];
        let statusLine = "";
        if (ds.show_runtime !== false) {
          if (live && rr) {
            statusLine = `<span class="pill ok live">● ${esc(rr.activity || "active")}${rr.activity_detail ? ` · ${esc(rr.activity_detail)}` : ""}</span>`;
          } else if (s.state === "active") {
            statusLine = `<span class="pill">● active (no live signal)</span>`;
          }
        }
        if (state === "error") statusLine = `<span class="pill bad">error</span>`;
        if (ds.show_latest_prompt !== false && s.latest_user_prompt) {
          parts.push(`<div class="prompt">${esc(clip(s.latest_user_prompt, 200))}</div>`);
        }
        if (ds.show_agent_preview && s.latest_assistant_preview) {
          parts.push(`<div class="preview">${esc(clip(s.latest_assistant_preview, previewLen))}</div>`);
        }
        const meta = [];
        if (ds.show_cwd !== false && s.cwd) meta.push(esc(s.cwd));
        if (ds.show_model !== false && s.model) meta.push(esc(s.model));
        if (dur) meta.push(dur);
        meta.push(`updated ${ago(s.last_seen_at)}`);
        return `<div class="session ${state === "error" ? "stale" : ""}">
          <div class="session-head">
            <span class="session-title ${state === "active" ? "" : "muted"}">${esc(title)}</span>
            ${statusLine}
            <span class="pill ${stateCls}">${esc(state)}</span>
          </div>
          ${parts.join("")}
          <div class="muted meta">${meta.join(" · ")}</div>
        </div>`;
      })
      .join("");
    localStorage.setItem("omp-ntfy:dash", JSON.stringify({ sessions }));
    localStorage.setItem("omp-ntfy:dash-ts", String(Math.floor(Date.now() / 1000)));
  }

  // ------------------------------------------------------------ settings

  async function renderSettings() {
    try {
      settings = await api("/api/settings");
      setServer(true);
      buildSettingsForm();
    } catch (e) {
      setServer(false);
      settings = JSON.parse(localStorage.getItem("omp-ntfy:settings") || "null");
      if (settings) {
        buildSettingsForm();
        $("settings-save-state").textContent = "offline — showing last saved";
      }
    }
  }

  function buildSettingsForm() {
    const n = settings.notifications;
    const d = settings.dashboard;
    const toggle = (key, group, label, desc) =>
      `<label class="check"><input type="checkbox" data-group="${group}" data-key="${key}" ${
        (group === "notifications" ? n : d)[key] ? "checked" : ""
      }> <span>${esc(label)}</span>${desc ? `<small>${esc(desc)}</small>` : ""}</label>`;
    const num = (key, group, label) =>
      `<label class="check">${esc(label)} <input type="number" min="0" max="1200" value="${
        (group === "notifications" ? n : d)[key]
      }" data-group="${group}" data-key="${key}" class="num"></label>`;

    $("settings-notify").innerHTML = [
      toggle("enabled", "notifications", "Enable push notifications"),
      toggle("use_title", "notifications", "Include session title"),
      toggle("include_user_message", "notifications", "Include latest user message"),
      toggle("include_agent_preview", "notifications", "Include agent response preview"),
      num("preview_length", "notifications", "Preview length (chars)"),
      `<div class="row"><span>Notify on</span><select data-group="notifications" data-key="terminal_states" multiple size="4">
         ${["completed", "error", "aborted", "stopped"]
           .map((s) => `<option value="${s}" ${n.terminal_states.includes(s) ? "selected" : ""}>${s}</option>`)
           .join("")}
       </select></div>`,
    ].join("");

    $("settings-dash").innerHTML = [
      toggle("show_latest_prompt", "dashboard", "Show latest user prompt"),
      toggle("show_agent_preview", "dashboard", "Show agent response preview"),
      num("preview_length", "dashboard", "Preview length (chars)"),
      toggle("show_cwd", "dashboard", "Show workspace"),
      toggle("show_runtime", "dashboard", "Show live runtime state"),
      toggle("show_stats", "dashboard", "Show usage summary"),
      toggle("show_model", "dashboard", "Show model"),
    ].join("");

    document.querySelectorAll("#settings-notify input, #settings-dash input").forEach((el) => {
      el.addEventListener("change", () => saveSettings());
      el.addEventListener("input", debounce(saveSettings, 300));
    });
    document.querySelectorAll("#settings-notify select, #settings-dash select").forEach((el) => {
      el.addEventListener("change", () => saveSettings());
    });
  }

  let saveTimer = null;
  function debounce(fn, ms) {
    return (...args) => {
      clearTimeout(saveTimer);
      saveTimer = setTimeout(() => fn(...args), ms);
    };
  }

  async function saveSettings() {
    const n = settings.notifications;
    const d = settings.dashboard;
    document.querySelectorAll("#settings-notify input, #settings-dash input").forEach((el) => {
      const group = el.dataset.group;
      const key = el.dataset.key;
      const target = group === "notifications" ? n : d;
      target[key] = el.type === "checkbox" ? el.checked : Math.max(0, Math.min(1200, +el.value || 0));
    });
    const sel = document.querySelector('select[data-key="terminal_states"]');
    if (sel) {
      n.terminal_states = [...sel.selectedOptions].map((o) => o.value);
    }
    localStorage.setItem("omp-ntfy:settings", JSON.stringify(settings));
    $("settings-save-state").textContent = "saving…";
    try {
      await api("/api/settings", {
        method: "PUT",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(settings),
      });
      $("settings-save-state").textContent = `saved ${new Date().toLocaleTimeString()}`;
      setServer(true);
    } catch (e) {
      $("settings-save-state").textContent = "save failed — will retry on next change";
      setServer(false);
    }
  }

  // ------------------------------------------------------ subscription (kept)

  function setHint(text) {
    $("hint").textContent = text;
  }

  async function refreshSub() {
    if (!("Notification" in window)) {
      $("perm-state").textContent = "unsupported";
      btn.disabled = true;
      btn.textContent = "Unsupported browser";
      return;
    }
    const perm = Notification.permission;
    $("perm-state").textContent = perm;
    btn.disabled = false;
    if (perm === "denied") {
      btn.textContent = "Permission denied";
      btn.disabled = true;
      setHint("Enable notifications for this site in browser settings.");
      return;
    }
    sub = sw ? await sw.pushManager.getSubscription() : null;
    $("sub-state").textContent = sub ? "yes" : "no";
    if (sub) {
      btn.textContent = "Unsubscribe";
      btn.onclick = unsubscribe;
      setHint("This device is registered for push notifications.");
    } else if (perm === "granted") {
      btn.textContent = "Subscribe";
      btn.onclick = subscribe;
      setHint("Register this device for push notifications.");
    } else {
      btn.textContent = "Enable + subscribe";
      btn.onclick = subscribe;
      setHint("Requesting notification permission and registering this device.");
    }
  }

  async function subscribe() {
    btn.disabled = true;
    try {
      const permission = await Notification.requestPermission();
      if (permission !== "granted") {
        setHint("Permission not granted — notifications stay off.");
        refreshSub();
        return;
      }
      sub = await sw.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: urlBase64ToUint8Array(vapid.key),
      });
      const json = sub.toJSON();
      const res = await fetch("/api/subscribe", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          endpoint: json.endpoint,
          keys: json.keys,
          device_name: deviceName || undefined,
        }),
      });
      if (!res.ok) throw new Error(`server responded ${res.status}`);
      refreshSub();
    } catch (e) {
      setHint(`subscribe failed: ${e.message}`);
      refreshSub();
    }
  }

  async function unsubscribe() {
    btn.disabled = true;
    try {
      if (sub) {
        await fetch("/api/subscribe", {
          method: "DELETE",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ endpoint: sub.endpoint }),
        });
        await sub.unsubscribe();
      }
    } catch (e) {
      setHint(`unsubscribe failed: ${e.message}`);
    }
    sub = null;
    refreshSub();
  }

  // ------------------------------------------------------------ boot

  const saved = localStorage.getItem("omp-ntfy:device") || "";
  $("device-name").value = saved;
  deviceName = saved.trim();
  $("device-name").addEventListener("input", (e) => {
    deviceName = e.target.value.trim();
    localStorage.setItem("omp-ntfy:device", deviceName);
    // re-register name on the server for an existing subscription
    if (sub) {
      fetch("/api/subscribe", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          endpoint: sub.endpoint,
          keys: sub.toJSON().keys,
          device_name: deviceName || undefined,
        }),
      }).catch(() => {});
    }
  });

  try {
    const r = await fetch("/api/vapid-public");
    if (!r.ok) throw new Error(String(r.status));
    vapid = await r.json();
    setServer(true);
  } catch {
    setServer(false);
    renderDashboard(); // still render cached/offline UI
  }

  if ("serviceWorker" in navigator && "PushManager" in window) {
    try {
      await navigator.serviceWorker.register("/sw.js");
      sw = await navigator.serviceWorker.ready;
      await refreshSub();
    } catch (e) {
      setHint(`service worker failed: ${e.message}`);
    }
  }

  renderSettings().then(route);
})();