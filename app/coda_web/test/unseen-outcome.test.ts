import { expect, test } from "vitest";

import { reconcileRunningWithUnseenOutcome, type OpenedSession } from "../src/store/session.ts";

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
// stale `running: true` forever once the tab stops receiving its events
// (switched away, or a dropped `session_status` push). A non-null
// `unseen_outcome` is proof the turn is over, from wherever the client
// learned it — catalog fetch or live push — so it always wins over a stale
// local `running`.
test("a non-null outcome clears a stale running flag", () => {
  const session = openedSession({ running: true });
  expect(reconcileRunningWithUnseenOutcome(session, "completed")).toEqual({
    ...session,
    running: false,
  });
  expect(reconcileRunningWithUnseenOutcome(session, "failed")).toEqual({
    ...session,
    running: false,
  });
});

test("no outcome leaves the session untouched, same reference", () => {
  const session = openedSession({ running: true });
  expect(reconcileRunningWithUnseenOutcome(session, null)).toBe(session);
  expect(reconcileRunningWithUnseenOutcome(session, undefined)).toBe(session);
});

test("an outcome on an already-idle session is a no-op, same reference", () => {
  // Not just an optimization: returning the identical object (rather than an
  // equal-but-new one) is what keeps a Zustand selector from thinking this
  // session changed and re-rendering it for nothing.
  const session = openedSession({ running: false });
  expect(reconcileRunningWithUnseenOutcome(session, "completed")).toBe(session);
});
