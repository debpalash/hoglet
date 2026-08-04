# Auth & Accounts — Implementation Spec

P4. Required before anyone runs Hoglet for real — currently the dashboard and
all `/api/*` endpoints are wide open. Capture edge stays token-authed.

Status: ◐ implementing
Exit: fresh install forces account creation; anonymous /api/* is 401;
      admin API absorbed into role=admin.

---

## 1. Data model

All in SQLite. Single writer (one auth store behind a mutex).

```sql
CREATE TABLE users (
    id TEXT NOT NULL PRIMARY KEY,       -- UUIDv7
    email TEXT NOT NULL UNIQUE,
    pw_hash TEXT NOT NULL,              -- argon2id, 64-char hex
    name TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);

CREATE TABLE orgs (
    id TEXT NOT NULL PRIMARY KEY,
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE org_members (
    org_id TEXT NOT NULL REFERENCES orgs(id),
    user_id TEXT NOT NULL REFERENCES users(id),
    role TEXT NOT NULL DEFAULT 'member', -- owner | admin | member
    PRIMARY KEY (org_id, user_id)
);

CREATE TABLE projects (
    id TEXT NOT NULL PRIMARY KEY,
    org_id TEXT NOT NULL REFERENCES orgs(id),
    name TEXT NOT NULL,
    token TEXT NOT NULL UNIQUE,         -- phc_... project write token
    created_at INTEGER NOT NULL
);

CREATE TABLE personal_api_keys (
    id TEXT NOT NULL PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    key_hash TEXT NOT NULL,             -- sha256 hex
    key_prefix TEXT NOT NULL,           -- phx_... (first 8 chars)
    last_used INTEGER,
    created_at INTEGER NOT NULL
);

CREATE TABLE sessions (
    id TEXT NOT NULL PRIMARY KEY,       -- UUIDv4
    user_id TEXT NOT NULL REFERENCES users(id),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL         -- 7 days
);
```

Sessions: server-side, SameSite=Lax, HttpOnly, Path=/. No JWT — single-node
makes session IDs sufficient. 7-day expiry. On logout, delete the row.

---

## 2. First-run flow

When `users` table is empty, the first request to any `/api/*` or `/dashboard`
is redirected to `/setup`. `/setup` presents a form: email + password + org
name. On submit:

1. Create user (owner)
2. Create org
3. Create org_members row (role=owner)
4. Create a project for the org (name = "Default", token = phc_...)
5. Create session, set cookie
6. Redirect to /dashboard

After first-run, `/setup` returns 404.

---

## 3. Login flow

`POST /api/auth/login { email, password }`

1. Look up user by email
2. Verify argon2 hash
3. Create session, set cookie
4. Return 200 `{ user: { id, email, name }, orgs: [...] }`

`POST /api/auth/logout` — delete session, clear cookie, 200.

`GET /api/auth/me` — return current user + orgs (from session cookie or
personal API key header). 401 if unauthenticated.

---

## 4. Auth middleware

Two layers:

### Capture edge (unchanged)
`/e`, `/batch`, `/capture`, `/track`, `/engage`, `/i/v0/e` — token-authed.
No session required. These are public by nature (SDKs send them).

### Dashboard + API edge
Everything under `/api/*` and `/dashboard` requires auth. Two auth methods:

1. **Session cookie** (`hoglet_sid`). Check `sessions` table.
2. **Personal API key** (`Authorization: Bearer phx_...`). SHA256-hash the key,
   look up in `personal_api_keys`. Rate-limited (100 req/min).

The middleware runs on every request to `/api/*` (except `/api/auth/login` and
`/api/auth/setup` and the capture aliases). On 401, return JSON `{ error:
"unauthorized" }` for API paths, redirect to `/login` for page paths.

Admin endpoints (`/api/admin/*`) additionally check `role = owner | admin`.

### Middleware architecture

```rust
// src/auth.rs (new module, not middleware — just the store)
pub struct AuthStore { conn: Mutex<Connection> }

// Apply as an axum middleware layer:
async fn auth_middleware(
    State(auth): State<Arc<AuthStore>>,
    cookies: Cookies,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    // Try session cookie first, then API key header
    // If neither works and path is /api/auth/login or /api/auth/setup, pass through
    // If neither works, 401
}
```

---

## 5. API key management

`GET /api/auth/keys` — list personal API keys for current user (prefix only,
never the full key).

`POST /api/auth/keys { name }` — create a new personal API key. Return the
full key exactly once (it cannot be retrieved again). Store sha256 hash.

`DELETE /api/auth/keys/:id` — revoke a key.

Keys start with `phx_` (already rejected by capture token validation, so
personal keys can't accidentally be used as project tokens).

---

## 6. Org & project CRUD

`GET /api/orgs` — list orgs for current user.

`POST /api/orgs { name }` — create org (user becomes owner).

`GET /api/orgs/:org_id/projects` — list projects in org.

`POST /api/orgs/:org_id/projects { name }` — create project (generates token).

`DELETE /api/orgs/:org_id` — org owner only.

`POST /api/orgs/:org_id/members { email, role }` — invite by email (just
adds; email-based invites are post-v1).

`DELETE /api/orgs/:org_id/members/:user_id` — remove member (owner/admin).

---

## 7. Admin API migration

Current `/api/admin/*` endpoints are gated by `HOGLET_ADMIN_TOKEN`. After P4
lands:

- `/api/admin/projects` → absorbed into `/api/orgs/:org_id/projects` (POST)
- `/api/admin/flags` → absorbed into `/api/flags` (POST/DELETE) — flag
  management requires role=admin on the project's org
- `/api/admin/forget` → absorbed into `/api/orgs/:org_id/gdpr` (POST) —
  GDPR erasure requires role=owner

`HOGLET_ADMIN_TOKEN` is deprecated but still works for backward compat.
New installs ignore it.

---

## 8. Module layout

```
src/
  auth.rs              → AuthStore: users, orgs, org_members, projects,
                          sessions, personal_api_keys
  middleware/
    mod.rs              → re-exports
    auth.rs             → auth_middleware: session cookie + API key check
  routes/
    auth.rs             → login, logout, setup, me, keys
    orgs.rs             → org + project CRUD
  lib.rs                → updated: auth store, middleware layer
  main.rs               → updated: first-run detection, admin token deprecation
```

---

## 9. Dashboard changes

- `web/src/App.tsx` — check auth state on load (GET /api/auth/me). If 401,
  render `<LoginPage>`.
- `web/src/Login.tsx` — email + password form, POST /api/auth/login.
- `web/src/Setup.tsx` — first-run form: email + password + org name,
  POST /api/auth/setup.
- Token/org context: after login, store current org + project in React state.
  All API calls include the token.

No routing library — inline conditionals in App.tsx. Keep it simple.

---

## 10. Invariants

1. **Password hashes never leave the server.** Only argon2id hashes stored.
2. **Personal API keys shown exactly once.** On creation, return the full key.
   Subsequent GETs return only the prefix.
3. **Capture edge never gated.** `/e`, `/batch`, etc. remain token-authed only.
4. **Session IDs are random (UUIDv4).** Not predictable.
5. **First-run creates owner.** The first user to sign up owns the first org.
   No second first-run.
6. **No JWT.** Single-node, server-side sessions are sufficient. No key rotation
   needed.

---

## 11. Exit criteria

1. Fresh install: visiting `/dashboard` redirects to `/setup`.
2. After setup: `/dashboard` shows the app; `/api/query` requires auth.
3. Login/logout works: POST /api/auth/login → cookie set → /api/auth/me works.
4. Personal API key can be created, used in Authorization header, and revoked.
5. Capture endpoints still work without auth (token-based only).
6. Org owner can create projects and invite members.
7. Admin API absorbed into org-scoped endpoints.
8. All existing tests (178) continue to pass.
9. New tests: auth roundtrip (create user → login → session → me → logout),
   API key roundtrip, first-run flow, unauthorized rejection.

---

## 12. What P4 does NOT do

- Email verification, password reset, MFA (post-v1).
- Email-based invitations (just direct member-add by email lookup).
- OAuth / SSO (post-v1).
- Rate limiting on login attempts (post-v1).
- Account deletion (post-v1).
- Project tokens scoped by event type (all tokens are write-all).
