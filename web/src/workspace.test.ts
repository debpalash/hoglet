import {
  activeProjectPreference,
  beginProjectRequest,
  currentProjectRequest,
  decodeActiveProjectPreference,
  initialWorkspaceState,
  isCurrentProjectRequest,
  readActiveProjectPreference,
  selectActiveProject,
  workspaceReducer,
  type ApiError,
  type Workspace,
  type WorkspaceState,
} from "./workspace";

const aliceWorkspace: Workspace = {
  user: { id: "user-alice", email: "alice@example.com", name: "Alice" },
  organizations: [
    {
      id: "org-one",
      name: "One",
      role: "owner",
      projects: [
        { id: "project-one", name: "One", token: "phc_one" },
        { id: "project-two", name: "Two", token: "phc_two" },
      ],
    },
  ],
};

const noProjectWorkspace: Workspace = {
  user: { id: "user-empty", email: "empty@example.com", name: "Empty" },
  organizations: [{ id: "org-empty", name: "Empty", role: "owner", projects: [] }],
};

const unavailableError: ApiError = {
  code: "unavailable",
  message: "Control plane unavailable",
  request_id: "request-1",
};

function assert(condition: boolean, message: string): void {
  if (!condition) throw new Error(message);
}

function assertStatus(state: WorkspaceState, status: WorkspaceState["status"]): void {
  assert(state.status === status, `expected ${status}, received ${state.status}`);
}

export interface WorkspaceTableTest {
  name: string;
  run: () => void;
}

export const workspaceTableTests: WorkspaceTableTest[] = [
  {
    name: "preference decoder rejects malformed, stale-schema, and empty data",
    run: () => {
      assert(decodeActiveProjectPreference("not json") === null, "malformed JSON was accepted");
      assert(
        decodeActiveProjectPreference('{"version":2,"userId":"user-alice","projectId":"project-two"}') ===
          null,
        "unknown schema version was accepted",
      );
      assert(
        decodeActiveProjectPreference('{"version":1,"userId":"","projectId":"project-two"}') === null,
        "empty user id was accepted",
      );
    },
  },
  {
    name: "preference reader contains storage failures behind its injected boundary",
    run: () => {
      const preference = readActiveProjectPreference(() => {
        throw new Error("storage disabled");
      });
      assert(preference === null, "storage failure escaped the preference boundary");
    },
  },
  {
    name: "active project uses only a valid preference for the current user",
    run: () => {
      const preferred = selectActiveProject(
        aliceWorkspace,
        activeProjectPreference("user-alice", "project-two"),
      );
      assert(preferred?.id === "project-two", "valid preference was not selected");

      const otherUser = selectActiveProject(
        aliceWorkspace,
        activeProjectPreference("user-other", "project-two"),
      );
      assert(otherUser?.id === "project-one", "another user's preference leaked across sessions");

      const missingProject = selectActiveProject(
        aliceWorkspace,
        activeProjectPreference("user-alice", "project-missing"),
      );
      assert(missingProject?.id === "project-one", "missing preference did not fall back deterministically");
    },
  },
  {
    name: "bootstrap maps setup, auth, empty, and unavailable results to explicit states",
    run: () => {
      const setup = workspaceReducer(initialWorkspaceState, {
        type: "bootstrapResult",
        requestEpoch: 0,
        result: { kind: "setupRequired" },
      });
      assertStatus(setup, "setupRequired");

      const unauthenticated = workspaceReducer(initialWorkspaceState, {
        type: "bootstrapResult",
        requestEpoch: 0,
        result: { kind: "unauthenticated" },
      });
      assertStatus(unauthenticated, "unauthenticated");

      const empty = workspaceReducer(initialWorkspaceState, {
        type: "bootstrapResult",
        requestEpoch: 0,
        result: { kind: "workspace", workspace: noProjectWorkspace, preference: null },
      });
      assertStatus(empty, "readyNoProject");

      const unavailable = workspaceReducer(initialWorkspaceState, {
        type: "bootstrapResult",
        requestEpoch: 0,
        result: { kind: "unavailable", error: unavailableError },
      });
      assertStatus(unavailable, "unavailable");
    },
  },
  {
    name: "stale bootstrap responses cannot overwrite a retry",
    run: () => {
      const retrying = workspaceReducer(initialWorkspaceState, { type: "retry" });
      const stale = workspaceReducer(retrying, {
        type: "bootstrapResult",
        requestEpoch: 0,
        result: { kind: "setupRequired" },
      });
      assert(stale === retrying, "stale bootstrap response changed state");
    },
  },
  {
    name: "project switches advance the epoch and invalidate earlier responses",
    run: () => {
      const ready = workspaceReducer(initialWorkspaceState, {
        type: "authenticatedWorkspace",
        workspace: aliceWorkspace,
        preference: null,
      });
      assertStatus(ready, "ready");
      const firstRequest = currentProjectRequest(ready);
      assert(firstRequest !== null, "ready workspace did not provide request identity");

      const switched = workspaceReducer(ready, { type: "projectSelected", projectId: "project-two" });
      assertStatus(switched, "ready");
      assert(
        firstRequest !== null && !isCurrentProjectRequest(switched, firstRequest),
        "previous-project response remained current",
      );

      const unchanged = workspaceReducer(switched, {
        type: "projectSelected",
        projectId: "project-missing",
      });
      assert(unchanged === switched, "unknown project id changed state");
    },
  },
  {
    name: "starting a new project request aborts the previous fetch group",
    run: () => {
      const ready = workspaceReducer(initialWorkspaceState, {
        type: "authenticatedWorkspace",
        workspace: aliceWorkspace,
        preference: null,
      });
      const first = beginProjectRequest(ready);
      assert(first !== null, "request was not created for ready state");
      const second = beginProjectRequest(ready, first ?? undefined);
      assert(first?.signal.aborted === true, "previous request was not aborted");
      assert(second?.signal.aborted === false, "new request began aborted");
    },
  },
  {
    name: "stale forbidden and unavailable project responses are ignored",
    run: () => {
      const ready = workspaceReducer(initialWorkspaceState, {
        type: "authenticatedWorkspace",
        workspace: aliceWorkspace,
        preference: null,
      });
      const oldRequest = currentProjectRequest(ready);
      assert(oldRequest !== null, "missing project request identity");
      const switched = workspaceReducer(ready, { type: "projectSelected", projectId: "project-two" });
      if (oldRequest === null) return;

      const forbidden = workspaceReducer(switched, {
        type: "forbiddenWorkspaceRefresh",
        request: oldRequest,
      });
      assert(forbidden === switched, "stale forbidden response triggered a workspace refresh");

      const unavailable = workspaceReducer(switched, {
        type: "unavailable",
        error: unavailableError,
        request: oldRequest,
      });
      assert(unavailable === switched, "stale unavailable response replaced current workspace");
    },
  },
  {
    name: "logout clears workspace context and advances the request epoch",
    run: () => {
      const ready = workspaceReducer(initialWorkspaceState, {
        type: "authenticatedWorkspace",
        workspace: aliceWorkspace,
        preference: null,
      });
      const loggedOut = workspaceReducer(ready, { type: "logout" });
      assertStatus(loggedOut, "unauthenticated");
      assert(loggedOut.requestEpoch > ready.requestEpoch, "logout did not invalidate pending requests");
    },
  },
];

/** Framework-agnostic entry point; a future test runner can invoke the same table unchanged. */
export function runWorkspaceTableTests(): void {
  for (const test of workspaceTableTests) test.run();
}
