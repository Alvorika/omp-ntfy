// omp-ntfy OMP extension — notifies a remote server about the main session
// and reports lightweight runtime state for the omp-ntfy dashboard.
//
// Loaded by OMP automatically from ~/.omp/agent/extensions/omp-ntfy.ts
// (auto-discovered user extension root). Install/update with:
//   omp-ntfy omp install --url <server-url> --token <token>
//
// Uses the official extension API only (pi.on). Never blocks the agent:
// every HTTP POST has a 3s timeout and all errors are swallowed/logged.
//
// Event discipline (verified against OMP 18.0.9):
// - `session_stop` is the completion signal: main session only (never
//   subagents), fired once at settle, not emitted on abort.
// - `turn_start`/`agent_end` derive the activity state; there is no explicit
//   idle event, so "idle" is inferred from terminal agent_end only.
// - Reports are coalesced (at most one POST per session per 5s) and carry
//   metadata only — never prompts beyond the latest, tool arguments, or
//   conversation history. No thinking content.

const OMP_NTFY_EXT_VERSION = "0.2.0";

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

function modelOf(ctx: any): string | undefined {
  try {
    const m = ctx?.models?.current?.() ?? ctx?.model;
    if (!m) return undefined;
    const id = m.id || m.name;
    const provider = m.provider || m.api;
    if (!id) return undefined;
    return provider ? `${provider}/${id}` : String(id);
  } catch {
    return undefined;
  }
}

function textOf(content: unknown): string | undefined {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    for (const part of content) {
      if (part && typeof part === "object" && (part as any).type === "text") {
        const t = (part as any).text;
        if (typeof t === "string" && t.trim()) return t;
      }
    }
  }
  return undefined;
}

/** Strip leading wrapper tags like <environment_context>/<attachment>. */
function stripLeadingTags(text: string): string {
  return text.replace(/^(<[^>\n]+>\s*\n?)+/, "").trim();
}

/** First meaningful line: non-empty, not indented (code/output), not a list
 *  item or code fence. Deterministic; no LLM. */
function firstMeaningfulLine(text: string): string | undefined {
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (!line) continue;
    if (/^\s/.test(raw)) continue; // indented: code/output
    if (/^```|^[-*#] |^>\s/.test(line)) continue;
    return line;
  }
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (line) return line;
  }
  return undefined;
}

const clip = (s: string, n: number) => (s.length > n ? s.slice(0, n) : s);

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
  const pending = new Map<string, { snapshot: Record<string, unknown>; dirty: boolean }>();
  const flushTimers = new Map<string, ReturnType<typeof setTimeout>>();

  function record(ev: any, ctx: any, patch: Record<string, unknown>) {
    const sid = (typeof ev?.session_id === "string" && ev.session_id) || sessionIdOf(ctx);
    if (!sid) return;
    const entry = pending.get(sid) ?? {
      snapshot: { session_id: sid, hostname: hostnameOf() || undefined },
      dirty: true,
    };
    Object.assign(entry.snapshot, patch);
    entry.dirty = true;
    pending.set(sid, entry);
    scheduleFlush(sid, false);
  }

  function scheduleFlush(sid: string, immediate: boolean) {
    if (immediate) {
      flush(sid);
      return;
    }
    if (flushTimers.has(sid)) return;
    const t = setTimeout(() => flush(sid), 5000);
    flushTimers.set(sid, t);
  }

  function flush(sid: string) {
    void sendPending(sid);
  }

  /** Build the coalesced snapshot for a session and POST it (awaited). */
  async function sendPending(sid: string): Promise<void> {
    const t = flushTimers.get(sid);
    if (t) clearTimeout(t);
    flushTimers.delete(sid);
    const entry = pending.get(sid);
    if (!entry || !entry.dirty) return;
    entry.dirty = false;
    const snapshot = { ...entry.snapshot, ts: Date.now() };
    await postRuntime(snapshot);
  }

  async function postRuntime(snapshot: Record<string, unknown>): Promise<boolean> {
    try {
      const res = await fetch(`${baseUrl}/api/runtime`, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          ...(cfg.token ? { authorization: `Bearer ${cfg.token}` } : {}),
        },
        body: JSON.stringify(snapshot),
        signal: AbortSignal.timeout(3000),
      });
      if (!res.ok) {
        const detail = await res.text().catch(() => "");
        pi.logger?.warn?.(`[omp-ntfy] runtime rejected: ${res.status} ${detail.slice(0, 120)}`);
        return false;
      }
      pi.logger?.debug?.(`[omp-ntfy] runtime posted ${JSON.stringify(snapshot).slice(0, 240)}`);
      return true;
    } catch (err: any) {
      const msg = err instanceof Error ? err.message : String(err);
      pi.logger?.warn?.(`[omp-ntfy] runtime update failed: ${msg}`);
      return false;
    }
  }

  pi.on("session_start", (_ev: any, ctx: any) => {
    const sid = sessionIdOf(ctx);
    if (sid) {
      startTimes.set(sid, Date.now());
      record(_ev, ctx, {
        title: sessionNameOf(ctx) ?? undefined,
        cwd: typeof ctx?.cwd === "string" ? ctx.cwd : undefined,
        model: modelOf(ctx),
        state: "active",
        started_at: Date.now(),
      });
    }
  });

  pi.on("message_start", (ev: any, ctx: any) => {
    const msg = ev?.message;
    if (msg?.role === "user") {
      const text = textOf(msg.content);
      if (text) {
        record(ev, ctx, { latest_user_prompt: clip(stripLeadingTags(text), 500) });
      }
    }
  });

  pi.on("message_end", (ev: any, ctx: any) => {
    const msg = ev?.message;
    if (msg?.role === "assistant") {
      const text = textOf(msg.content);
      if (text) {
        const preview = firstMeaningfulLine(text);
        if (preview) {
          record(ev, ctx, { latest_assistant_preview: clip(preview, 1000) });
        }
      }
    }
  });

  pi.on("turn_start", (ev: any, ctx: any) => {
    const n = typeof ev?.turnIndex === "number" ? ev.turnIndex : undefined;
    record(ev, ctx, {
      activity: "running",
      activity_detail: n !== undefined ? `turn ${n}` : "working",
    });
  });

  pi.on("tool_execution_start", (ev: any, ctx: any) => {
    if (typeof ev?.toolName === "string" && ev.toolName) {
      record(ev, ctx, { activity_detail: `tool: ${ev.toolName}` });
    }
  });

  pi.on("agent_end", (ev: any, ctx: any) => {
    // willContinue=true means an automatic continuation is scheduled; the
    // agent is still working. Only a terminal end marks the idle window.
    if (ev?.willContinue !== true) {
      record(ev, ctx, { activity: "idle" });
    }
  });

  pi.on("session_stop", async (ev: any, ctx: any) => {
    const sid = (typeof ev?.session_id === "string" && ev.session_id) || sessionIdOf(ctx);
    const startedAt = (sid && startTimes.get(sid)) || Date.now();
    const finishedAt = Date.now();
    const durationMin = Math.max(0, Math.round((finishedAt - startedAt) / 60000));
    const cwd = typeof ctx?.cwd === "string" ? ctx.cwd : "";

    // Final runtime snapshot: state + optional preview. This MUST be awaited:
    // in `-p` mode the process exits right after this handler, so a
    // fire-and-forget POST would be killed in flight.
    record(ev, ctx, {
      state: "completed",
      finished_at: finishedAt,
      title: sessionNameOf(ctx) ?? undefined,
      model: modelOf(ctx),
      activity: "idle",
    });
    if (sid) await sendPending(sid);

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
    if (sid) {
      startTimes.delete(sid);
      scheduleFlush(sid, true);
      pending.delete(sid);
    }
  });
}