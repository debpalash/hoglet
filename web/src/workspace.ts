export type OrganizationRole = "owner" | "admin" | "member";

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
  role: OrganizationRole;
  projects: Project[];
}

export interface Workspace {
  user: User;
  organizations: Organization[];
}

interface ApiErrorBase {
  message: string;
  request_id: string;
}

type FieldApiErrorCode = "invalid_request" | "unsupported_query" | "invalid_query";
type PlainApiErrorCode =
  | "unauthorized"
  | "forbidden"
  | "not_found"
  | "conflict"
  | "timeout"
  | "internal_error"
  | "unavailable";

export type ApiError =
  | (ApiErrorBase & { code: FieldApiErrorCode; field?: string })
  | (ApiErrorBase & { code: PlainApiErrorCode })
  | (ApiErrorBase & { code: "query_busy" });

export interface ApiErrorEnvelope {
  error: ApiError;
}

export const ACTIVE_PROJECT_PREFERENCE_VERSION = 1 as const;
export const ACTIVE_PROJECT_STORAGE_KEY = "hoglet.workspace.active-project";

export interface ActiveProjectPreference {
  version: typeof ACTIVE_PROJECT_PREFERENCE_VERSION;
  userId: string;
  projectId: string;
}

export type PreferenceReader = (key: string) => string | null;

export function decodeActiveProjectPreference(raw: string | null): ActiveProjectPreference | null {
  if (raw === null) return null;

  try {
    const candidate: unknown = JSON.parse(raw);
    if (typeof candidate !== "object" || candidate === null) return null;

    const value = candidate as Record<string, unknown>;
    if (
      value.version !== ACTIVE_PROJECT_PREFERENCE_VERSION ||
      typeof value.userId !== "string" ||
      value.userId.length === 0 ||
      typeof value.projectId !== "string" ||
      value.projectId.length === 0
    ) {
      return null;
    }

    return {
      version: ACTIVE_PROJECT_PREFERENCE_VERSION,
      userId: value.userId,
      projectId: value.projectId,
    };
  } catch {
    return null;
  }
}

/** Reads through an injected boundary so reducers and server rendering never touch localStorage. */
export function readActiveProjectPreference(
  read: PreferenceReader,
  key = ACTIVE_PROJECT_STORAGE_KEY,
): ActiveProjectPreference | null {
  try {
    return decodeActiveProjectPreference(read(key));
  } catch {
    return null;
  }
}

export function activeProjectPreference(userId: string, projectId: string): ActiveProjectPreference {
  return { version: ACTIVE_PROJECT_PREFERENCE_VERSION, userId, projectId };
}

export type RequestEpoch = number;

interface StateBase {
  /** Changes whenever auth, workspace, or active-project context changes. */
  requestEpoch: RequestEpoch;
}

export type WorkspaceState =
  | (StateBase & {
      status: "bootstrapping";
      reason: "initial" | "retry" | "forbiddenWorkspaceRefresh";
    })
  | (StateBase & { status: "setupRequired" })
  | (StateBase & { status: "unauthenticated" })
  | (StateBase & { status: "readyNoProject"; workspace: Workspace })
  | (StateBase & { status: "ready"; workspace: Workspace; activeProjectId: string })
  | (StateBase & { status: "unavailable"; error: ApiError });

export const initialWorkspaceState: WorkspaceState = {
  status: "bootstrapping",
  reason: "initial",
  requestEpoch: 0,
};

export type BootstrapResult =
  | { kind: "setupRequired" }
  | { kind: "unauthenticated" }
  | { kind: "workspace"; workspace: Workspace; preference: ActiveProjectPreference | null }
  | { kind: "unavailable"; error: ApiError };

export interface ProjectRequestIdentity {
  projectId: string;
  requestEpoch: RequestEpoch;
}

export type WorkspaceEvent =
  | { type: "bootstrapResult"; requestEpoch: RequestEpoch; result: BootstrapResult }
  | {
      type: "authenticatedWorkspace";
      workspace: Workspace;
      preference: ActiveProjectPreference | null;
    }
  | { type: "sessionExpired" }
  | { type: "forbiddenWorkspaceRefresh"; request: ProjectRequestIdentity }
  | { type: "projectSelected"; projectId: string }
  | { type: "unavailable"; error: ApiError; request?: ProjectRequestIdentity }
  | { type: "retry" }
  | { type: "logout" };

function nextEpoch(epoch: RequestEpoch): RequestEpoch {
  return epoch >= Number.MAX_SAFE_INTEGER ? 0 : epoch + 1;
}

export function projectsIn(workspace: Workspace): Project[] {
  return workspace.organizations.flatMap((organization) => organization.projects);
}

export function selectActiveProject(
  workspace: Workspace,
  preference: ActiveProjectPreference | null,
): Project | null {
  const projects = projectsIn(workspace);
  if (projects.length === 0) return null;

  if (preference !== null && preference.userId === workspace.user.id) {
    const preferredProject = projects.find((project) => project.id === preference.projectId);
    if (preferredProject !== undefined) return preferredProject;
  }

  return projects[0] ?? null;
}

function readyState(
  workspace: Workspace,
  preference: ActiveProjectPreference | null,
  requestEpoch: RequestEpoch,
): WorkspaceState {
  const activeProject = selectActiveProject(workspace, preference);
  return activeProject === null
    ? { status: "readyNoProject", workspace, requestEpoch }
    : { status: "ready", workspace, activeProjectId: activeProject.id, requestEpoch };
}

function applyBootstrapResult(state: WorkspaceState, result: BootstrapResult): WorkspaceState {
  const requestEpoch = nextEpoch(state.requestEpoch);
  switch (result.kind) {
    case "setupRequired":
      return { status: "setupRequired", requestEpoch };
    case "unauthenticated":
      return { status: "unauthenticated", requestEpoch };
    case "workspace":
      return readyState(result.workspace, result.preference, requestEpoch);
    case "unavailable":
      return { status: "unavailable", error: result.error, requestEpoch };
  }
}

export function currentProjectRequest(state: WorkspaceState): ProjectRequestIdentity | null {
  return state.status === "ready"
    ? { projectId: state.activeProjectId, requestEpoch: state.requestEpoch }
    : null;
}

export function isCurrentProjectRequest(
  state: WorkspaceState,
  request: ProjectRequestIdentity,
): boolean {
  const current = currentProjectRequest(state);
  return (
    current !== null &&
    current.projectId === request.projectId &&
    current.requestEpoch === request.requestEpoch
  );
}

export interface AbortableProjectRequest {
  identity: ProjectRequestIdentity;
  signal: AbortSignal;
  abort: () => void;
}

/** Aborts the previous project fetch group and snapshots the current project/epoch. */
export function beginProjectRequest(
  state: WorkspaceState,
  previous?: Pick<AbortableProjectRequest, "abort">,
): AbortableProjectRequest | null {
  previous?.abort();
  const identity = currentProjectRequest(state);
  if (identity === null) return null;

  const controller = new AbortController();
  return {
    identity,
    signal: controller.signal,
    abort: () => controller.abort(),
  };
}

export function workspaceReducer(state: WorkspaceState, event: WorkspaceEvent): WorkspaceState {
  switch (event.type) {
    case "bootstrapResult":
      if (state.status !== "bootstrapping" || event.requestEpoch !== state.requestEpoch) return state;
      return applyBootstrapResult(state, event.result);

    case "authenticatedWorkspace":
      return readyState(event.workspace, event.preference, nextEpoch(state.requestEpoch));

    case "sessionExpired":
    case "logout":
      return { status: "unauthenticated", requestEpoch: nextEpoch(state.requestEpoch) };

    case "forbiddenWorkspaceRefresh":
      if (!isCurrentProjectRequest(state, event.request)) return state;
      return {
        status: "bootstrapping",
        reason: "forbiddenWorkspaceRefresh",
        requestEpoch: nextEpoch(state.requestEpoch),
      };

    case "projectSelected": {
      if (state.status !== "ready" || event.projectId === state.activeProjectId) return state;
      const projectExists = projectsIn(state.workspace).some((project) => project.id === event.projectId);
      return projectExists
        ? {
            ...state,
            activeProjectId: event.projectId,
            requestEpoch: nextEpoch(state.requestEpoch),
          }
        : state;
    }

    case "unavailable":
      if (event.request !== undefined && !isCurrentProjectRequest(state, event.request)) return state;
      return {
        status: "unavailable",
        error: event.error,
        requestEpoch: nextEpoch(state.requestEpoch),
      };

    case "retry":
      return {
        status: "bootstrapping",
        reason: "retry",
        requestEpoch: nextEpoch(state.requestEpoch),
      };
  }
}
