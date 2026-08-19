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
  const session = openedSession({ running: true });
  expect(reconcileRunningWithStatus(session, undefined)).toBe(session);
});

test("a status that already matches running is a no-op, same reference", () => {
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
      request: () => new Promise(() => {}), // leaves open_session pending
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
