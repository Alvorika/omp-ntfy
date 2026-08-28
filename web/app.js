(async () => {
  const $ = (id) => document.getElementById(id);
  const btn = $("toggle");
  let sw = null;
  let vapid = null;
  let sub = null;
  let deviceName = "";

  const urlBase64ToUint8Array = (b64) => {
    const pad = "=".repeat((4 - (b64.length % 4)) % 4);
    const raw = atob((b64 + pad).replace(/-/g, "+").replace(/_/g, "/"));
    return Uint8Array.from([...raw].map((c) => c.charCodeAt(0)));
  };

  function setHint(text) {
    $("hint").textContent = text;
  }

  function showState(perm, subscribed) {
    $("perm-state").textContent = perm;
    $("sub-state").textContent = subscribed ? "yes" : "no";
  }

  async function refresh() {
    if (!("Notification" in window)) {
      showState("unsupported", false);
      btn.disabled = true;
      btn.textContent = "Unsupported browser";
      setHint("Push notifications are not available in this browser.");
      return;
    }
    const perm = Notification.permission;
    showState(perm, false);
    btn.disabled = false;
    if (perm === "denied") {
      btn.textContent = "Permission denied";
      btn.disabled = true;
      setHint("Enable notifications for this site in browser settings.");
      return;
    }
    sub = sw ? await sw.pushManager.getSubscription() : null;
    showState(perm, !!sub);
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
        refresh();
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
      $("server-state").textContent = "server ok";
      $("server-state").classList.remove("bad");
      refresh();
    } catch (e) {
      setHint(`subscribe failed: ${e.message}`);
      refresh();
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
    refresh();
  }

  async function showLast() {
    try {
      const r = await fetch("/api/status");
      if (!r.ok) return;
      const st = await r.json();
      if (!st.last_event) return;
      const d = new Date(st.last_event.ts * 1000);
      const name = st.last_event.session_name || st.last_event.cwd || "session";
      const mins = st.last_event.duration_min;
      $("last").textContent = `Last: ${name}${mins ? ` · ${mins}m` : ""} at ${d.toLocaleTimeString()}`;
    } catch {
      // offline — ignore
    }
  }

  const saved = localStorage.getItem("omp-ntfy:device") || "";
  $("device-name").value = saved;
  deviceName = saved.trim();
  $("device-name").addEventListener("input", (e) => {
    deviceName = e.target.value.trim();
    localStorage.setItem("omp-ntfy:device", deviceName);
  });

  try {
    const r = await fetch("/api/vapid-public");
    if (!r.ok) throw new Error(String(r.status));
    vapid = await r.json();
    $("server-state").textContent = "server ok";
  } catch {
    $("server-state").textContent = "server unreachable";
    $("server-state").classList.add("bad");
  }
  if (!vapid) return;

  if (!("serviceWorker" in navigator)) {
    setHint("Service workers unsupported in this browser.");
    return;
  }
  if (!("PushManager" in window)) {
    setHint("Push API unsupported here. On iOS, add to Home Screen first, then open the app.");
    return;
  }
  try {
    await navigator.serviceWorker.register("/sw.js");
    sw = await navigator.serviceWorker.ready;
    navigator.serviceWorker.addEventListener("message", (e) => {
      if (e.data && e.data.type === "omp-ntfy:push") {
        const d = new Date();
        $("last").textContent = `Received: ${e.data.title} · ${e.data.body} at ${d.toLocaleTimeString()}`;
      }
    });
    await refresh();
    showLast();
  } catch (e) {
    setHint(`service worker failed: ${e.message}`);
  }
})();