# Dashboards, Saved Insights, Sharing — Implementation Spec

P6. The product surface: build insights, save them, pin them to dashboards,
share with a link. The backend is straightforward CRUD on SQLite; the frontend
is an IR editor that talks to the catalog for autocomplete.

Status: ◐ implementing
Exit: build → save → pin → share loop works end to end from the dashboard.

---

## 1. Data model

```sql
CREATE TABLE saved_insights (
    id TEXT NOT NULL PRIMARY KEY,
    token TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    query_ir TEXT NOT NULL,            -- serialized Query IR JSON
    created_by TEXT NOT NULL,          -- user_id
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE dashboards (
    id TEXT NOT NULL PRIMARY KEY,
    token TEXT NOT NULL,
    name TEXT NOT NULL,
    layout_json TEXT NOT NULL DEFAULT '[]',  -- array of {i, x, y, w, h}
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE dashboard_tiles (
    id TEXT NOT NULL PRIMARY KEY,
    dashboard_id TEXT NOT NULL REFERENCES dashboards(id) ON DELETE CASCADE,
    insight_id TEXT NOT NULL REFERENCES saved_insights(id) ON DELETE CASCADE,
    grid_x INTEGER NOT NULL DEFAULT 0,
    grid_y INTEGER NOT NULL DEFAULT 0,
    grid_w INTEGER NOT NULL DEFAULT 4,
    grid_h INTEGER NOT NULL DEFAULT 3,
    PRIMARY KEY (dashboard_id, insight_id)
);

CREATE TABLE share_links (
    id TEXT NOT NULL PRIMARY KEY,
    object_type TEXT NOT NULL,          -- 'dashboard' | 'insight'
    object_id TEXT NOT NULL,
    token TEXT NOT NULL UNIQUE,         -- short random token for the URL
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER                  -- null = never expires
);

CREATE INDEX idx_insights_token ON saved_insights(token);
CREATE INDEX idx_dashboards_token ON dashboards(token);
CREATE INDEX idx_share_links_token ON share_links(token);
```

---

## 2. API

### Saved insights

`GET /api/insights?token=X` — list insights for a project.

`POST /api/insights` — create or update an insight.
```json
{
  "token": "phc_...",
  "id": null,
  "name": "Weekly signups",
  "description": "",
  "query_ir": { <Query IR> }
}
```
Returns the insight with its `id` set. If `id` is provided, update instead.

`GET /api/insights/:id` — get a single insight.

`DELETE /api/insights/:id` — delete.

### Dashboards

`GET /api/dashboards?token=X` — list dashboards for a project.

`POST /api/dashboards` — create or update.
```json
{
  "token": "phc_...",
  "id": null,
  "name": "Growth dashboard",
  "tiles": [
    { "insight_id": "...", "x": 0, "y": 0, "w": 4, "h": 3 }
  ]
}
```

`GET /api/dashboards/:id` — get dashboard with tiles + insight IRs embedded.

`DELETE /api/dashboards/:id` — delete.

`PUT /api/dashboards/:id/tiles` — replace tile layout.

### Sharing

`POST /api/share` — create a share link.
```json
{
  "object_type": "dashboard",
  "object_id": "..."
}
```
Returns `{ url: "/shared/abc123" }`.

`GET /shared/:token` — serves the shared dashboard or insight as a read-only
HTML page (server-rendered, no auth required). Embeds the IR directly so the
page loads with data pre-fetched.

`DELETE /api/share/:id` — revoke a share link.

---

## 3. Module layout

```
src/
  dashboard_store.rs    → DashboardStore (insights, dashboards, tiles, shares)
  routes/
    insights.rs          → insight CRUD
    dashboards.rs        → dashboard CRUD + share
    shared.rs            → GET /shared/:token
```

The `DashboardStore` is SQLite behind a Mutex, same pattern as every other
store.

---

## 4. Dashboard UI

The current dashboard has hardcoded tabs (Overview, Funnels, Trends, Flags).
P6 replaces this with:

**Insight builder** — a sidebar/modal that lets the user build an insight:
1. **Event picker** — dropdown fed by `GET /api/catalog/events?token=X&prefix=...`
2. **Math selector** — dropdown of Math variants (Total, DAU, etc.)
3. **Filter rows** — property key dropdown (from catalog), operator dropdown, value input (autocomplete from catalog)
4. **Breakdown selector** — property key dropdown
5. **Date range** — last N days picker
6. **Interval** — hour/day/week/month

On "Run", the IR is POSTed to `/api/query` and the result renders as a chart
or table. On "Save", the IR is POSTed to `/api/insights`.

**Dashboard grid** — a grid of tiles, each rendering a saved insight. Uses a
simple CSS grid (no drag-and-drop library — just fixed grid with edit mode
for repositioning). Each tile shows the insight name, a mini chart, and the
latest data point.

**Share dialog** — a modal with a copyable URL. The shared page renders the
dashboard with the same React components but without auth gates.

---

## 5. Shared page

`GET /shared/:token` does NOT serve the React SPA. It serves a minimal
server-rendered HTML page that:
1. Loads the shared object (dashboard or insight) from SQLite
2. Executes each insight's IR query via `POST /api/query` (internally, not HTTP)
3. Renders the results as a simple HTML page with inline charts (SVG bars)

This avoids building a separate SPA for shared views. The shared page is
self-contained and works without JavaScript.

Actually, for v1: the shared page redirects to the dashboard with a `?shared=`
param. The dashboard app detects the param and renders in read-only mode,
loading the shared object's data via the API (which exempts shared tokens
from auth in the middleware). This reuses the existing React app.

---

## 6. Auth integration

Shared objects are exempt from auth. The middleware checks:
- Normal `/api/*` → requires session or API key
- `/api/share/*` → requires session or API key
- `/shared/*` and `/api/query?shared_token=...` → no auth required (the share token IS the auth)

Add `shared_token` as an accepted auth method in the middleware alongside
session cookies and API keys.

---

## 7. Invariants

1. **IR is the source of truth.** A saved insight is a serialized Query IR.
   The frontend builds it, sends it to `/api/query` to run, and sends it to
   `/api/insights` to save.
2. **Tiles reference insights, not inline IR.** A dashboard tile always points
   to a saved insight. Deleting an insight cascades to remove its tiles.
3. **Share tokens are random and unguessable** (UUIDv4, 32 hex chars).
4. **No foreign keys to users table** — insights and dashboards reference
   user_id as a free-text field. Auth store might not be configured.
5. **Shared pages never leak project tokens.** The shared token resolves to
   the underlying insight/dashboard; the project token is looked up server-side.

---

## 8. Exit criteria

1. Create an insight via the builder, save it, see it in the list.
2. Re-open a saved insight, edit it, re-save.
3. Create a dashboard, pin 2+ insights to it.
4. Share a dashboard, open the shared link, see the tiles rendered.
5. Delete an insight, verify tiles are cleaned up.
6. All existing tests (187) continue to pass.

---

## 9. What P6 does NOT do

- Drag-and-drop dashboard layout (edit mode with position inputs only).
- Insight comments, tags, favorites.
- Dashboard templates.
- Scheduled dashboard email reports.
- Real-time dashboard updates (polling is sufficient, already implemented).
- Shared insight embedding (iframe).
