// Typed client for Hoglet's dashboard API. The response types are generated
// from the Rust structs by ts-rs (web/src/types/*) — Rust is the source of
// truth, so a breaking API change fails `tsc`, not the browser.

import type { Stats } from "./types/Stats";
import type { EventCount } from "./types/EventCount";
import type { TrendPoint } from "./types/TrendPoint";
import type { FunnelStep } from "./types/FunnelStep";
import type { RecentEvent } from "./types/RecentEvent";
import type { FlagDef } from "./types/FlagDef";

async function getJson<T>(url: string): Promise<T | null> {
  try {
    const r = await fetch(url);
    if (!r.ok) return null;
    return (await r.json()) as T;
  } catch {
    return null;
  }
}

const q = (token: string) => encodeURIComponent(token.trim() || "phc_demo");

export const api = {
  stats: (token: string) => getJson<Stats>(`/api/stats?token=${q(token)}`),
  topEvents: (token: string, limit = 8) =>
    getJson<EventCount[]>(`/api/top_events?token=${q(token)}&limit=${limit}`),
  recent: (token: string, limit = 25) =>
    getJson<RecentEvent[]>(`/api/recent?token=${q(token)}&limit=${limit}`),
  trend: (token: string, event: string, days: number) =>
    getJson<TrendPoint[]>(
      `/api/trend?token=${q(token)}&event=${encodeURIComponent(event)}&days=${days}`,
    ),
  flags: (token: string) => getJson<FlagDef[]>(`/api/flags?token=${q(token)}`),
  funnel: async (token: string, steps: string[]): Promise<FunnelStep[] | null> => {
    try {
      const r = await fetch("/api/funnel", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ token: token.trim(), steps }),
      });
      if (!r.ok) return null;
      return (await r.json()) as FunnelStep[];
    } catch {
      return null;
    }
  },
};

export type { Stats, EventCount, TrendPoint, FunnelStep, RecentEvent, FlagDef };
