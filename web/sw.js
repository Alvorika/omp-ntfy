// omp-ntfy service worker — receives Web Push and shows a notification.
self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));

self.addEventListener("push", (e) => {
  let title = "OMP";
  let body = "Task completed";
  let tag = "omp-ntfy";
  let url = "/";
  if (e.data) {
    try {
      const d = e.data.json();
      if (d.title) title = String(d.title);
      if (d.body) body = String(d.body);
      if (d.tag) tag = String(d.tag);
      if (d.url) url = String(d.url);
    } catch {
      // not JSON; keep defaults
    }
  }
  const options = {
    body,
    tag,
    renotify: true,
    icon: "/icon-192.png",
    badge: "/icon-192.png",
    data: { url },
  };
  e.waitUntil(self.registration.showNotification(title, options));
  e.waitUntil(
    self.clients.matchAll({ type: "window", includeUncontrolled: true }).then((cs) => {
      for (const c of cs) c.postMessage({ type: "omp-ntfy:push", title, body });
    }),
  );
});
self.addEventListener("notificationclick", (e) => {
  e.notification.close();
  const url = (e.notification.data && e.notification.data.url) || "/";
  e.waitUntil(
    self.clients
      .matchAll({ type: "window", includeUncontrolled: true })
      .then((list) => {
        for (const c of list) {
          if ("focus" in c) {
            c.navigate(url);
            return c.focus();
          }
        }
        return self.clients.openWindow(url);
      }),
  );
});