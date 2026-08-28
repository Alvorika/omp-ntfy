# Changelog

All notable changes to omp-ntfy are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-08-29

First release: notify your devices when an Oh My Pi agent finishes a task.

### Added

- **OMP extension** (`ext/omp-ntfy.ts`): official `session_stop` event only —
  never fires for subagents, once per session settle, no per-turn spam.
  Stateless; reads endpoint/token from `~/.config/omp-ntfy/client.json` or
  `OMP_NTFY_URL`/`OMP_NTFY_TOKEN`. 3s timeout; agent never blocked.
- **Notification server** (single static Rust binary, zero runtime deps):
  - `POST /api/events` with Bearer-token auth, strict JSON validation,
    16 KiB body limit; pushes fan out asynchronously so the extension's
    short HTTP timeout is never hit.
  - Web Push per RFC 8030 (VAPID ES256 JWT) + RFC 8291 (aes128gcm payload
    encryption); byte-exact against the RFC 8291 Appendix A test vector.
  - Dead subscription pruning on push-service 404/410.
  - JSON store (atomic rename) at `$XDG_STATE_HOME/omp-ntfy/store.json`.
  - Embeds the PWA (manifest, service worker, subscribe/unsubscribe page).
- **PWA**: installable; subscribe/unsubscribe, permission handling,
  status card; works as a desktop PWA and iOS Home Screen Web Push.
- **CLI**: `init`, `serve`, `token`, `token-rotate`, `subs`, `subs-rm`,
  `push-test`, `service install|start|stop|restart|status|uninstall`
  (systemd user unit, no root), `omp install|update|status|uninstall`.
- **Deployment**: native systemd user service (primary, XDG paths) and an
  optional Docker image (static musl build; identical env-var config;
  volume-persisted config + subscriptions).

### Security notes

- VAPID subject must be a well-formed `mailto:`/`https:` URI — Apple
  rejects non-FQDN subjects (e.g. `localhost`) with `403 BadJwtToken`
  (`sub` claim). Default subject is `mailto:omp-ntfy@example.com`; set
  `OMP_NTFY_SUBJECT` to a real contact address.
- Notifications carry metadata only (session name, cwd basename, duration);
  never prompts, tool arguments, commands, or message content.