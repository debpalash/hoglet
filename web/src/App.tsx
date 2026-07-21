import { useEffect, useState, useCallback } from "react";
import { api } from "./api";
import type { EventCount, RecentEvent, FunnelStep, TrendPoint, FlagDef, Stats } from "./api";

type Tab = "overview" | "funnels" | "trends" | "flags";
const TABS: Tab[] = ["overview", "funnels", "trends", "flags"];

export function App() {
  const [token, setToken] = useState("phc_demo");
  const [tab, setTab] = useState<Tab>("overview");

  return (
    <>
      <header>
        <span className="hog">🦔</span>
        <h1>Hoglet</h1>
        <span className="tag">PostHog-compatible · one binary</span>
        <input value={token} spellCheck={false} onChange={(e) => setToken(e.target.value)} />
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
      </main>
      <footer>Hoglet · numbers you can trust</footer>
    </>
  );
}

function Overview({ token }: { token: string }) {
  const [stats, setStats] = useState<Stats | null>(null);
  const [top, setTop] = useState<EventCount[]>([]);
  const [live, setLive] = useState<RecentEvent[]>([]);

  const load = useCallback(async () => {
    const [s, t, l] = await Promise.all([
      api.stats(token),
      api.topEvents(token, 8),
      api.recent(token, 25),
    ]);
    if (s) setStats(s);
    if (t) setTop(t);
    if (l) setLive(l);
  }, [token]);

  useEffect(() => {
    load();
    const id = setInterval(load, 2000);
    return () => clearInterval(id);
  }, [load]);

  const epp = stats && stats.unique_persons > 0 ? (stats.total_events / stats.unique_persons).toFixed(1) : "–";
  const maxTop = Math.max(1, ...top.map((t) => t.count));

  return (
    <>
      <div className="cards">
        <Card label="Total events" value={stats?.total_events} />
        <Card label="Unique persons" value={stats?.unique_persons} />
        <Card label="Last 24h" value={stats?.events_24h} />
        <Card label="Events / person" value={epp} raw />
      </div>
      <div className="grid2">
        <section className="panel">
          <h2>Top events</h2>
          {top.length ? (
            top.map((t) => (
              <div className="row" key={t.event}>
                <span>{t.event}</span>
                <div className="bar-wrap"><div className="bar" style={{ width: `${(t.count / maxTop) * 100}%` }} /></div>
                <span className="n">{t.count.toLocaleString()}</span>
              </div>
            ))
          ) : (
            <div className="empty">No events yet.</div>
          )}
        </section>
        <section className="panel">
          <h2>Live stream</h2>
          {live.length ? (
            live.map((e) => (
              <div className="ev" key={e.uuid}>
                <span className="name">{e.event}</span>
                <span className="did">{e.distinct_id}</span>
                <span className="ts">{e.timestamp.slice(11, 19)}</span>
              </div>
            ))
          ) : (
            <div className="empty">Waiting for events…</div>
          )}
        </section>
      </div>
    </>
  );
}

function Card({ label, value, raw }: { label: string; value?: number | string; raw?: boolean }) {
  const shown = value === undefined ? "–" : raw ? value : Number(value).toLocaleString();
  return (
    <div className="card">
      <div className="label">{label}</div>
      <div className="value">{shown}</div>
    </div>
  );
}

function Funnels({ token }: { token: string }) {
  const [input, setInput] = useState("signup, activate, purchase");
  const [steps, setSteps] = useState<FunnelStep[]>([]);
  const [ran, setRan] = useState(false);

  const run = async () => {
    const names = input.split(",").map((s) => s.trim()).filter(Boolean);
    if (!names.length) return;
    const data = await api.funnel(token, names);
    setSteps(data ?? []);
    setRan(true);
  };

  const first = steps[0]?.reached ?? 0;

  return (
    <section className="panel">
      <h2>Funnel builder</h2>
      <div className="controls">
        <input style={{ flex: 1, minWidth: 280 }} value={input} onChange={(e) => setInput(e.target.value)} placeholder="comma-separated event names" />
        <button onClick={run}>Run funnel</button>
      </div>
      {steps.length ? (
        steps.map((s, i) => {
          const pctOfFirst = first ? (s.reached / first) * 100 : 0;
          const prev = i ? steps[i - 1].reached : s.reached;
          const drop = prev ? 100 - (s.reached / prev) * 100 : 0;
          return (
            <div className="funnel-step" key={i}>
              <div className="top">
                <span>{i + 1}. {s.event}</span>
                <span><span className="conv">{pctOfFirst.toFixed(0)}%</span>{i ? <span className="drop"> (−{drop.toFixed(0)}% from prev)</span> : null}</span>
              </div>
              <div className="fbar" style={{ width: `${Math.max(pctOfFirst, 2)}%` }}>{s.reached.toLocaleString()}</div>
            </div>
          );
        })
      ) : (
        <div className="empty">{ran ? "No data for those steps." : "Enter steps and run."}</div>
      )}
    </section>
  );
}

function Trends({ token }: { token: string }) {
  const [event, setEvent] = useState("$pageview");
  const [days, setDays] = useState(30);
  const [data, setData] = useState<TrendPoint[] | null>(null);

  const plot = async () => setData((await api.trend(token, event, days)) ?? []);

  const W = 900, H = 240, pad = 30;
  const max = Math.max(1, ...(data ?? []).map((d) => d.count));
  const bw = data && data.length ? (W - pad * 2) / data.length : 0;

  return (
    <section className="panel">
      <h2>Trend</h2>
      <div className="controls">
        <input value={event} onChange={(e) => setEvent(e.target.value)} placeholder="event name" />
        <select value={days} onChange={(e) => setDays(Number(e.target.value))}>
          <option value={7}>7 days</option>
          <option value={30}>30 days</option>
          <option value={90}>90 days</option>
        </select>
        <button onClick={plot}>Plot</button>
      </div>
      {data && data.length ? (
        <svg viewBox={`0 0 ${W} ${H}`} width="100%">
          <text x={pad} y={16}>{event} — max {max}/day</text>
          {data.map((d, i) => {
            const h = (d.count / max) * (H - pad * 2);
            const x = pad + i * bw;
            const y = H - pad - h;
            return (
              <g key={d.day}>
                <rect x={x + 2} y={y} width={bw - 4} height={h} fill="var(--accent)" rx={2}>
                  <title>{d.day}: {d.count}</title>
                </rect>
                {data.length <= 31 && <text x={x + bw / 2} y={H - pad + 14} textAnchor="middle">{d.day.slice(5)}</text>}
              </g>
            );
          })}
        </svg>
      ) : (
        <div className="empty">{data ? `No data for "${event}".` : "Pick an event and plot."}</div>
      )}
    </section>
  );
}

function Flags({ token }: { token: string }) {
  const [flags, setFlags] = useState<FlagDef[] | null>(null);
  useEffect(() => { api.flags(token).then(setFlags); }, [token]);

  return (
    <section className="panel">
      <h2>Feature flags</h2>
      {flags && flags.length ? (
        flags.map((f) => (
          <div className="flag" key={f.key}>
            <span className="key">
              {f.key}
              {f.variants.length > 0 && (
                <span className="did"> {f.variants.map((v) => `${v.key} ${v.rollout}%`).join(" · ")}</span>
              )}
            </span>
            <span className={`pill ${f.active ? "on" : "off"}`}>{f.active ? "active" : "inactive"}</span>
            <span className="rollout">{f.rollout_percentage}%</span>
          </div>
        ))
      ) : (
        <div className="empty">No flags defined. Create one via the admin API.</div>
      )}
    </section>
  );
}
