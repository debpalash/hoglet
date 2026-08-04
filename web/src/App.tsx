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

const KINDS = ["Trends", "Funnels", "Retention", "Lifecycle", "Stickiness", "Actors"] as const;
const MATHS = ["total", "dau", "wau", "mau", "unique_sessions", "first_time"] as const;
const OPERATORS = ["exact", "not_equal", "icontains", "not_contains", "is_set", "is_not_set", "gt", "lt"] as const;
const RANGES: [string, number][] = [["7 days", 7], ["30 days", 30], ["90 days", 90]];

type FilterRow = { key: string; op: string; value: string };

/** Build the IR the backend expects from the builder's current state. */
function buildQuery(kind: string, events: string[], math: string, days: number,
                    interval: string, filters: FilterRow[], breakdown: string) {
  const valued = (op: string) => !["is_set", "is_not_set"].includes(op);
  return {
    kind,
    series: events.map(e => ({ event: { type: "name", value: e }, math: { type: math } })),
    filters: {
      op: "AND",
      // FilterOperator is adjacently tagged with only unit variants, so it is
      // `{op}` alone — the operand rides on the filter's own `value` field.
      values: filters.filter(f => f.key).map(f => ({
        type: "filter",
        source: "event",
        key: f.key,
        operator: { op: f.op },
        value: valued(f.op) ? f.value : null,
      })),
    },
    breakdown: breakdown ? { source: "event", key: breakdown, limit: 10 } : null,
    range: { from: null, to: null, last_n: { unit: "d", value: days } },
    interval,
    formulas: [],
    // Funnels and Retention need their config or the compiler rejects them.
    funnel_config: kind === "Funnels"
      ? { order_type: "Ordered", conversion_window_seconds: null, exclusions: [], attribution: "AllSteps" }
      : null,
    retention_config: kind === "Retention"
      ? { cohort_event: { type: "name", value: events[0] ?? "" },
          retention_event: { type: "name", value: events[1] ?? events[0] ?? "" },
          retention_type: "Recurring", period: "Day", total_periods: 7 }
      : null,
    lifecycle_config: kind === "Lifecycle"
      ? { event: { type: "name", value: events[0] ?? "" }, prior_period: "Day" } : null,
    stickiness_config: kind === "Stickiness"
      ? { event: { type: "name", value: events[0] ?? "" }, window_days: days } : null,
    actors_config: kind === "Actors"
      ? { series_index: 0, day: "", offset: 0, limit: 100 } : null,
    sql_config: null,
  };
}

/** The insight builder — an editor for the query IR.
 *
 *  Every control here maps to one IR field, and the IR it produces is exactly
 *  what gets saved: the saved-insight format and the query format are the same
 *  object, so anything buildable is savable and vice versa. */
function InsightBuilder({ token, onSave }: { token: string; onSave: (ir: any, name: string) => void }) {
  const [kind, setKind] = useState<string>("Trends");
  const [events, setEvents] = useState<string[]>(["$pageview"]);
  const [math, setMath] = useState<string>("total");
  const [days, setDays] = useState(30);
  const [interval, setInterval] = useState("Day");
  const [filters, setFilters] = useState<FilterRow[]>([]);
  const [breakdown, setBreakdown] = useState("");
  const [result, setResult] = useState<any>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [name, setName] = useState("");

  const [eventNames, setEventNames] = useState<string[]>([]);
  const [propKeys, setPropKeys] = useState<string[]>([]);
  const [valueHints, setValueHints] = useState<Record<string, string[]>>({});

  useEffect(() => {
    api.catalogEvents(token).then(e => setEventNames(e ?? []));
    api.catalogProperties(token).then(p => setPropKeys((p ?? []).map(k => k.key)));
  }, [token]);

  // Pull value suggestions lazily, once per property actually used in a filter.
  useEffect(() => {
    for (const f of filters) {
      if (f.key && !(f.key in valueHints)) {
        setValueHints(v => ({ ...v, [f.key]: [] }));
        api.catalogValues(token, f.key).then(vs =>
          setValueHints(v => ({ ...v, [f.key]: (vs ?? []).map(x => x.value) })));
      }
    }
  }, [filters, token, valueHints]);

  const multiEvent = kind === "Funnels" || kind === "Retention";
  const ir = () => buildQuery(kind, events.filter(Boolean), math, days, interval, filters, breakdown);

  const run = async () => {
    setBusy(true); setError("");
    const r = await api.runQuery(token, ir() as any);
    setBusy(false);
    if (!r) { setError("Query failed — check the events and filters."); setResult(null); return; }
    setResult(r);
  };

  const setEventAt = (i: number, v: string) =>
    setEvents(es => es.map((e, j) => (j === i ? v : e)));

  return (<div style={{ marginBottom: 20 }}>
    <datalist id="event-names">{eventNames.map(e => <option key={e} value={e} />)}</datalist>
    <datalist id="prop-keys">{propKeys.map(k => <option key={k} value={k} />)}</datalist>

    <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap", marginBottom: 8 }}>
      <select value={kind} onChange={e => { setKind(e.target.value); setResult(null); }}>
        {KINDS.map(k => <option key={k} value={k}>{k}</option>)}
      </select>
      {!multiEvent && (
        <select value={math} onChange={e => setMath(e.target.value)}>
          {MATHS.map(m => <option key={m} value={m}>{m}</option>)}
        </select>
      )}
      <select value={days} onChange={e => setDays(Number(e.target.value))}>
        {RANGES.map(([l, v]) => <option key={v} value={v}>{l}</option>)}
      </select>
      <select value={interval} onChange={e => setInterval(e.target.value)}>
        {["Hour", "Day", "Week", "Month"].map(i => <option key={i} value={i}>{i}</option>)}
      </select>
      <button onClick={run} disabled={busy}>{busy ? "Running…" : "Run"}</button>
    </div>

    <div style={{ marginBottom: 8 }}>
      {events.map((ev, i) => (
        <div key={i} style={{ display: "flex", gap: 8, marginBottom: 4, alignItems: "center" }}>
          <span style={{ color: "var(--dim)", fontSize: 12, width: 48 }}>
            {multiEvent ? `Step ${i + 1}` : "Event"}
          </span>
          <input list="event-names" value={ev} onChange={e => setEventAt(i, e.target.value)}
                 placeholder="event name" style={{ width: 220 }} />
          {events.length > 1 && (
            <button onClick={() => setEvents(es => es.filter((_, j) => j !== i))}>−</button>
          )}
        </div>
      ))}
      <button onClick={() => setEvents(es => [...es, ""])} style={{ fontSize: 12 }}>
        + {multiEvent ? "step" : "series"}
      </button>
    </div>

    <div style={{ marginBottom: 8 }}>
      {filters.map((f, i) => (
        <div key={i} style={{ display: "flex", gap: 8, marginBottom: 4, alignItems: "center" }}>
          <span style={{ color: "var(--dim)", fontSize: 12, width: 48 }}>{i === 0 ? "Where" : "and"}</span>
          <input list="prop-keys" value={f.key} placeholder="property" style={{ width: 160 }}
                 onChange={e => setFilters(fs => fs.map((x, j) => j === i ? { ...x, key: e.target.value } : x))} />
          <select value={f.op}
                  onChange={e => setFilters(fs => fs.map((x, j) => j === i ? { ...x, op: e.target.value } : x))}>
            {OPERATORS.map(o => <option key={o} value={o}>{o}</option>)}
          </select>
          {!["is_set", "is_not_set"].includes(f.op) && (<>
            <datalist id={`vals-${i}`}>
              {(valueHints[f.key] ?? []).map(v => <option key={v} value={v} />)}
            </datalist>
            <input list={`vals-${i}`} value={f.value} placeholder="value" style={{ width: 160 }}
                   onChange={e => setFilters(fs => fs.map((x, j) => j === i ? { ...x, value: e.target.value } : x))} />
          </>)}
          <button onClick={() => setFilters(fs => fs.filter((_, j) => j !== i))}>−</button>
        </div>
      ))}
      <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
        <button style={{ fontSize: 12 }}
                onClick={() => setFilters(fs => [...fs, { key: "", op: "exact", value: "" }])}>
          + filter
        </button>
        <span style={{ color: "var(--dim)", fontSize: 12, marginLeft: 8 }}>Breakdown</span>
        <input list="prop-keys" value={breakdown} placeholder="none"
               onChange={e => setBreakdown(e.target.value)} style={{ width: 160 }} />
      </div>
    </div>

    {error && <div style={{ color: "#e55", marginBottom: 8 }}>{error}</div>}

    {result && <ResultView result={result} />}

    {result && (
      <div style={{ display: "flex", gap: 8, marginTop: 8 }}>
        <input placeholder="Insight name" value={name} onChange={e => setName(e.target.value)} />
        <button disabled={!name.trim()} onClick={() => { onSave(ir(), name.trim()); setName(""); }}>
          Save insight
        </button>
      </div>
    )}
  </div>);
}

/** Renders whatever shape came back — a bar chart for one series over time,
 *  a table when there are several or when the x-axis is not time. */
function ResultView({ result }: { result: any }) {
  const series: any[] = result.results ?? [];
  if (!series.length || !series.some(s => (s.data?.length ?? 0) > 0)) {
    return <div className="empty">No data for this query.</div>;
  }
  const single = series.length === 1;
  const max = Math.max(1, ...series.flatMap(s => (s.data ?? []).map((d: any) => d.count)));

  return (<div style={{ padding: 12, background: "var(--panel2)", borderRadius: 8 }}>
    {single ? (
      <div>
        <div style={{ fontSize: 12, color: "var(--dim)", marginBottom: 6 }}>
          {series[0].label} — max {max.toLocaleString()}
        </div>
        {series[0].data.slice(0, 40).map((d: any) => (
          <div key={d.interval} style={{ display: "flex", alignItems: "center", gap: 8, marginBottom: 2 }}>
            <span style={{ width: 130, fontSize: 12, color: "var(--dim)" }}>{d.interval}</span>
            <div style={{ flex: 1, background: "var(--line)", borderRadius: 3, height: 12 }}>
              <div style={{ width: `${(d.count / max) * 100}%`, background: "var(--accent)", height: "100%", borderRadius: 3 }} />
            </div>
            <span style={{ width: 60, textAlign: "right", fontSize: 12 }}>{d.count.toLocaleString()}</span>
          </div>
        ))}
      </div>
    ) : (
      series.map((s, i) => (
        <div key={i} style={{ marginBottom: 6 }}>
          <strong>{s.label}</strong>
          {s.breakdown_value ? <span className="did"> · {s.breakdown_value}</span> : null}
          <span style={{ marginLeft: 8 }}>
            {(s.data ?? []).slice(0, 8).map((d: any) => `${d.interval}: ${d.count.toLocaleString()}`).join("  ")}
          </span>
        </div>
      ))
    )}
    <div style={{ fontSize: 11, color: "var(--dim)", marginTop: 6 }}>
      {result.meta?.kind} · {result.meta?.elapsed_ms}ms{result.meta?.cached ? " · cached" : ""}
    </div>
  </div>);
}

function Insights({ token }: { token: string }) {
  const [insights, setInsights] = useState<SavedInsight[]>([]);
  const [dashboards, setDashboards] = useState<Dashboard[]>([]);

  const load = async () => {
    const [ir, dr] = await Promise.all([
      fetch(`/api/insights?token=${encodeURIComponent(token)}`),
      fetch(`/api/dashboards?token=${encodeURIComponent(token)}`),
    ]);
    if (ir.ok) setInsights(await ir.json());
    if (dr.ok) setDashboards(await dr.json());
  };
  useEffect(() => { load(); }, [token]);

  const save = async (query_ir: any, name: string) => {
    const r = await fetch("/api/insights", {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ token, name, query_ir }),
    });
    if (r.ok) load();
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
    <InsightBuilder token={token} onSave={save} />
    <h3>Saved ({insights.length})</h3>
    {insights.map(i => (
      <div key={i.id} className="row" style={{ padding: "6px 0", flexWrap: "wrap" }}>
        <span style={{ fontWeight: 600 }}>{i.name}</span>
        <span className="did" style={{ marginLeft: 8 }}>{i.query_ir?.kind ?? i.description}</span>
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
