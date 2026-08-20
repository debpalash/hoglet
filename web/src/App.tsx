import {
  useCallback,
  useEffect,
  useReducer,
  useRef,
  useState,
  type FormEvent,
} from "react";
import {
  api,
  isApiError,
  setSessionExpiredCallback,
  type Dashboard,
  type FlagDef,
  type Query,
  type QueryResponse,
  type SavedInsight,
} from "./api";
import {
  ACTIVE_PROJECT_STORAGE_KEY,
  activeProjectPreference,
  beginProjectRequest,
  initialWorkspaceState,
  isCurrentProjectRequest,
  projectsIn,
  readActiveProjectPreference,
  workspaceReducer,
  type AbortableProjectRequest,
  type ApiError as WorkspaceApiError,
  type Project,
  type ProjectRequestIdentity,
  type Workspace,
  type WorkspaceState,
} from "./workspace";

type Tab = "trends" | "flags" | "insights" | "dashboards";
type ReadyWorkspaceState = Extract<WorkspaceState, { status: "ready" }>;
type ProjectErrorHandler = (error: unknown, request: ProjectRequestIdentity) => void;

const TABS: readonly Tab[] = ["trends", "flags", "insights", "dashboards"];

function storedPreference() {
  if (typeof window === "undefined") return null;
  return readActiveProjectPreference((key) => window.localStorage.getItem(key));
}

function rememberProject(userId: string, projectId: string): void {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.setItem(
      ACTIVE_PROJECT_STORAGE_KEY,
      JSON.stringify(activeProjectPreference(userId, projectId)),
    );
  } catch {
    // Project selection still works when storage is disabled.
  }
}

function unavailableError(error: unknown): WorkspaceApiError {
  if (isApiError(error)) {
    return {
      code: "unavailable",
      message: error.message,
      request_id:
        error.kind === "network" ? "not-issued" : (error.requestId ?? "not-returned"),
    };
  }
  return {
    code: "unavailable",
    message: error instanceof Error ? error.message : "Hoglet is unavailable.",
    request_id: "not-issued",
  };
}

function errorMessage(error: unknown): string {
  if (isApiError(error)) {
    const field = error.kind === "http" && error.field ? ` (${error.field})` : "";
    return `${error.message}${field}`;
  }
  return error instanceof Error ? error.message : "The request failed.";
}

function isHttpStatus(error: unknown, status: number): boolean {
  return isApiError(error) && error.kind === "http" && error.status === status;
}

export function App() {
  const [state, dispatch] = useReducer(workspaceReducer, initialWorkspaceState);
  const [tab, setTab] = useState<Tab>("trends");

  useEffect(
    () => setSessionExpiredCallback(() => dispatch({ type: "sessionExpired" })),
    [],
  );

  useEffect(() => {
    if (state.status !== "bootstrapping") return;

    const controller = new AbortController();
    const requestEpoch = state.requestEpoch;
    const reason = state.reason;

    const finish = (
      result:
        | { kind: "setupRequired" }
        | { kind: "unauthenticated" }
        | { kind: "workspace"; workspace: Workspace; preference: ReturnType<typeof storedPreference> }
        | { kind: "unavailable"; error: WorkspaceApiError },
    ) => dispatch({ type: "bootstrapResult", requestEpoch, result });

    const load = async () => {
      try {
        if (reason !== "forbiddenWorkspaceRefresh") {
          const bootstrap = await api.bootstrap(controller.signal);
          if (bootstrap.setup_required) {
            finish({ kind: "setupRequired" });
            return;
          }
        }

        try {
          const workspace = await api.me(controller.signal);
          finish({ kind: "workspace", workspace, preference: storedPreference() });
        } catch (error) {
          if (controller.signal.aborted) return;
          if (isHttpStatus(error, 401)) {
            finish({ kind: "unauthenticated" });
          } else {
            finish({ kind: "unavailable", error: unavailableError(error) });
          }
        }
      } catch (error) {
        if (!controller.signal.aborted) {
          finish({ kind: "unavailable", error: unavailableError(error) });
        }
      }
    };

    void load();
    return () => controller.abort();
  }, [state]);

  const acceptWorkspace = useCallback((workspace: Workspace) => {
    dispatch({
      type: "authenticatedWorkspace",
      workspace,
      preference: storedPreference(),
    });
  }, []);

  const logout = useCallback(async () => {
    try {
      await api.logout();
      dispatch({ type: "logout" });
    } catch (error) {
      if (isHttpStatus(error, 401)) {
        dispatch({ type: "logout" });
      } else {
        dispatch({ type: "unavailable", error: unavailableError(error) });
      }
    }
  }, []);

  const handleProjectError = useCallback<ProjectErrorHandler>((error, request) => {
    // request() owns the application-wide 401 callback, which dispatches
    // sessionExpired once for the current session generation.
    if (isHttpStatus(error, 401)) return;
    if (isHttpStatus(error, 403)) {
      dispatch({ type: "forbiddenWorkspaceRefresh", request });
      return;
    }
    if (isApiError(error) && (error.kind === "network" || error.kind === "decode")) {
      dispatch({ type: "unavailable", error: unavailableError(error), request });
    }
  }, []);

  switch (state.status) {
    case "bootstrapping":
      return <StatusScreen title="Loading Hoglet…" />;
    case "setupRequired":
      return (
        <Setup
          onDone={acceptWorkspace}
          onConflict={() => dispatch({ type: "retry" })}
        />
      );
    case "unauthenticated":
      return <Login onDone={acceptWorkspace} />;
    case "unavailable":
      return (
        <StatusScreen
          title="Hoglet is unavailable"
          detail={state.error.message}
          action="Retry"
          onAction={() => dispatch({ type: "retry" })}
        />
      );
    case "readyNoProject":
      return (
        <NoProject
          workspace={state.workspace}
          onWorkspace={acceptWorkspace}
          onLogout={logout}
        />
      );
    case "ready":
      return (
        <WorkspaceShell
          state={state}
          tab={tab}
          onTab={setTab}
          onLogout={logout}
          onProject={(projectId) => {
            rememberProject(state.workspace.user.id, projectId);
            dispatch({ type: "projectSelected", projectId });
          }}
          onProjectError={handleProjectError}
        />
      );
  }
}

interface StatusScreenProps {
  title: string;
  detail?: string;
  action?: string;
  onAction?: () => void;
}

function StatusScreen({ title, detail, action, onAction }: StatusScreenProps) {
  return (
    <main style={{ maxWidth: 520, paddingTop: 64 }}>
      <section className="panel">
        <h1>{title}</h1>
        {detail ? <p className="empty">{detail}</p> : null}
        {action && onAction ? <button onClick={onAction}>{action}</button> : null}
      </section>
    </main>
  );
}

interface SetupProps {
  onDone: (workspace: Workspace) => void;
  onConflict: () => void;
}

function Setup({ onDone, onConflict }: SetupProps) {
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [organizationName, setOrganizationName] = useState("");
  const [projectName, setProjectName] = useState("My project");
  const [existingToken, setExistingToken] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      const workspace = await api.setup({
        email,
        password,
        organization_name: organizationName,
        project_name: projectName || undefined,
        existing_project_token: existingToken.trim() || undefined,
      });
      onDone(workspace);
    } catch (requestError) {
      if (isHttpStatus(requestError, 409)) {
        onConflict();
        return;
      }
      setError(errorMessage(requestError));
    } finally {
      setBusy(false);
    }
  };

  return (
    <AuthPanel title="Welcome to Hoglet" detail="Create the first workspace owner.">
      <form onSubmit={submit}>
        {error ? <p style={{ color: "#e55" }}>{error}</p> : null}
        <AuthInput label="Email" value={email} onChange={setEmail} type="email" />
        <AuthInput label="Password" value={password} onChange={setPassword} type="password" />
        <AuthInput label="Organization" value={organizationName} onChange={setOrganizationName} />
        <AuthInput label="Project" value={projectName} onChange={setProjectName} />
        <AuthInput
          label="Existing project token (optional)"
          value={existingToken}
          onChange={setExistingToken}
          spellCheck={false}
        />
        <button type="submit" disabled={busy || !email || !password || !organizationName}>
          {busy ? "Creating…" : "Create workspace"}
        </button>
      </form>
    </AuthPanel>
  );
}

function Login({ onDone }: { onDone: (workspace: Workspace) => void }) {
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      onDone(await api.login({ email, password }));
    } catch (requestError) {
      setError(
        isHttpStatus(requestError, 401)
          ? "Invalid email or password."
          : errorMessage(requestError),
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <AuthPanel title="Log in">
      <form onSubmit={submit}>
        {error ? <p style={{ color: "#e55" }}>{error}</p> : null}
        <AuthInput label="Email" value={email} onChange={setEmail} type="email" />
        <AuthInput label="Password" value={password} onChange={setPassword} type="password" />
        <button type="submit" disabled={busy || !email || !password}>
          {busy ? "Logging in…" : "Log in"}
        </button>
      </form>
    </AuthPanel>
  );
}

function AuthPanel({
  title,
  detail,
  children,
}: {
  title: string;
  detail?: string;
  children: React.ReactNode;
}) {
  return (
    <main style={{ maxWidth: 420, paddingTop: 60 }}>
      <section className="panel">
        <h1>{title}</h1>
        {detail ? <p className="empty">{detail}</p> : null}
        {children}
      </section>
    </main>
  );
}

interface AuthInputProps {
  label: string;
  value: string;
  onChange: (value: string) => void;
  type?: "text" | "email" | "password";
  spellCheck?: boolean;
}

function AuthInput({
  label,
  value,
  onChange,
  type = "text",
  spellCheck,
}: AuthInputProps) {
  return (
    <label style={{ display: "block", marginBottom: 12 }}>
      <span className="empty" style={{ display: "block", padding: "0 0 4px" }}>
        {label}
      </span>
      <input
        style={{ width: "100%" }}
        type={type}
        value={value}
        spellCheck={spellCheck}
        onChange={(event) => onChange(event.target.value)}
        required={!label.includes("optional")}
      />
    </label>
  );
}

function NoProject({
  workspace,
  onWorkspace,
  onLogout,
}: {
  workspace: Workspace;
  onWorkspace: (workspace: Workspace) => void;
  onLogout: () => void;
}) {
  const [name, setName] = useState("My project");
  const [organizationId, setOrganizationId] = useState(
    () => workspace.organizations[0]?.id ?? "",
  );
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const create = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      await api.createProject(organizationId, name);
      onWorkspace(await api.me());
    } catch (requestError) {
      setError(errorMessage(requestError));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <SimpleHeader email={workspace.user.email} onLogout={onLogout} />
      <main style={{ maxWidth: 560 }}>
        <section className="panel">
          <h2>No project yet</h2>
          <p className="empty">Create a project before querying analytics.</p>
          {workspace.organizations.length ? (
            <form className="controls" onSubmit={create}>
              <select
                aria-label="Organization"
                value={organizationId}
                onChange={(event) => setOrganizationId(event.target.value)}
              >
                {workspace.organizations.map((organization) => (
                  <option key={organization.id} value={organization.id}>
                    {organization.name}
                  </option>
                ))}
              </select>
              <input value={name} onChange={(event) => setName(event.target.value)} />
              <button type="submit" disabled={busy || !name.trim()}>
                {busy ? "Creating…" : "Create project"}
              </button>
            </form>
          ) : (
            <p className="empty">Ask an administrator to grant organization access.</p>
          )}
          {error ? <p style={{ color: "#e55" }}>{error}</p> : null}
        </section>
      </main>
    </>
  );
}

interface WorkspaceShellProps {
  state: ReadyWorkspaceState;
  tab: Tab;
  onTab: (tab: Tab) => void;
  onProject: (projectId: string) => void;
  onLogout: () => void;
  onProjectError: ProjectErrorHandler;
}

function WorkspaceShell({
  state,
  tab,
  onTab,
  onProject,
  onLogout,
  onProjectError,
}: WorkspaceShellProps) {
  const projects = projectsIn(state.workspace);
  const activeProject = projects.find((project) => project.id === state.activeProjectId);

  if (!activeProject) {
    return <StatusScreen title="The selected project is no longer available" />;
  }

  return (
    <>
      <WorkspaceHeader
        workspace={state.workspace}
        activeProject={activeProject}
        projects={projects}
        onProject={onProject}
        onLogout={onLogout}
      />
      <nav>
        {TABS.map((item) => (
          <button
            key={item}
            className={item === tab ? "active" : ""}
            onClick={() => onTab(item)}
          >
            {item[0].toUpperCase() + item.slice(1)}
          </button>
        ))}
      </nav>
      <main>
        {tab === "trends" ? (
          <Trends state={state} onProjectError={onProjectError} />
        ) : null}
        {tab === "flags" ? <Flags state={state} onProjectError={onProjectError} /> : null}
        {tab === "insights" ? (
          <Insights state={state} onProjectError={onProjectError} />
        ) : null}
        {tab === "dashboards" ? (
          <Dashboards state={state} onProjectError={onProjectError} />
        ) : null}
      </main>
      <footer>Hoglet · numbers you can trust</footer>
    </>
  );
}

function SimpleHeader({ email, onLogout }: { email: string; onLogout: () => void }) {
  return (
    <header>
      <span className="hog">🦔</span>
      <h1>Hoglet</h1>
      <span className="tag">PostHog-compatible capture · one binary</span>
      <button style={{ marginLeft: "auto" }} onClick={onLogout} title="Log out">
        {email}
      </button>
    </header>
  );
}

interface WorkspaceHeaderProps {
  workspace: Workspace;
  activeProject: Project;
  projects: Project[];
  onProject: (projectId: string) => void;
  onLogout: () => void;
}

function WorkspaceHeader({
  workspace,
  activeProject,
  projects,
  onProject,
  onLogout,
}: WorkspaceHeaderProps) {
  const [copyStatus, setCopyStatus] = useState("Copy token");

  const copyToken = async () => {
    try {
      await navigator.clipboard.writeText(activeProject.token);
      setCopyStatus("Copied");
    } catch {
      setCopyStatus("Copy failed");
    }
  };

  useEffect(() => setCopyStatus("Copy token"), [activeProject.id]);

  return (
    <header>
      <span className="hog">🦔</span>
      <h1>Hoglet</h1>
      <select
        aria-label="Active project"
        value={activeProject.id}
        onChange={(event) => onProject(event.target.value)}
        style={{ marginLeft: "auto" }}
      >
        {projects.map((project) => (
          <option key={project.id} value={project.id}>
            {project.name}
          </option>
        ))}
      </select>
      <code title="Capture token">{activeProject.token}</code>
      <button onClick={copyToken}>{copyStatus}</button>
      <button onClick={onLogout} title="Log out">
        {workspace.user.email}
      </button>
    </header>
  );
}

interface ProjectRequestTools {
  start: () => AbortableProjectRequest | null;
  isCurrent: (request: AbortableProjectRequest) => boolean;
  report: (error: unknown, request: AbortableProjectRequest) => void;
}

function useProjectRequests(
  state: ReadyWorkspaceState,
  onProjectError: ProjectErrorHandler,
): ProjectRequestTools {
  const stateRef = useRef<WorkspaceState>(state);
  const errorHandlerRef = useRef(onProjectError);
  const activeRequestRef = useRef<AbortableProjectRequest | undefined>(undefined);
  stateRef.current = state;
  errorHandlerRef.current = onProjectError;

  useEffect(
    () => () => {
      activeRequestRef.current?.abort();
    },
    [],
  );

  const start = useCallback(() => {
    const request = beginProjectRequest(stateRef.current, activeRequestRef.current);
    if (request) activeRequestRef.current = request;
    return request;
  }, []);

  const isCurrent = useCallback(
    (request: AbortableProjectRequest) =>
      !request.signal.aborted && isCurrentProjectRequest(stateRef.current, request.identity),
    [],
  );

  const report = useCallback((error: unknown, request: AbortableProjectRequest) => {
    if (!request.signal.aborted && isCurrentProjectRequest(stateRef.current, request.identity)) {
      errorHandlerRef.current(error, request.identity);
    }
  }, []);

  return { start, isCurrent, report };
}

type TrendMath = "total" | "unique_persons";
type EventOperator =
  | "exact"
  | "iexact"
  | "not_equal"
  | "contains"
  | "not_contains"
  | "icontains"
  | "is_set"
  | "is_not_set";

interface SeriesDraft {
  id: number;
  event: string;
  math: TrendMath;
}

interface FilterDraft {
  id: number;
  key: string;
  operator: EventOperator;
  value: string;
}

const EVENT_OPERATORS: readonly EventOperator[] = [
  "exact",
  "iexact",
  "not_equal",
  "contains",
  "not_contains",
  "icontains",
  "is_set",
  "is_not_set",
];

function utcInput(date: Date): string {
  return date.toISOString().slice(0, 16);
}

function initialRange(): { from: string; to: string } {
  const to = new Date();
  const from = new Date(to.getTime() - 30 * 24 * 60 * 60 * 1000);
  return { from: utcInput(from), to: utcInput(to) };
}

function absoluteUtc(value: string): string | null {
  if (!value) return null;
  const parsed = new Date(`${value}:00Z`);
  return Number.isNaN(parsed.getTime()) ? null : parsed.toISOString();
}

function buildTrendsQuery(
  series: SeriesDraft[],
  filters: FilterDraft[],
  breakdown: string,
  interval: "Hour" | "Day" | "Week" | "Month",
  from: string,
  to: string,
): Query {
  const needsValue = (operator: EventOperator) =>
    operator !== "is_set" && operator !== "is_not_set";

  // GroupOp's generated TS spelling follows the Rust variant ("And"), while
  // serde's actual wire spelling is "AND". Keep the cast at this one boundary.
  return {
    kind: "Trends",
    series: series.map((item) => ({
      event: { type: "name", value: item.event.trim() },
      math: { type: item.math },
    })),
    filters: {
      op: "AND",
      values: filters
        .filter((filter) => filter.key.trim())
        .map((filter) => ({
          type: "filter",
          source: "event",
          key: filter.key.trim(),
          operator: { op: filter.operator },
          value: needsValue(filter.operator) ? filter.value : null,
        })),
    },
    breakdown: breakdown.trim()
      ? { source: "event", key: breakdown.trim(), limit: 10 }
      : null,
    breakdown2: null,
    range: { from, to, last_n: null },
    interval,
    formulas: [],
    funnel_config: null,
    retention_config: null,
    lifecycle_config: null,
    stickiness_config: null,
    actors_config: null,
    sql_config: null,
  } as unknown as Query;
}

function Trends({
  state,
  onProjectError,
}: {
  state: ReadyWorkspaceState;
  onProjectError: ProjectErrorHandler;
}) {
  const { start, isCurrent, report } = useProjectRequests(state, onProjectError);
  const range = useRef(initialRange()).current;
  const nextId = useRef(1);
  const [series, setSeries] = useState<SeriesDraft[]>([
    { id: 0, event: "$pageview", math: "total" },
  ]);
  const [filters, setFilters] = useState<FilterDraft[]>([]);
  const [breakdown, setBreakdown] = useState("");
  const [interval, setInterval] = useState<"Hour" | "Day" | "Week" | "Month">("Day");
  const [from, setFrom] = useState(range.from);
  const [to, setTo] = useState(range.to);
  const [eventNames, setEventNames] = useState<string[]>([]);
  const [propertyKeys, setPropertyKeys] = useState<string[]>([]);
  const [valueHints, setValueHints] = useState<Record<string, string[]>>({});
  const [result, setResult] = useState<QueryResponse | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [insightName, setInsightName] = useState("");
  const [saved, setSaved] = useState("");

  useEffect(() => {
    setResult(null);
    setError("");
    setSaved("");
    setBusy(false);
    setValueHints({});
    const request = start();
    if (!request) return;

    const loadCatalog = async () => {
      try {
        const [events, properties] = await Promise.all([
          api.catalogEvents(request.identity.projectId, 200, request.signal),
          api.catalogProperties(request.identity.projectId, "event", request.signal),
        ]);
        if (!isCurrent(request)) return;
        setEventNames(events);
        setPropertyKeys(properties.map((property) => property.key));
      } catch (requestError) {
        if (!isCurrent(request)) return;
        setError(errorMessage(requestError));
        report(requestError, request);
      }
    };

    void loadCatalog();
  }, [state.activeProjectId, state.requestEpoch, start, isCurrent, report]);

  const loadValueHints = async (key: string) => {
    const normalized = key.trim();
    if (!normalized || normalized in valueHints) return;
    const request = start();
    if (!request) return;
    try {
      const values = await api.catalogValues(
        request.identity.projectId,
        normalized,
        50,
        request.signal,
      );
      if (isCurrent(request)) {
        setValueHints((current) => ({ ...current, [normalized]: values }));
      }
    } catch (requestError) {
      report(requestError, request);
    }
  };

  const queryFromDraft = (): Query | null => {
    const absoluteFrom = absoluteUtc(from);
    const absoluteTo = absoluteUtc(to);
    if (!absoluteFrom || !absoluteTo) {
      setError("From and to must be absolute UTC timestamps.");
      return null;
    }
    if (absoluteFrom >= absoluteTo) {
      setError("From must be strictly before to.");
      return null;
    }
    const populatedSeries = series.filter((item) => item.event.trim());
    if (!populatedSeries.length) {
      setError("Add at least one event series.");
      return null;
    }
    return buildTrendsQuery(
      populatedSeries,
      filters,
      breakdown,
      interval,
      absoluteFrom,
      absoluteTo,
    );
  };

  const run = async () => {
    const query = queryFromDraft();
    if (!query) return;
    const request = start();
    if (!request) return;
    setBusy(true);
    setError("");
    setSaved("");
    try {
      const response = await api.runQuery(
        request.identity.projectId,
        query,
        false,
        request.signal,
      );
      if (isCurrent(request)) setResult(response);
    } catch (requestError) {
      if (!isCurrent(request)) return;
      setResult(null);
      setError(errorMessage(requestError));
      report(requestError, request);
    } finally {
      if (isCurrent(request)) setBusy(false);
    }
  };

  const save = async () => {
    const query = queryFromDraft();
    if (!query || !insightName.trim()) return;
    const request = start();
    if (!request) return;
    setError("");
    try {
      await api.saveInsight(
        request.identity.projectId,
        { name: insightName.trim(), query_ir: query },
        request.signal,
      );
      if (isCurrent(request)) {
        setSaved(`Saved “${insightName.trim()}”.`);
        setInsightName("");
      }
    } catch (requestError) {
      if (!isCurrent(request)) return;
      setError(errorMessage(requestError));
      report(requestError, request);
    }
  };

  const updateSeries = (id: number, patch: Partial<SeriesDraft>) =>
    setSeries((current) =>
      current.map((item) => (item.id === id ? { ...item, ...patch } : item)),
    );
  const updateFilter = (id: number, patch: Partial<FilterDraft>) =>
    setFilters((current) =>
      current.map((item) => (item.id === id ? { ...item, ...patch } : item)),
    );

  return (
    <section className="panel">
      <h2>Trends</h2>
      <p className="empty">
        Supported today: event totals and unique people, event-property filters, and one
        event-property breakdown.
      </p>
      <datalist id="event-catalog">
        {eventNames.map((event) => (
          <option key={event} value={event} />
        ))}
      </datalist>
      <datalist id="property-catalog">
        {propertyKeys.map((key) => (
          <option key={key} value={key} />
        ))}
      </datalist>

      {series.map((item, index) => (
        <div className="controls" key={item.id}>
          <span className="empty">Series {index + 1}</span>
          <input
            list="event-catalog"
            value={item.event}
            placeholder="event name"
            onChange={(event) => updateSeries(item.id, { event: event.target.value })}
          />
          <select
            aria-label={`Math for series ${index + 1}`}
            value={item.math}
            onChange={(event) =>
              updateSeries(item.id, { math: event.target.value as TrendMath })
            }
          >
            <option value="total">Total events</option>
            <option value="unique_persons">Unique people</option>
          </select>
          {series.length > 1 ? (
            <button onClick={() => setSeries((items) => items.filter((x) => x.id !== item.id))}>
              Remove
            </button>
          ) : null}
        </div>
      ))}
      <button
        disabled={series.length >= 10}
        onClick={() =>
          setSeries((items) => [
            ...items,
            { id: nextId.current++, event: "", math: "total" },
          ])
        }
      >
        Add series
      </button>

      <h3>Event filters</h3>
      {filters.map((filter) => (
        <div className="controls" key={filter.id}>
          <input
            list="property-catalog"
            value={filter.key}
            placeholder="property"
            onBlur={() => void loadValueHints(filter.key)}
            onChange={(event) => updateFilter(filter.id, { key: event.target.value })}
          />
          <select
            aria-label="Filter operator"
            value={filter.operator}
            onChange={(event) =>
              updateFilter(filter.id, { operator: event.target.value as EventOperator })
            }
          >
            {EVENT_OPERATORS.map((operator) => (
              <option key={operator} value={operator}>
                {operator}
              </option>
            ))}
          </select>
          {filter.operator === "is_set" || filter.operator === "is_not_set" ? null : (
            <>
              <datalist id={`values-${filter.id}`}>
                {(valueHints[filter.key.trim()] ?? []).map((value) => (
                  <option key={value} value={value} />
                ))}
              </datalist>
              <input
                list={`values-${filter.id}`}
                value={filter.value}
                placeholder="value"
                onChange={(event) => updateFilter(filter.id, { value: event.target.value })}
              />
            </>
          )}
          <button
            onClick={() => setFilters((items) => items.filter((x) => x.id !== filter.id))}
          >
            Remove
          </button>
        </div>
      ))}
      <button
        disabled={filters.length >= 32}
        onClick={() =>
          setFilters((items) => [
            ...items,
            { id: nextId.current++, key: "", operator: "exact", value: "" },
          ])
        }
      >
        Add filter
      </button>

      <h3>Range and grouping</h3>
      <div className="controls">
        <label>
          <span className="empty">From (UTC)</span>
          <input type="datetime-local" value={from} onChange={(event) => setFrom(event.target.value)} />
        </label>
        <label>
          <span className="empty">To (UTC)</span>
          <input type="datetime-local" value={to} onChange={(event) => setTo(event.target.value)} />
        </label>
        <select
          aria-label="Interval"
          value={interval}
          onChange={(event) =>
            setInterval(event.target.value as "Hour" | "Day" | "Week" | "Month")
          }
        >
          <option value="Hour">Hour</option>
          <option value="Day">Day</option>
          <option value="Week">Week</option>
          <option value="Month">Month</option>
        </select>
        <input
          list="property-catalog"
          value={breakdown}
          placeholder="breakdown property (optional)"
          onChange={(event) => setBreakdown(event.target.value)}
        />
        <button onClick={run} disabled={busy}>
          {busy ? "Running…" : "Run trend"}
        </button>
      </div>

      {error ? <p style={{ color: "#e55" }}>{error}</p> : null}
      {result ? <ResultView result={result} /> : <div className="empty">Build and run a trend.</div>}
      {result ? (
        <div className="controls" style={{ marginTop: 12 }}>
          <input
            placeholder="Insight name"
            value={insightName}
            onChange={(event) => setInsightName(event.target.value)}
          />
          <button disabled={!insightName.trim()} onClick={save}>
            Save insight
          </button>
          {saved ? <span className="empty">{saved}</span> : null}
        </div>
      ) : null}
    </section>
  );
}

function ResultView({ result }: { result: QueryResponse }) {
  const populated = result.results.filter((series) => series.data.length > 0);
  if (!populated.length) return <div className="empty">No data for this query.</div>;
  const maximum = Math.max(
    1,
    ...populated.flatMap((series) => series.data.map((point) => point.count)),
  );

  return (
    <div style={{ padding: 12, background: "var(--panel2)", borderRadius: 8 }}>
      {populated.map((series, seriesIndex) => (
        <div key={`${series.label}-${series.breakdown_value ?? seriesIndex}`}>
          <strong>{series.label}</strong>
          {series.breakdown_value ? <span className="did"> · {series.breakdown_value}</span> : null}
          {series.data.map((point) => (
            <div className="row" key={point.interval}>
              <span>{point.interval}</span>
              <div className="bar-wrap">
                <div className="bar" style={{ width: `${(point.count / maximum) * 100}%` }} />
              </div>
              <span className="n">{point.count.toLocaleString()}</span>
            </div>
          ))}
        </div>
      ))}
      <p className="empty">
        {result.meta.kind} · {result.meta.elapsed_ms}ms{result.meta.cached ? " · cached" : ""}
      </p>
    </div>
  );
}

function Flags({
  state,
  onProjectError,
}: {
  state: ReadyWorkspaceState;
  onProjectError: ProjectErrorHandler;
}) {
  const { start, isCurrent, report } = useProjectRequests(state, onProjectError);
  const [flags, setFlags] = useState<FlagDef[] | null>(null);
  const [error, setError] = useState("");

  useEffect(() => {
    setFlags(null);
    setError("");
    const request = start();
    if (!request) return;
    api
      .listFlags(request.identity.projectId, request.signal)
      .then((response) => {
        if (isCurrent(request)) setFlags(response);
      })
      .catch((requestError: unknown) => {
        if (!isCurrent(request)) return;
        setError(errorMessage(requestError));
        report(requestError, request);
      });
  }, [state.activeProjectId, state.requestEpoch, start, isCurrent, report]);

  return (
    <section className="panel">
      <h2>Feature flags</h2>
      {error ? <p style={{ color: "#e55" }}>{error}</p> : null}
      {flags?.length ? (
        flags.map((flag) => (
          <div className="flag" key={flag.key}>
            <span className="key">
              {flag.key}
              {flag.variants.length ? (
                <span className="did">
                  {" "}
                  {flag.variants.map((variant) => `${variant.key} ${variant.rollout}%`).join(" · ")}
                </span>
              ) : null}
            </span>
            <span className={`pill ${flag.active ? "on" : "off"}`}>
              {flag.active ? "active" : "inactive"}
            </span>
            <span className="rollout">{flag.rollout_percentage}%</span>
          </div>
        ))
      ) : (
        <div className="empty">{flags ? "No flags defined." : "Loading flags…"}</div>
      )}
    </section>
  );
}

function Insights({
  state,
  onProjectError,
}: {
  state: ReadyWorkspaceState;
  onProjectError: ProjectErrorHandler;
}) {
  const { start, isCurrent, report } = useProjectRequests(state, onProjectError);
  const [insights, setInsights] = useState<SavedInsight[]>([]);
  const [dashboards, setDashboards] = useState<Dashboard[]>([]);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    const request = start();
    if (!request) return;
    setLoading(true);
    setError("");
    try {
      const [nextInsights, nextDashboards] = await Promise.all([
        api.listInsights(request.identity.projectId, request.signal),
        api.listDashboards(request.identity.projectId, request.signal),
      ]);
      if (!isCurrent(request)) return;
      setInsights(nextInsights);
      setDashboards(nextDashboards);
    } catch (requestError) {
      if (!isCurrent(request)) return;
      setError(errorMessage(requestError));
      report(requestError, request);
    } finally {
      if (isCurrent(request)) setLoading(false);
    }
  }, [start, isCurrent, report]);

  useEffect(() => {
    setInsights([]);
    setDashboards([]);
    void load();
  }, [state.activeProjectId, state.requestEpoch, load]);

  const remove = async (insightId: string) => {
    const request = start();
    if (!request) return;
    try {
      await api.deleteInsight(request.identity.projectId, insightId, request.signal);
      if (isCurrent(request)) void load();
    } catch (requestError) {
      if (!isCurrent(request)) return;
      setError(errorMessage(requestError));
      report(requestError, request);
    }
  };

  const pin = async (insightId: string, dashboardId: string) => {
    const dashboard = dashboards.find((item) => item.id === dashboardId);
    if (!dashboard || dashboard.tiles.some((tile) => tile.insight_id === insightId)) return;
    const request = start();
    if (!request) return;
    const tiles = [
      ...dashboard.tiles,
      { insight_id: insightId, x: 0, y: dashboard.tiles.length * 3, w: 4, h: 3 },
    ];
    try {
      await api.updateDashboardTiles(
        request.identity.projectId,
        dashboardId,
        tiles,
        request.signal,
      );
      if (isCurrent(request)) void load();
    } catch (requestError) {
      if (!isCurrent(request)) return;
      setError(errorMessage(requestError));
      report(requestError, request);
    }
  };

  return (
    <section className="panel">
      <h2>Saved insights</h2>
      <p className="empty">Create truthful Trends insights from the Trends tab.</p>
      {error ? <p style={{ color: "#e55" }}>{error}</p> : null}
      {loading ? <div className="empty">Loading insights…</div> : null}
      {insights.map((insight) => (
        <div className="row" key={insight.id} style={{ flexWrap: "wrap" }}>
          <strong>{insight.name}</strong>
          <span className="did">{insight.query_ir.kind}</span>
          {dashboards.length ? (
            <select
              aria-label={`Pin ${insight.name} to dashboard`}
              defaultValue=""
              onChange={(event) => {
                if (event.target.value) void pin(insight.id, event.target.value);
                event.target.value = "";
              }}
            >
              <option value="">Pin to…</option>
              {dashboards.map((dashboard) => (
                <option key={dashboard.id} value={dashboard.id}>
                  {dashboard.name}
                </option>
              ))}
            </select>
          ) : null}
          <button onClick={() => void remove(insight.id)}>Delete</button>
        </div>
      ))}
      {!loading && !insights.length ? <div className="empty">No saved insights.</div> : null}
    </section>
  );
}

function Dashboards({
  state,
  onProjectError,
}: {
  state: ReadyWorkspaceState;
  onProjectError: ProjectErrorHandler;
}) {
  const { start, isCurrent, report } = useProjectRequests(state, onProjectError);
  const [dashboards, setDashboards] = useState<Dashboard[]>([]);
  const [name, setName] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    const request = start();
    if (!request) return;
    setLoading(true);
    try {
      const response = await api.listDashboards(request.identity.projectId, request.signal);
      if (isCurrent(request)) setDashboards(response);
    } catch (requestError) {
      if (!isCurrent(request)) return;
      setError(errorMessage(requestError));
      report(requestError, request);
    } finally {
      if (isCurrent(request)) setLoading(false);
    }
  }, [start, isCurrent, report]);

  useEffect(() => {
    setDashboards([]);
    setError("");
    void load();
  }, [state.activeProjectId, state.requestEpoch, load]);

  const create = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (!name.trim()) return;
    const request = start();
    if (!request) return;
    try {
      await api.saveDashboard(request.identity.projectId, { name: name.trim() }, request.signal);
      if (isCurrent(request)) {
        setName("");
        void load();
      }
    } catch (requestError) {
      if (!isCurrent(request)) return;
      setError(errorMessage(requestError));
      report(requestError, request);
    }
  };

  const remove = async (dashboardId: string) => {
    const request = start();
    if (!request) return;
    try {
      await api.deleteDashboard(request.identity.projectId, dashboardId, request.signal);
      if (isCurrent(request)) void load();
    } catch (requestError) {
      if (!isCurrent(request)) return;
      setError(errorMessage(requestError));
      report(requestError, request);
    }
  };

  return (
    <section className="panel">
      <h2>Dashboards</h2>
      <form className="controls" onSubmit={create}>
        <input
          placeholder="Dashboard name"
          value={name}
          onChange={(event) => setName(event.target.value)}
        />
        <button type="submit" disabled={!name.trim()}>
          Create
        </button>
      </form>
      {error ? <p style={{ color: "#e55" }}>{error}</p> : null}
      {loading ? <div className="empty">Loading dashboards…</div> : null}
      {dashboards.map((dashboard) => (
        <div
          key={dashboard.id}
          style={{ background: "var(--panel2)", borderRadius: 8, padding: 12, marginBottom: 8 }}
        >
          <div style={{ display: "flex", alignItems: "center" }}>
            <strong>{dashboard.name}</strong>
            <span className="did" style={{ marginLeft: 8 }}>
              {dashboard.tiles.length} tiles
            </span>
            <button style={{ marginLeft: "auto" }} onClick={() => void remove(dashboard.id)}>
              Delete
            </button>
          </div>
          {dashboard.tiles.length ? (
            dashboard.tiles.map((tile) => (
              <div className="row" key={tile.insight_id}>
                {tile.insight?.name ?? tile.insight_id}
              </div>
            ))
          ) : (
            <div className="empty">Pin insights from the Insights tab.</div>
          )}
        </div>
      ))}
      {!loading && !dashboards.length ? <div className="empty">No dashboards yet.</div> : null}
    </section>
  );
}
