// omp-ntfy OMP extension — notify a remote server when the main session ends.
//
// Loaded by OMP automatically from ~/.omp/agent/extensions/omp-ntfy.ts
// (auto-discovered user extension root). Install/update with:
//   omp-ntfy omp install --url http://host:8090 --token <token>
//
// Uses the official extension API only (pi.on). Never blocks the agent:
// the HTTP POST has a 3s timeout and all errors are swallowed/logged.
//
// Event choice: `session_stop` — the main-session stop hook. OMP never
// emits it for task/subagent sessions and it fires once when the session
// settles (not per turn), so there are no duplicate or intermediate
// notifications. Aborted sessions (e.g. Escape) do not emit it.
//
// The payload carries metadata only — never prompts, tool calls, or
// conversation content.

const OMP_NTFY_EXT_VERSION = "0.1.0";

import { readFileSync } from "node:fs";
import { hostname as osHostname } from "node:os";

function home(): string {
  return process.env.HOME ?? "";
}

function clientConfigPath(): string {
  const override = process.env.OMP_NTFY_CONFIG;
  if (override) return override;
  const base = process.env.XDG_CONFIG_HOME || `${home()}/.config`;
  return `${base}/omp-ntfy/client.json`;
}

interface ClientConfig {
  url?: string;
  token?: string;
}

function loadClientConfig(): ClientConfig {
  const cfg: ClientConfig = {};
  try {
    const raw = readFileSync(clientConfigPath(), "utf8");
    const parsed = JSON.parse(raw) as Record<string, unknown>;
    if (typeof parsed.url === "string") cfg.url = parsed.url;
    if (typeof parsed.token === "string") cfg.token = parsed.token;
  } catch {
    // missing/unreadable config: env may still provide the values
  }
  const envUrl = process.env.OMP_NTFY_URL;
  const envToken = process.env.OMP_NTFY_TOKEN;
  if (envUrl) cfg.url = envUrl;
  if (envToken) cfg.token = envToken;
  return cfg;
}

function hostnameOf(): string {
  try {
    return osHostname();
  } catch {
    return process.env.HOSTNAME ?? "";
  }
}

function sessionIdOf(ctx: any): string | undefined {
  try {
    const sid = ctx?.sessionManager?.getSessionId?.();
    return typeof sid === "string" && sid ? sid : undefined;
  } catch {
    return undefined;
  }
}

function sessionNameOf(ctx: any): string | undefined {
  try {
    const n = ctx?.sessionManager?.getSessionName?.();
    return typeof n === "string" && n ? n : undefined;
  } catch {
    return undefined;
  }
}

export default function (pi: any) {
  const cfg = loadClientConfig();
  if (!cfg.url) {
    pi.logger?.warn?.(
      "[omp-ntfy] no server URL (set OMP_NTFY_URL or ~/.config/omp-ntfy/client.json); notifications disabled",
    );
    return;
  }
  const baseUrl = cfg.url.replace(/\/+$/, "");
  const startTimes = new Map<string, number>();

  pi.on("session_start", (_ev: any, ctx: any) => {
    const sid = sessionIdOf(ctx);
    if (sid) startTimes.set(sid, Date.now());
  });

  pi.on("session_stop", async (ev: any, ctx: any) => {
    const sid =
      (typeof ev?.session_id === "string" && ev.session_id) || sessionIdOf(ctx);
    const startedAt = (sid && startTimes.get(sid)) || Date.now();
    const finishedAt = Date.now();
    const durationMin = Math.max(0, Math.round((finishedAt - startedAt) / 60000));
    const cwd = typeof ctx?.cwd === "string" ? ctx.cwd : "";

    const payload = {
      event: "session_stop",
      session_id: sid || undefined,
      session_name: sessionNameOf(ctx),
      cwd: cwd || undefined,
      hostname: hostnameOf() || undefined,
      started_at: startedAt,
      finished_at: finishedAt,
      duration_min: durationMin,
      outcome: "completed",
    };

    try {
      const res = await fetch(`${baseUrl}/api/events`, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          ...(cfg.token ? { authorization: `Bearer ${cfg.token}` } : {}),
        },
        body: JSON.stringify(payload),
        signal: AbortSignal.timeout(3000),
      });
      if (!res.ok) {
        const detail = await res.text().catch(() => "");
        pi.logger?.warn?.(
          `[omp-ntfy] server rejected notification: ${res.status} ${detail.slice(0, 160)}`,
        );
      } else {
        pi.logger?.debug?.(`[omp-ntfy] notification delivered (${res.status})`);
      }
    } catch (err: any) {
      const msg = err instanceof Error ? err.message : String(err);
      pi.logger?.warn?.(`[omp-ntfy] notification failed: ${msg}`);
      try {
        ctx?.ui?.notify?.(`omp-ntfy: notification failed (${msg})`, "error");
      } catch {
        // never let UI reporting break the session
      }
    }
  });

  pi.on("session_shutdown", (_ev: any, ctx: any) => {
    const sid = sessionIdOf(ctx);
    if (sid) startTimes.delete(sid);
  });
}