import { expect, test } from "vitest";

import {
  codaStore,
  openSession,
  reconcileRunningWithStatus,
  type OpenedSession,
} from "../src/store/session.ts";
import type { WorkspaceSummary } from "../src/lib/protocol.ts";

function openedSession(overrides: Partial<OpenedSession> = {}): OpenedSession {
  return {
    key: "ws/s1",
    workspaceId: "ws",
    sessionId: "s1",
    entries: [],
    activity: [],
    approvals: [],
    pendingCallInfo: {},
    generationSpans: {},
    drafts: {},
    allowDrafts: {},
    running: true,
    evicted: false,
    usage: [],
    ...overrides,
  };
}

// The bug this exists to fix: a session opened once in this tab keeps a
// stale `running` forever once the tab stops receiving its events (switched
// away, or a dropped `session_status` push, or a reconnect that doesn't
// reopen every session). The catalog's `status` is proof of the real state,
// from wherever the client learned it, so it always wins over a stale local
// `running` — in either direction.
test("a settled status clears a stale running flag", () => {
  const session = openedSession({ running: true });
  expect(reconcileRunningWithStatus(session, "completed")).toEqual({
    ...session,
    running: false,
  });
  expect(reconcileRunningWithStatus(session, "failed")).toEqual({
    ...session,
    running: false,
  });
});

test('a "running" status sets a stale running flag back on', () => {
  // E.g. this tab opened the session, watched it go idle, then another tab
  // started a new task on it while this tab wasn't attached.
  const session = openedSession({ running: false });
  expect(reconcileRunningWithStatus(session, "running")).toEqual({
    ...session,
    running: true,
  });
});

test("an explicit null (server-confirmed idle) clears a stale running flag", () => {
  const session = openedSession({ running: true });
  expect(reconcileRunningWithStatus(session, null)).toEqual({
    ...session,
    running: false,
  });
});

test("undefined (no real catalog data for this row) leaves the session untouched, same reference", () => {
  // Distinct from an explicit `null`: this is what a locally-synthesized
  // "extras" catalog row (`mergeCatalog`) looks like before the server has
  // ever confirmed anything about it — nothing to reconcile against yet.
  const session = openedSession({ running: true });
  expect(reconcileRunningWithStatus(session, undefined)).toBe(session);
});

test("a status that already matches running is a no-op, same reference", () => {
  // Not just an optimization: returning the identical object (rather than an
  // equal-but-new one) is what keeps a Zustand selector from thinking this
  // session changed and re-rendering it for nothing.
  const idle = openedSession({ running: false });
  expect(reconcileRunningWithStatus(idle, "completed")).toBe(idle);

  const running = openedSession({ running: true });
  expect(reconcileRunningWithStatus(running, "running")).toBe(running);
});

function catalogWithStatus(status: "running" | "completed" | "failed" | null): WorkspaceSummary[] {
  return [
    {
      id: "ws",
      path: "/tmp/ws",
      sessions: [{ id: "s1", name: null, has_pending_approval: false, status }],
    },
  ];
}

function seedServerForOpen(server: string, status: "running" | "completed" | "failed" | null) {
  codaStore.setState((state) => {
    state.servers[server] = {
      url: server,
      status: "connected",
      catalog: catalogWithStatus(status),
      providers: [],
      sessions: {},
    };
    state.rpcMap[server] = {
      notify: () => true,
      // Leaves `open_session` pending and off the wire — this test only cares
      // about the synchronous optimistic patch `openSession` applies first.
      request: () => new Promise(() => {}),
    } as never;
  });
}

function catalogStatus(server: string) {
  return codaStore
    .getState()
    .servers[server]?.catalog.find((workspace) => workspace.id === "ws")
    ?.sessions.find((session) => session.id === "s1")?.status;
}

function teardownServer(server: string) {
  codaStore.setState((state) => {
    delete state.rpcMap[server];
    delete state.servers[server];
  });
}

// Attach only ever clears a *settled* outcome — it never changes whether a
// session is actually running — so opening a session the catalog already
// reports as "running" must leave that status alone rather than optimistically
// stomping it to null.
test("opening a running session does not clear its catalog status", () => {
  const server = "ws://open-running";
  seedServerForOpen(server, "running");

  openSession(server, "ws", "s1");

  expect(catalogStatus(server)).toBe("running");
  teardownServer(server);
});

test("opening a session with a settled status optimistically clears it", () => {
  const server = "ws://open-settled";
  seedServerForOpen(server, "completed");

  openSession(server, "ws", "s1");

  expect(catalogStatus(server)).toBeNull();
  teardownServer(server);
});
