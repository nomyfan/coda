import { expect, test } from "vitest";

import { applySnapshotToSession, reduceEvent, type OpenedSession } from "../src/store/session.ts";
import type { PendingApproval, WireEvent } from "../src/lib/protocol.ts";

import serializedEvents from "./fixtures/background-events.json";
const controlEvents = serializedEvents as WireEvent[];

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
    task_id: null,
    agent_path: ["coda"],
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

test("background approvals preserve root execution and root abort preserves their drafts", () => {
  const pending: PendingApproval = {
    ...approval("child"),
    task_id: "bg_task",
    agent_path: ["coda", "worker", "child"],
    parent_message_id: "batch-child",
  };
  const started = session({
    running: true,
    drafts: { "child:batch-child": { call: "Execute" } },
    allowDrafts: { "child:batch-child": { call: "cargo test" } },
  });
  const suspended = reduceEvent(started, {
    type: "suspended",
    agent_name: "child",
    thread_id: "child",
    approval: pending,
  });
  expect(suspended.running).toBe(true);
  const aborted = reduceEvent(suspended, {
    type: "aborted",
    agent_name: "coda",
    thread_id: "s1",
    target: { reason: "generation" },
  });
  expect(aborted.approvals).toEqual([pending]);
  expect(aborted.drafts["child:batch-child"]).toEqual({ call: "Execute" });
  expect(aborted.running).toBe(false);
});

test("removing one approval batch leaves another thread and reused call ID intact", () => {
  const a: PendingApproval = {
    ...approval("a"),
    task_id: "bg_a",
    agent_path: ["coda", "worker"],
    parent_message_id: "batch-a",
  };
  const b: PendingApproval = {
    ...approval("b"),
    task_id: "bg_b",
    agent_path: ["coda", "worker"],
    parent_message_id: "batch-b",
  };
  const before = session({
    approvals: [a, b],
    drafts: { "a:batch-a": { call: "Execute" }, "b:batch-b": { call: "Execute" } },
  });
  const after = reduceEvent(before, {
    type: "approval_removed",
    agent_name: "worker",
    thread_id: "a",
    parent_message_id: "batch-a",
    task_id: "bg_a",
  });
  expect(after.approvals).toEqual([b]);
  expect(after.drafts).toEqual({ "b:batch-b": { call: "Execute" } });
  const stale = reduceEvent(after, {
    type: "approval_removed",
    agent_name: "worker",
    thread_id: "b",
    parent_message_id: "older-batch",
    task_id: "bg_b",
  });
  expect(stale.approvals).toEqual([b]);
});

// The server's wire test checks its actual serde output against this same JSON.
test("serialized control events remove foreground/background approvals without losing the session", () => {
  const approvals = controlEvents
    .filter((event) => event.type === "approval_removed")
    .map((event) => ({
      ...approval(event.thread_id),
      parent_message_id: event.parent_message_id,
      task_id: event.task_id,
    }));
  let current = session({ approvals, running: true });
  for (const event of controlEvents) {
    current = reduceEvent(current, event);
    expect(current).toBeDefined();
    expect(current.running).toBe(true);
  }
  expect(current.approvals).toEqual([]);
  expect(JSON.stringify(current.activity)).toContain("checkpoint failed");
});

test.each(["abort", "complete"])(
  "a snapshot after root %s preserves only still-pending approval drafts",
  (ending) => {
    const batch: PendingApproval = {
      ...approval("child"),
      task_id: "bg_task",
      calls: [
        { id: "approve", name: "shell", arguments: "{}" },
        { id: "reject", name: "shell", arguments: "{}" },
        { id: "answered", name: "shell", arguments: "{}" },
      ],
    };
    const before = session({
      approvals: [batch],
      running: true,
      drafts: {
        "child:m-batch": {
          approve: "Execute",
          reject: { Rejected: { reason: "not needed" } },
          answered: "Execute",
        },
        "old:m-batch": { approve: "Execute" },
      },
      allowDrafts: {
        "child:m-batch": { approve: "cargo test", answered: "ls *" },
        "old:m-batch": { approve: "pwd" },
      },
    });
    const settled =
      ending === "abort"
        ? reduceEvent(before, {
            type: "aborted",
            agent_name: "coda",
            thread_id: "s1",
            target: { reason: "generation" },
          })
        : reduceEvent(before, {
            type: "llm_end",
            agent_name: "coda",
            thread_id: "s1",
            message: {
              message_id: "done",
              content: "done",
              tool_calls: [],
              started_at: "2026-09-05T00:00:00Z",
              ended_at: "2026-09-05T00:00:01Z",
            },
          });
    const snapshot = {
      messages: [],
      approvals: [{ ...batch, calls: batch.calls.slice(0, 2) }],
      providerId: "test",
      reasoningEffort: null,
      permissionMode: "accept_edits" as const,
      turnRunning: false,
    };
    const after = applySnapshotToSession(settled, snapshot);
    expect(after.drafts).toEqual({
      "child:m-batch": { approve: "Execute", reject: { Rejected: { reason: "not needed" } } },
    });
    expect(after.allowDrafts).toEqual({ "child:m-batch": { approve: "cargo test" } });
    expect(after.running).toBe(false);
    const replaced = applySnapshotToSession(after, {
      ...snapshot,
      approvals: [{ ...batch, parent_message_id: "new-batch" }],
    });
    expect(replaced.drafts).toEqual({});
    expect(replaced.allowDrafts).toEqual({});
  },
);
