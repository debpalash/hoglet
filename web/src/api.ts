// Typed client for Hoglet's dashboard API. Project IDs scope every primary
// analytics request; capture tokens only exist in the explicitly legacy client.

import type { Stats } from "./types/Stats";
import type { EventCount } from "./types/EventCount";
import type { TrendPoint } from "./types/TrendPoint";
import type { FunnelStep } from "./types/FunnelStep";
import type { RecentEvent } from "./types/RecentEvent";
import type { FlagDef } from "./types/FlagDef";
import type { Query } from "./types/Query";
import type { QueryResponse } from "./types/QueryResponse";

export type ApiError =
  | {
      kind: "http";
      status: number;
      code?: string;
      message: string;
      field?: string;
      requestId?: string;
    }
  | {
      kind: "network";
      message: string;
    }
  | {
      kind: "decode";
      status: number;
      message: string;
      requestId?: string;
    };

interface ErrorEnvelope {
  error?: {
    code?: string;
    message?: string;
    field?: string;
    request_id?: string;
  };
}

let sessionExpiredCallback: (() => void) | undefined;
let sessionExpiredNotified = false;
let sessionGeneration = 0;

/**
 * Installs the one application-wide expired-session listener. The returned
 * function only removes the listener when it is still the active one.
 */
export function setSessionExpiredCallback(callback?: () => void): () => void {
  sessionExpiredCallback = callback;
  sessionExpiredNotified = false;
  return () => {
    if (sessionExpiredCallback === callback) {
      sessionExpiredCallback = undefined;
      sessionExpiredNotified = false;
    }
  };
}

export function isApiError(error: unknown): error is ApiError {
  if (typeof error !== "object" || error === null || !("kind" in error)) {
    return false;
  }
  const kind = (error as { kind?: unknown }).kind;
  return kind === "http" || kind === "network" || kind === "decode";
}

function notifySessionExpired(requestGeneration: number): void {
  if (
    requestGeneration !== sessionGeneration ||
    sessionExpiredNotified ||
    !sessionExpiredCallback
  ) {
    return;
  }
  sessionExpiredNotified = true;
  const callback = sessionExpiredCallback;
  queueMicrotask(callback);
}

function markSessionActive(): void {
  sessionGeneration += 1;
  sessionExpiredNotified = false;
}

function requestId(response: Response): string | undefined {
  return response.headers.get("x-request-id") ?? undefined;
}

async function readBody(response: Response): Promise<string> {
  try {
    return await response.text();
  } catch {
    throw {
      kind: "network",
      message: "The response was interrupted before it finished.",
    } satisfies ApiError;
  }
}

function parseErrorBody(text: string): ErrorEnvelope | undefined {
  if (!text) return undefined;
  try {
    return JSON.parse(text) as ErrorEnvelope;
  } catch {
    return undefined;
  }
}

/** Performs one JSON request and throws a discriminated ApiError on failure. */
export async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  const requestGeneration = sessionGeneration;
  let response: Response;
  try {
    const headers = new Headers(init.headers);
    if (!headers.has("Accept")) headers.set("Accept", "application/json");
    response = await fetch(path, {
      credentials: "same-origin",
      ...init,
      headers,
    });
  } catch (error) {
    throw {
      kind: "network",
      message:
        error instanceof DOMException && error.name === "AbortError"
          ? "The request was cancelled."
          : "Hoglet could not be reached.",
    } satisfies ApiError;
  }

  const text = await readBody(response);
  if (!response.ok) {
    if (response.status === 401) notifySessionExpired(requestGeneration);
    const body = parseErrorBody(text)?.error;
    throw {
      kind: "http",
      status: response.status,
      code: body?.code,
      message: body?.message ?? (response.statusText || `Request failed (${response.status}).`),
      field: body?.field,
      requestId: body?.request_id ?? requestId(response),
    } satisfies ApiError;
  }

  if (response.status === 204 || text.length === 0) return undefined as T;
  try {
    return JSON.parse(text) as T;
  } catch {
    throw {
      kind: "decode",
      status: response.status,
      message: "Hoglet returned a response the dashboard could not decode.",
      requestId: requestId(response),
    } satisfies ApiError;
  }
}

function jsonInit(method: string, body: unknown, signal?: AbortSignal): RequestInit {
  return {
    method,
    signal,
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  };
}

function pathSegment(value: string): string {
  return encodeURIComponent(value);
}

function withQuery(path: string, values: Record<string, string | number | boolean | undefined>): string {
  const parameters = new URLSearchParams();
  for (const [key, value] of Object.entries(values)) {
    if (value !== undefined) parameters.set(key, String(value));
  }
  const query = parameters.toString();
  return query ? `${path}?${query}` : path;
}

function projectPath(projectId: string, resource: string): string {
  return `/api/projects/${pathSegment(projectId)}/${resource}`;
}

export type Role = "owner" | "admin" | "member";

export interface User {
  id: string;
  email: string;
  name: string;
}

export interface Project {
  id: string;
  name: string;
  token: string;
}

export interface Organization {
  id: string;
  name: string;
  role: Role;
  projects: Project[];
}

export interface Workspace {
  user: User;
  organizations: Organization[];
}

export interface Bootstrap {
  setup_required: boolean;
}

export interface SetupInput {
  email: string;
  password: string;
  organization_name: string;
  project_name?: string;
  existing_project_token?: string;
}

export interface LoginInput {
  email: string;
  password: string;
}

export interface PersonalApiKey {
  id: string;
  name: string;
  key_prefix: string;
  last_used: number | null;
  created_at: number;
}

export interface CreatedPersonalApiKey {
  key: PersonalApiKey;
  secret: string;
}

export interface PropertyKey {
  key: string;
  source: string;
  type_guess: string;
  count: number;
}

export interface SaveInsightInput {
  name: string;
  description?: string;
  query_ir: Query;
}

export interface SavedInsight {
  id: string;
  project_id: string;
  name: string;
  description: string;
  query_ir: Query;
  created_by: string;
  created_at: number;
  updated_at: number;
}

export interface DashboardTile {
  insight_id: string;
  x: number;
  y: number;
  w: number;
  h: number;
  insight?: SavedInsight;
}

export interface Dashboard {
  id: string;
  project_id: string;
  name: string;
  tiles: DashboardTile[];
  created_by: string;
  created_at: number;
}

export interface SaveDashboardInput {
  name: string;
}

export const api = {
  bootstrap: (signal?: AbortSignal) => request<Bootstrap>("/api/auth/bootstrap", { signal }),

  setup: async (input: SetupInput, signal?: AbortSignal) => {
    const workspace = await request<Workspace>("/api/auth/setup", jsonInit("POST", input, signal));
    markSessionActive();
    return workspace;
  },

  login: async (input: LoginInput, signal?: AbortSignal) => {
    const workspace = await request<Workspace>("/api/auth/login", jsonInit("POST", input, signal));
    markSessionActive();
    return workspace;
  },

  logout: async (signal?: AbortSignal) => {
    const result = await request<{ status: "ok" }>("/api/auth/logout", {
      method: "POST",
      signal,
    });
    markSessionActive();
    return result;
  },

  me: async (signal?: AbortSignal) => {
    const workspace = await request<Workspace>("/api/auth/me", { signal });
    markSessionActive();
    return workspace;
  },

  listKeys: (signal?: AbortSignal) =>
    request<PersonalApiKey[]>("/api/auth/keys", { signal }),
  createKey: (name: string, signal?: AbortSignal) =>
    request<CreatedPersonalApiKey>("/api/auth/keys", jsonInit("POST", { name }, signal)),
  revokeKey: (keyId: string, signal?: AbortSignal) =>
    request<void>(`/api/auth/keys/${pathSegment(keyId)}`, { method: "DELETE", signal }),

  listOrganizations: (signal?: AbortSignal) =>
    request<Organization[]>("/api/organizations", { signal }),
  createOrganization: (name: string, signal?: AbortSignal) =>
    request<Organization>("/api/organizations", jsonInit("POST", { name }, signal)),
  createProject: (organizationId: string, name: string, signal?: AbortSignal) =>
    request<Project>(
      `/api/organizations/${pathSegment(organizationId)}/projects`,
      jsonInit("POST", { name }, signal),
    ),

  runQuery: (projectId: string, query: Query, refresh = false, signal?: AbortSignal) =>
    request<QueryResponse>(
      projectPath(projectId, "query"),
      jsonInit("POST", { query, refresh }, signal),
    ),

  catalogEvents: (projectId: string, limit = 200, signal?: AbortSignal) =>
    request<string[]>(withQuery(projectPath(projectId, "catalog/events"), { limit }), { signal }),
  catalogProperties: (
    projectId: string,
    source: "event" | "person" = "event",
    signal?: AbortSignal,
  ) =>
    request<PropertyKey[]>(withQuery(projectPath(projectId, "catalog/properties"), { source }), {
      signal,
    }),
  catalogValues: (projectId: string, key: string, limit = 50, signal?: AbortSignal) =>
    request<string[]>(withQuery(projectPath(projectId, "catalog/values"), { key, limit }), {
      signal,
    }),

  listFlags: (projectId: string, signal?: AbortSignal) =>
    request<FlagDef[]>(projectPath(projectId, "flags"), { signal }),
  createFlag: (projectId: string, flag: FlagDef, signal?: AbortSignal) =>
    request<FlagDef>(projectPath(projectId, "flags"), jsonInit("POST", flag, signal)),
  updateFlag: (projectId: string, key: string, flag: FlagDef, signal?: AbortSignal) =>
    request<FlagDef>(
      `${projectPath(projectId, "flags")}/${pathSegment(key)}`,
      jsonInit("PUT", flag, signal),
    ),
  deleteFlag: (projectId: string, key: string, signal?: AbortSignal) =>
    request<void>(`${projectPath(projectId, "flags")}/${pathSegment(key)}`, {
      method: "DELETE",
      signal,
    }),

  listInsights: (projectId: string, signal?: AbortSignal) =>
    request<SavedInsight[]>(projectPath(projectId, "insights"), { signal }),
  getInsight: (projectId: string, insightId: string, signal?: AbortSignal) =>
    request<SavedInsight>(
      `${projectPath(projectId, "insights")}/${pathSegment(insightId)}`,
      { signal },
    ),
  saveInsight: (projectId: string, insight: SaveInsightInput, signal?: AbortSignal) =>
    request<SavedInsight>(
      projectPath(projectId, "insights"),
      jsonInit("POST", insight, signal),
    ),
  deleteInsight: (projectId: string, insightId: string, signal?: AbortSignal) =>
    request<void>(`${projectPath(projectId, "insights")}/${pathSegment(insightId)}`, {
      method: "DELETE",
      signal,
    }),

  listDashboards: (projectId: string, signal?: AbortSignal) =>
    request<Dashboard[]>(projectPath(projectId, "dashboards"), { signal }),
  getDashboard: (projectId: string, dashboardId: string, signal?: AbortSignal) =>
    request<Dashboard>(
      `${projectPath(projectId, "dashboards")}/${pathSegment(dashboardId)}`,
      { signal },
    ),
  saveDashboard: (projectId: string, dashboard: SaveDashboardInput, signal?: AbortSignal) =>
    request<Dashboard>(
      projectPath(projectId, "dashboards"),
      jsonInit("POST", dashboard, signal),
    ),
  deleteDashboard: (projectId: string, dashboardId: string, signal?: AbortSignal) =>
    request<void>(`${projectPath(projectId, "dashboards")}/${pathSegment(dashboardId)}`, {
      method: "DELETE",
      signal,
    }),
  updateDashboardTiles: (
    projectId: string,
    dashboardId: string,
    tiles: DashboardTile[],
    signal?: AbortSignal,
  ) =>
    request<Dashboard>(
      `${projectPath(projectId, "dashboards")}/${pathSegment(dashboardId)}/tiles`,
      jsonInit("PUT", tiles, signal),
    ),
};

// Temporary compatibility surface for the pre-workspace dashboard. New UI code
// must use `api` so authorization is based on a project ID, never a raw token.
const legacyToken = (token: string): string => token.trim() || "phc_demo";

export const legacyApi = {
  stats: (token: string, signal?: AbortSignal) =>
    request<Stats>(withQuery("/api/stats", { token: legacyToken(token) }), { signal }),
  topEvents: (token: string, limit = 8, signal?: AbortSignal) =>
    request<EventCount[]>(withQuery("/api/top_events", { token: legacyToken(token), limit }), {
      signal,
    }),
  recent: (token: string, limit = 25, signal?: AbortSignal) =>
    request<RecentEvent[]>(withQuery("/api/recent", { token: legacyToken(token), limit }), {
      signal,
    }),
  trend: (token: string, event: string, days: number, signal?: AbortSignal) =>
    request<TrendPoint[]>(withQuery("/api/trend", { token: legacyToken(token), event, days }), {
      signal,
    }),
  flags: (token: string, signal?: AbortSignal) =>
    request<FlagDef[]>(withQuery("/api/flags", { token: legacyToken(token) }), { signal }),
  funnel: (token: string, steps: string[], signal?: AbortSignal) =>
    request<FunnelStep[]>(
      "/api/funnel",
      jsonInit("POST", { token: token.trim(), steps }, signal),
    ),
  catalogEvents: (token: string, limit = 200, signal?: AbortSignal) =>
    request<string[]>(withQuery("/api/catalog/events", { token: legacyToken(token), limit }), {
      signal,
    }),
  catalogProperties: (
    token: string,
    source: "event" | "person" = "event",
    signal?: AbortSignal,
  ) =>
    request<PropertyKey[]>(
      withQuery("/api/catalog/properties", { token: legacyToken(token), source }),
      { signal },
    ),
  catalogValues: (token: string, key: string, limit = 50, signal?: AbortSignal) =>
    request<Array<{ value: string; count: number }>>(
      withQuery("/api/catalog/values", { token: legacyToken(token), key, limit }),
      { signal },
    ),
  runQuery: (token: string, query: Query, refresh = false, signal?: AbortSignal) =>
    request<QueryResponse>(
      "/api/query",
      jsonInit("POST", { token: token.trim(), query, refresh }, signal),
    ),
};

export type {
  Stats,
  EventCount,
  TrendPoint,
  FunnelStep,
  RecentEvent,
  FlagDef,
  Query,
  QueryResponse,
};
