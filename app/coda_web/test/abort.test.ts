import { expect, test } from "vitest";

import { reduceEvent, type OpenedSession } from "../src/store/session.ts";
import type { PendingApproval } from "../src/lib/protocol.ts";

function session(overrides: Partial<OpenedSession> = {}): OpenedSession {
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
    running: false,
    evicted: false,
    usage: [],
    ...overrides,
  } as OpenedSession;
}

function approval(threadId: string): PendingApproval {
  return {
    thread_id: threadId,
    agent_name: "coda",
    parent_message_id: "m-batch",
    calls: [],
    suspended_at: new Date().toISOString(),
    suggested_shell_allow_patterns: {},
  };
}

test("a root abort buries the turn's pending approval", () => {
  // Mirrors the server: the settlement that ends the turn clears its
  // approvals, or the composer would stay busy on a decision nobody can take.
  const suspended = session({
    approvals: [approval("s1")],
    drafts: { "s1/coda": { call_1: "Execute" } },
    allowDrafts: { "s1/coda": { call_1: "git *" } },
    pendingCallInfo: { call_1: { id: "call_1", name: "shell", arguments: "{}" } },
  });

  const after = reduceEvent(suspended, {
    type: "aborted",
    agent_name: "coda",
    thread_id: "s1",
    target: { reason: "generation" },
  });

  expect(after.approvals).toEqual([]);
  expect(after.drafts).toEqual({});
  expect(after.allowDrafts).toEqual({});
  expect(after.pendingCallInfo).toEqual({});
  expect(after.running).toBe(false);
});

test("a root error buries the turn's pending approval too", () => {
  const suspended = session({ approvals: [approval("s1")] });

  const after = reduceEvent(suspended, {
    type: "error",
    agent_name: "coda",
    thread_id: "s1",
    message: "provider unreachable",
  });

  expect(after.approvals).toEqual([]);
});

test("a sub-agent abort settles nothing and keeps approvals", () => {
  const suspended = session({ approvals: [approval("child-thread")] });

  const after = reduceEvent(suspended, {
    type: "aborted",
    agent_name: "explore",
    thread_id: "child-thread",
    target: { reason: "generation" },
  });

  expect(after.approvals).toHaveLength(1);
});
