// Typed client for Hoglet's dashboard API. The response types are generated
// from the Rust structs by ts-rs (web/src/types/*) — Rust is the source of
// truth, so a breaking API change fails `tsc`, not the browser.

import type { Stats } from "./types/Stats";
import type { EventCount } from "./types/EventCount";
import type { TrendPoint } from "./types/TrendPoint";
import type { FunnelStep } from "./types/FunnelStep";
import type { RecentEvent } from "./types/RecentEvent";
import type { FlagDef } from "./types/FlagDef";
import type { Query } from "./types/Query";
import type { QueryResponse } from "./types/QueryResponse";

/** Shapes the catalog endpoints return; no ts-rs export for these yet. */
export interface PropertyKey {
  key: string; source: string; type_guess: string; count: number;
}
export interface PropertyValue { value: string; count: number }

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
  me: () => getJson<{ user: { id: string; email: string; name: string } | null }>("/api/auth/me"),
  login: async (email: string, password: string) => {
    const r = await fetch("/api/auth/login", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ email, password }),
    });
    if (!r.ok) return null;
    return r.json();
  },
  setup: async (email: string, password: string, org_name: string) => {
    const r = await fetch("/api/auth/setup", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ email, password, org_name }),
    });
    if (!r.ok) return null;
    return r.json();
  },
  logout: () => fetch("/api/auth/logout", { method: "POST" }),

  // ── Catalog: what the insight builder autocompletes against ──
  catalogEvents: (token: string, limit = 200) =>
    getJson<string[]>(`/api/catalog/events?token=${q(token)}&limit=${limit}`),
  catalogProperties: (token: string, source: "event" | "person" = "event") =>
    getJson<PropertyKey[]>(`/api/catalog/properties?token=${q(token)}&source=${source}`),
  catalogValues: (token: string, key: string, limit = 50) =>
    getJson<PropertyValue[]>(
      `/api/catalog/values?token=${q(token)}&key=${encodeURIComponent(key)}&limit=${limit}`,
    ),

  // ── The IR endpoint the builder drives ──
  runQuery: async (token: string, query: Query, refresh = false): Promise<QueryResponse | null> => {
    try {
      const r = await fetch("/api/query", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ token: token.trim(), query, refresh }),
      });
      if (!r.ok) return null;
      return (await r.json()) as QueryResponse;
    } catch {
      return null;
    }
  },
};

export type { Stats, EventCount, TrendPoint, FunnelStep, RecentEvent, FlagDef, Query, QueryResponse };

export interface SavedInsight {
  id: string; token: string; name: string; description: string;
  query_ir: any; created_by: string; created_at: number; updated_at: number;
}

export interface DashboardTile {
  insight_id: string; x: number; y: number; w: number; h: number;
  insight?: SavedInsight;
}

export interface Dashboard {
  id: string; token: string; name: string; tiles: DashboardTile[];
  created_by: string; created_at: number;
}
