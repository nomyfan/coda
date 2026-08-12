import { expect, test } from "vitest";

import { applySnapshotToSession, reduceEvent, type OpenedSession } from "../src/store/session.ts";
import type { AssistantMessage } from "../src/lib/protocol.ts";

const at = (seconds: number) => new Date(Date.UTC(2026, 0, 1, 0, 0, seconds)).toISOString();

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

function answer(id: string): AssistantMessage {
  return {
    message_id: id,
    content: "done",
    tool_calls: [],
    usage: null,
    reasoning_content: null,
    reasoning_ended_at: null,
    aborted: false,
    started_at: at(0),
    ended_at: at(1),
  } as AssistantMessage;
}

test("a persist failure does not end the turn", () => {
  const running = { ...session(), running: true };

  const after = reduceEvent(running, {
    type: "persist_failed",
    agent_name: "coda",
    thread_id: "s1",
    message: "storage is unavailable",
  });

  expect(after.persistError).toBe("storage is unavailable");
  expect(after.running).toBe(true);
});

test("the notice survives the reattach the failure itself causes", () => {
  // The server drops the session right after reporting the failure, and the
  // snapshot the client comes back with replaces the transcript wholesale — so
  // a notice kept in `entries` would be wiped by the very resync it reports on.
  const failed = reduceEvent(session(), {
    type: "persist_failed",
    agent_name: "coda",
    thread_id: "s1",
    message: "storage is unavailable",
  });

  const reattached = applySnapshotToSession(failed, {
    messages: [],
    approvals: [],
    providerId: "p",
    reasoningEffort: null,
    turnRunning: false,
  });

  expect(reattached.persistError).toBe("storage is unavailable");
});

test("a later turn that finishes normally clears the notice", () => {
  const failed = reduceEvent(session(), {
    type: "persist_failed",
    agent_name: "coda",
    thread_id: "s1",
    message: "storage is unavailable",
  });

  const recovered = reduceEvent(failed, {
    type: "llm_end",
    agent_name: "coda",
    thread_id: "s1",
    message: answer("m1"),
  });

  expect(recovered.persistError).toBeUndefined();
});

test("a sub-agent finishing is not the turn finishing, so the notice stays", () => {
  const failed = reduceEvent(session(), {
    type: "persist_failed",
    agent_name: "coda",
    thread_id: "s1",
    message: "storage is unavailable",
  });

  const stillOpen = reduceEvent(failed, {
    type: "llm_end",
    agent_name: "explore",
    thread_id: "sub",
    message: answer("m1"),
  });

  expect(stillOpen.persistError).toBe("storage is unavailable");
});

test("an aborted generation is not a turn ending, so the notice stays", () => {
  // The driver emits the partial message as an `llm_end` with `aborted` set and
  // no tool calls, then the `aborted` event that actually settles the turn. The
  // server does not count the former as an ending; neither may the client, or a
  // cancelled generation would pass for a stored one.
  const failed = reduceEvent(session(), {
    type: "persist_failed",
    agent_name: "coda",
    thread_id: "s1",
    message: "storage is unavailable",
  });

  const interrupted = reduceEvent(failed, {
    type: "llm_end",
    agent_name: "coda",
    thread_id: "s1",
    message: { ...answer("m1"), aborted: true },
  });

  expect(interrupted.persistError).toBe("storage is unavailable");
});
