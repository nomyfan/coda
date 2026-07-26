import { expect, test } from "vitest";

import {
  applyRewound,
  discardedFrom,
  reconcileEditing,
  type OpenedSession,
  type TranscriptEntry,
} from "../src/store/session.ts";
import type { HistoryMessage } from "../src/lib/protocol.ts";

function userMessage(id: string, text: string): HistoryMessage {
  return {
    User: {
      message_id: id,
      parts: [{ type: "text", text }],
      created_at: "2026-07-26T00:00:00Z",
    },
  } as HistoryMessage;
}

function assistantMessage(id: string, content: string, promptTokens?: number): HistoryMessage {
  return {
    Assistant: {
      message_id: id,
      content,
      tool_calls: [],
      usage: promptTokens
        ? { prompt_tokens: promptTokens, completion_tokens: 0, total_tokens: promptTokens }
        : null,
      reasoning_content: null,
      reasoning_ended_at: null,
      aborted: false,
      started_at: "2026-07-26T00:00:00Z",
      ended_at: "2026-07-26T00:00:01Z",
    },
  } as HistoryMessage;
}

function session(overrides: Partial<OpenedSession> = {}): OpenedSession {
  return {
    key: "ws/s1",
    workspaceId: "ws",
    sessionId: "s1",
    entries: [],
    activity: [],
    approvals: [],
    pendingCallInfo: {},
    drafts: {},
    allowDrafts: {},
    running: false,
    evicted: false,
    usage: [],
    ...overrides,
  } as OpenedSession;
}

function userEntry(messageId: string, content: string): TranscriptEntry {
  return { id: `user:${messageId}`, messageId, kind: "user", content };
}

test("discardedFrom points at the target entry, and nowhere when it is gone", () => {
  const entries = [
    userEntry("m1", "first"),
    { id: "assistant:a1", kind: "assistant", content: "ok" } as TranscriptEntry,
    userEntry("m2", "second"),
  ];
  expect(discardedFrom(entries, "m1")).toBe(0);
  expect(discardedFrom(entries, "m2")).toBe(2);
  expect(discardedFrom(entries, "m3")).toBeUndefined();
  // An optimistic entry has no server id yet, so it can never be a target.
  expect(discardedFrom([{ id: "user-1", kind: "user", content: "x" }], "user-1")).toBeUndefined();
});

test("applyRewound rebuilds the transcript and appends the edited message", () => {
  const before = session({
    entries: [userEntry("m1", "first"), userEntry("m2", "second")],
    usage: [{ agentName: "coda", usage: { prompt_tokens: 900, completion_tokens: 0 } }],
    approvals: [{ thread_id: "t", agent_name: "coda", calls: [], suspended_at: "" }],
    drafts: { a: {} },
    allowDrafts: { a: {} },
    pendingCallInfo: { call_1: { id: "call_1", name: "shell", arguments: null } },
    editing: { target: "m2", text: "rewritten", images: [], submitting: true },
    firstUserMessage: "first",
  } as Partial<OpenedSession>);

  const after = applyRewound(before, {
    messages: [userMessage("m1", "first"), assistantMessage("a1", "ok", 100)],
    messageId: "m9",
    text: "rewritten",
    images: ["data:image/png;base64,AAAA"],
  });

  // The surviving history, then the edited message — which the event stream
  // will never deliver, so it has to come from here.
  expect(after.entries.map((entry) => entry.content)).toEqual(["first", "ok", "rewritten"]);
  const appended = after.entries[after.entries.length - 1];
  expect(appended.id).toBe("user:m9");
  expect(appended.messageId).toBe("m9");
  expect(appended.images).toEqual(["data:image/png;base64,AAAA"]);

  // Usage is recomputed from what survived, not carried over.
  expect(after.usage).toEqual([
    { agentName: "coda", usage: { prompt_tokens: 100, completion_tokens: 0, total_tokens: 100 } },
  ]);
  expect(after.running).toBe(true);
  expect(after.editing).toBeUndefined();
  expect(after.approvals).toEqual([]);
  expect(after.drafts).toEqual({});
  expect(after.allowDrafts).toEqual({});
  expect(after.pendingCallInfo).toEqual({});
  expect(after.firstUserMessage).toBe("first");
});

test("rewinding past the opening message makes the edited text the session title", () => {
  const after = applyRewound(session({ firstUserMessage: "first" }), {
    messages: [],
    messageId: "m9",
    text: "start over",
    images: [],
  });
  expect(after.entries.map((entry) => entry.content)).toEqual(["start over"]);
  expect(after.firstUserMessage).toBe("start over");
});

test("reconcileEditing keeps the edit when its target survived", () => {
  const editing = { target: "m2", text: "rewritten", images: [], submitting: true };
  const settled = reconcileEditing(editing, [userMessage("m1", "first"), userMessage("m2", "b")]);
  expect(settled).toEqual({ target: "m2", text: "rewritten", images: [], submitting: false });
});

test("reconcileEditing downgrades to a plain draft when its target is gone", () => {
  // The truncation committed but the turn that should have replaced it did not
  // start. The text stays; it just no longer names a message, so the next
  // submit is an ordinary task — and against this history that is exactly the
  // result the user asked for.
  const editing = { target: "m2", text: "rewritten", images: ["img"], submitting: true };
  const settled = reconcileEditing(editing, [userMessage("m1", "first")]);
  expect(settled).toEqual({ target: null, text: "rewritten", images: ["img"], submitting: false });
});

test("reconcileEditing leaves an orphan draft alone and always clears submitting", () => {
  const orphan = { target: null, text: "rewritten", images: [], submitting: true };
  expect(reconcileEditing(orphan, [])).toEqual({ ...orphan, submitting: false });
  expect(reconcileEditing(undefined, [userMessage("m1", "first")])).toBeUndefined();
});
