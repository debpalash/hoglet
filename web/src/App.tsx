import { useEffect, useState, useCallback } from "react";
import { api } from "./api";
import type { EventCount, RecentEvent, FunnelStep, TrendPoint, FlagDef, Stats } from "./api";
import type { SavedInsight, Dashboard } from "./api";

type Tab = "overview" | "funnels" | "trends" | "flags" | "insights" | "dashboards";
const TABS: Tab[] = ["overview", "funnels", "trends", "flags", "insights", "dashboards"];

type AuthState = "loading" | "setup" | "login" | "ready";
interface UserInfo { id: string; email: string; name: string }

export function App() {
  const [auth, setAuth] = useState<AuthState>("loading");
  const [user, setUser] = useState<UserInfo | null>(null);
  const [token, setToken] = useState("phc_demo");
  const [tab, setTab] = useState<Tab>("overview");

  const checkAuth = useCallback(async () => {
    const resp = await api.me();
    if (resp === null) { setAuth("login"); return; }
    if (resp.user) { setUser(resp.user); setAuth("ready"); return; }
    try {
      const r = await fetch("/api/auth/setup", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({}) });
      if (r.status === 404) { setAuth("login"); } else { setAuth("setup"); }
    } catch { setAuth("login"); }
  }, []);

  useEffect(() => { checkAuth(); }, [checkAuth]);

  if (auth === "loading") return <div className="empty" style={{ margin: 40 }}>Loading…</div>;
  if (auth === "setup") return <Setup onDone={(u) => { setUser(u); setAuth("ready"); }} />;
  if (auth === "login") return <Login onDone={(u) => { setUser(u); setAuth("ready"); }} />;

  return (
    <>
      <header>
        <span className="hog">🦔</span>
        <h1>Hoglet</h1>
        <span className="tag">PostHog-compatible · one binary</span>
        <input value={token} spellCheck={false} onChange={(e) => setToken(e.target.value)} />
        <button className="logout" onClick={async () => { await api.logout(); setAuth("login"); setUser(null); }} title="Log out">
          {user?.email}
        </button>
      </header>
      <nav>
        {TABS.map((t) => (
          <button key={t} className={t === tab ? "active" : ""} onClick={() => setTab(t)}>
            {t[0].toUpperCase() + t.slice(1)}
          </button>
        ))}
      </nav>
      <main>
        {tab === "overview" && <Overview token={token} />}
        {tab === "funnels" && <Funnels token={token} />}
        {tab === "trends" && <Trends token={token} />}
        {tab === "flags" && <Flags token={token} />}
        {tab === "insights" && <Insights token={token} />}
        {tab === "dashboards" && <Dashboards token={token} />}
      </main>
      <footer>Hoglet · numbers you can trust</footer>
    </>
  );
}

function Setup({ onDone }: { onDone: (u: UserInfo) => void }) {
  const [email, setEmail] = useState(""); const [password, setPassword] = useState(""); const [org, setOrg] = useState(""); const [error, setError] = useState("");
  const submit = async () => { setError(""); const r = await api.setup(email, password, org); if (!r) { setError("Setup failed."); return; } onDone(r.user); };
  return (<div style={{ maxWidth:380, margin:"60px auto", padding:20 }}><h1 style={{ textAlign:"center" }}>Welcome to Hoglet</h1><p style={{ textAlign:"center", color:"var(--muted)" }}>First-run setup.</p>{error && <div style={{ color:"#e55", marginBottom:12 }}>{error}</div>}<input style={{ display:"block", width:"100%", marginBottom:8 }} placeholder="Email" value={email} onChange={e=>setEmail(e.target.value)} /><input style={{ display:"block", width:"100%", marginBottom:8 }} type="password" placeholder="Password" value={password} onChange={e=>setPassword(e.target.value)} /><input style={{ display:"block", width:"100%", marginBottom:12 }} placeholder="Organization name" value={org} onChange={e=>setOrg(e.target.value)} /><button style={{ width:"100%" }} onClick={submit}>Create account</button></div>);
}

function Login({ onDone }: { onDone: (u: UserInfo) => void }) {
  const [email, setEmail] = useState(""); const [password, setPassword] = useState(""); const [error, setError] = useState("");
  const submit = async () => { setError(""); const r = await api.login(email, password); if (!r) { setError("Invalid email or password."); return; } onDone(r.user); };
  return (<div style={{ maxWidth:380, margin:"60px auto", padding:20 }}><h1 style={{ textAlign:"center" }}>Log in</h1>{error && <div style={{ color:"#e55", marginBottom:12 }}>{error}</div>}<input style={{ display:"block", width:"100%", marginBottom:8 }} placeholder="Email" value={email} onChange={e=>setEmail(e.target.value)} /><input style={{ display:"block", width:"100%", marginBottom:12 }} type="password" placeholder="Password" value={password} onChange={e=>setPassword(e.target.value)} /><button style={{ width:"100%" }} onClick={submit}>Log in</button></div>);
}

function Overview({ token }: { token: string }) {
  const [stats, setStats] = useState<Stats | null>(null); const [top, setTop] = useState<EventCount[]>([]); const [live, setLive] = useState<RecentEvent[]>([]);
  const load = useCallback(async () => { const [s, t, l] = await Promise.all([api.stats(token), api.topEvents(token, 8), api.recent(token, 25)]); if (s) setStats(s); if (t) setTop(t); if (l) setLive(l); }, [token]);
  useEffect(() => { load(); const id = setInterval(load, 2000); return () => clearInterval(id); }, [load]);
  const epp = stats && stats.unique_persons > 0 ? (stats.total_events / stats.unique_persons).toFixed(1) : "–";
  const maxTop = Math.max(1, ...top.map((t) => t.count));
  return (<><div className="cards"><Card label="Total events" value={stats?.total_events} /><Card label="Unique persons" value={stats?.unique_persons} /><Card label="Last 24h" value={stats?.events_24h} /><Card label="Events / person" value={epp} raw /></div><div className="grid2"><section className="panel"><h2>Top events</h2>{top.length ? top.map((t) => (<div className="row" key={t.event}><span>{t.event}</span><div className="bar-wrap"><div className="bar" style={{ width: `${(t.count / maxTop) * 100}%` }} /></div><span className="n">{t.count.toLocaleString()}</span></div>)) : <div className="empty">No events yet.</div>}</section><section className="panel"><h2>Live stream</h2>{live.length ? live.map((e) => (<div className="ev" key={e.uuid}><span className="name">{e.event}</span><span className="did">{e.distinct_id}</span><span className="ts">{e.timestamp.slice(11, 19)}</span></div>)) : <div className="empty">Waiting for events…</div>}</section></div></>);
}

function Card({ label, value, raw }: { label: string; value?: number | string; raw?: boolean }) {
  const shown = value === undefined ? "–" : raw ? value : Number(value).toLocaleString();
  return (<div className="card"><div className="label">{label}</div><div className="value">{shown}</div></div>);
}

function Funnels({ token }: { token: string }) {
  const [input, setInput] = useState("signup, activate, purchase"); const [steps, setSteps] = useState<FunnelStep[]>([]); const [ran, setRan] = useState(false);
  const run = async () => { const names = input.split(",").map(s => s.trim()).filter(Boolean); if (!names.length) return; const data = await api.funnel(token, names); setSteps(data ?? []); setRan(true); };
  const first = steps[0]?.reached ?? 0;
  return (<section className="panel"><h2>Funnel builder</h2><div className="controls"><input style={{ flex:1, minWidth:280 }} value={input} onChange={e=>setInput(e.target.value)} placeholder="comma-separated event names" /><button onClick={run}>Run funnel</button></div>{steps.length ? steps.map((s,i) => { const p = first ? (s.reached/first)*100 : 0; const prev = i ? steps[i-1].reached : s.reached; const drop = prev ? 100-(s.reached/prev)*100 : 0; return (<div className="funnel-step" key={i}><div className="top"><span>{i+1}. {s.event}</span><span><span className="conv">{p.toFixed(0)}%</span>{i ? <span className="drop"> (−{drop.toFixed(0)}% from prev)</span> : null}</span></div><div className="fbar" style={{ width: `${Math.max(p, 2)}%` }}>{s.reached.toLocaleString()}</div></div>); }) : <div className="empty">{ran ? "No data for those steps." : "Enter steps and run."}</div>}</section>);
}

function Trends({ token }: { token: string }) {
  const [event, setEvent] = useState("$pageview"); const [days, setDays] = useState(30); const [data, setData] = useState<TrendPoint[] | null>(null);
  const plot = async () => setData((await api.trend(token, event, days)) ?? []);
  const W=900, H=240, pad=30; const max = Math.max(1, ...(data??[]).map(d=>d.count)); const bw = data?.length ? (W-pad*2)/data.length : 0;
  // A "MM-DD" label needs ~46px to stay readable, so only label every Nth bar —
  // at 30 and 90 days the axis is otherwise an unreadable smear.
  const labelEvery = bw ? Math.max(1, Math.ceil(46/bw)) : 1;
  return (<section className="panel"><h2>Trend</h2><div className="controls"><input value={event} onChange={e=>setEvent(e.target.value)} placeholder="event name" /><select value={days} onChange={e=>setDays(Number(e.target.value))}><option value={7}>7 days</option><option value={30}>30 days</option><option value={90}>90 days</option></select><button onClick={plot}>Plot</button></div>{data?.length ? (<svg viewBox={`0 0 ${W} ${H}`} width="100%"><text x={pad} y={16}>{event} — max {max}/day</text>{data.map((d,i) => { const h=(d.count/max)*(H-pad*2); const x=pad+i*bw; const y=H-pad-h; return (<g key={d.day}><rect x={x+2} y={y} width={bw-4} height={h} fill="var(--accent)" rx={2}><title>{d.day}: {d.count}</title></rect>{i%labelEvery===0 && <text x={x+bw/2} y={H-pad+14} textAnchor="middle">{d.day.slice(5)}</text>}</g>); })}</svg>) : <div className="empty">{data ? `No data for "${event}".` : "Pick an event and plot."}</div>}</section>);
}

function Flags({ token }: { token: string }) {
  const [flags, setFlags] = useState<FlagDef[] | null>(null);
  useEffect(() => { api.flags(token).then(setFlags); }, [token]);
  return (<section className="panel"><h2>Feature flags</h2>{flags?.length ? flags.map(f=>(<div className="flag" key={f.key}><span className="key">{f.key}{f.variants.length>0 && <span className="did"> {f.variants.map(v=>`${v.key} ${v.rollout}%`).join(" · ")}</span>}</span><span className={`pill ${f.active?"on":"off"}`}>{f.active?"active":"inactive"}</span><span className="rollout">{f.rollout_percentage}%</span></div>)) : <div className="empty">No flags defined.</div>}</section>);
}

// ── Insight builder ──────────────────────────────

function makeQuery(kind: string, eventName: string) {
  return {
    kind,
    series: [{ event: { type: "name", value: eventName }, math: { type: "total" as any } }],
    filters: { op: "AND", values: [] },
    range: { from: null, to: null, last_n: null },
    interval: "Day",
    formulas: [],
  };
}

function Insights({ token }: { token: string }) {
  const [insights, setInsights] = useState<SavedInsight[]>([]);
  const [dashboards, setDashboards] = useState<Dashboard[]>([]);
  const [name, setName] = useState("");
  const [eventName, setEventName] = useState("$pageview");
  const [result, setResult] = useState<any>(null);
  const [editing, setEditing] = useState(false);

  const load = async () => {
    const [ir, dr] = await Promise.all([
      fetch(`/api/insights?token=${encodeURIComponent(token)}`),
      fetch(`/api/dashboards?token=${encodeURIComponent(token)}`),
    ]);
    if (ir.ok) setInsights(await ir.json());
    if (dr.ok) setDashboards(await dr.json());
  };
  useEffect(() => { load(); }, [token]);

  const run = async () => {
    const q = makeQuery("Trends", eventName);
    const r = await fetch("/api/query", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ token, query: q }) });
    if (r.ok) setResult(await r.json());
  };

  const save = async () => {
    if (!name.trim()) return;
    const q = makeQuery("Trends", eventName);
    const r = await fetch("/api/insights", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ token, name, query_ir: q }) });
    if (r.ok) { setName(""); setEditing(false); load(); }
  };

  const pinToDashboard = async (insightId: string, dashboardId: string) => {
    const dash = dashboards.find(d => d.id === dashboardId);
    if (!dash) return;
    const tiles = [...(dash.tiles || []), { insight_id: insightId, x: 0, y: (dash.tiles || []).length * 3, w: 4, h: 3 }];
    await fetch(`/api/dashboards/${dashboardId}/tiles`, { method: "PUT", headers: { "Content-Type": "application/json" }, body: JSON.stringify(tiles) });
    load();
  };

  return (<section className="panel">
    <h2>Insights</h2>
    <div style={{ marginBottom: 16 }}>
      <div style={{ display: "flex", gap: 8, alignItems: "center", marginBottom: 8 }}>
        <input placeholder="Event name" value={eventName} onChange={e => setEventName(e.target.value)} style={{ width: 200 }} />
        <button onClick={run}>Run query</button>
        <button onClick={() => setEditing(true)} disabled={editing}>Save as insight…</button>
      </div>
      {editing && (
        <div style={{ display: "flex", gap: 8 }}>
          <input placeholder="Insight name" value={name} onChange={e => setName(e.target.value)} autoFocus />
          <button onClick={save}>Save</button>
          <button onClick={() => setEditing(false)}>Cancel</button>
        </div>
      )}
      {result && result.results && (
        <div style={{ marginTop: 8, padding: 12, background: "var(--bg-card)", borderRadius: 8 }}>
          {result.results.map((r: any, i: number) => (
            <div key={i} style={{ marginBottom: 6 }}>
              <strong>{r.label}</strong>: {r.data?.slice(0, 5).map((d: any) => `${d.interval}: ${d.count.toLocaleString()}`).join(", ")}
              {(r.data?.length ?? 0) > 5 ? ` ... and ${r.data.length - 5} more` : ""}
            </div>
          ))}
          <div style={{ fontSize: 11, color: "var(--muted)", marginTop: 4 }}>
            {result.meta?.elapsed_ms}ms {result.meta?.cached ? "(cached)" : ""}
          </div>
        </div>
      )}
    </div>
    <h3>Saved ({insights.length})</h3>
    {insights.map(i => (
      <div key={i.id} className="row" style={{ padding: "6px 0", flexWrap: "wrap" }}>
        <span style={{ fontWeight: 600 }}>{i.name}</span>
        <span className="did" style={{ marginLeft: 8 }}>{i.description}</span>
        {dashboards.length > 0 && (
          <select
            style={{ marginLeft: 12, fontSize: 12 }}
            value=""
            onChange={async (e) => { if (e.target.value) { await pinToDashboard(i.id, e.target.value); } }}
          >
            <option value="">Pin to…</option>
            {dashboards.map(d => (<option key={d.id} value={d.id}>{d.name}</option>))}
          </select>
        )}
        <button style={{ marginLeft: "auto" }} onClick={async () => { await fetch(`/api/insights/${i.id}`, { method: "DELETE" }); load(); }}>Del</button>
      </div>
    ))}
  </section>);
}

// ── Dashboards ───────────────────────────────────

function Dashboards({ token }: { token: string }) {
  const [dashboards, setDashboards] = useState<Dashboard[]>([]);
  const [name, setName] = useState("");

  const load = async () => {
    const r = await fetch(`/api/dashboards?token=${encodeURIComponent(token)}`);
    if (r.ok) setDashboards(await r.json());
  };
  useEffect(() => { load(); }, [token]);

  const create = async () => {
    if (!name.trim()) return;
    await fetch("/api/dashboards", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ token, name }) });
    setName(""); load();
  };

  return (<section className="panel">
    <h2>Dashboards</h2>
    <div className="controls" style={{ marginBottom: 12 }}>
      <input placeholder="Dashboard name" value={name} onChange={e => setName(e.target.value)} />
      <button onClick={create}>Create</button>
    </div>
    {dashboards.map(d => (
      <div key={d.id} style={{ background: "var(--bg-card)", borderRadius: 8, padding: 12, marginBottom: 8 }}>
        <div style={{ display: "flex", alignItems: "center" }}>
          <strong>{d.name}</strong>
          <span className="did" style={{ marginLeft: 8 }}>{d.tiles?.length ?? 0} tiles</span>
          <button onClick={async () => { await fetch(`/api/dashboards/${d.id}`, { method: "DELETE" }); load(); }} style={{ marginLeft: "auto", color: "#e55" }}>Del</button>
        </div>
        {d.tiles?.length ? (<div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fill, minmax(200px, 1fr))", gap: 8, marginTop: 8 }}>
          {d.tiles.map(t => (
            <div key={t.insight_id} style={{ background: "var(--bg)", borderRadius: 6, padding: 8, border: "1px solid var(--border)" }}>
              <div style={{ fontSize: 12, color: "var(--muted)" }}>{t.insight?.name ?? t.insight_id.slice(0, 8)}</div>
            </div>
          ))}
        </div>) : <div className="empty" style={{ marginTop: 8 }}>No tiles. Add tiles from the Insights tab.</div>}
      </div>
    ))}
  </section>);
}
