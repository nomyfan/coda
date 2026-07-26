import { expect, test } from "vitest";

import {
  adoptMessageId,
  applyRewound,
  applySnapshotToSession,
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
    editing: {
      target: "m2",
      text: "rewritten",
      images: [],
      submitting: true,
      precedingUserMessages: 1,
    },
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
  const editing = {
    target: "m2",
    text: "x",
    images: [],
    submitting: true,
    precedingUserMessages: 1,
  };
  const settled = reconcileEditing(editing, [userMessage("m1", "first"), userMessage("m2", "b")]);
  expect(settled).toEqual({ ...editing, submitting: false });
});

test("reconcileEditing downgrades to a plain draft when its target is gone", () => {
  // The truncation committed but the turn that should have replaced it did not
  // start. The text stays; it just no longer names a message, so the next
  // submit is an ordinary task — and against this history that is exactly the
  // result the user asked for.
  const editing = {
    target: "m2",
    text: "x",
    images: ["img"],
    submitting: true,
    precedingUserMessages: 1,
  };
  const settled = reconcileEditing(editing, [userMessage("m1", "first")]);
  expect(settled).toEqual({ ...editing, target: null, submitting: false });
});

test("reconcileEditing leaves an orphan draft alone and always clears submitting", () => {
  const orphan = {
    target: null,
    text: "rewritten",
    images: [],
    submitting: true,
    precedingUserMessages: 0,
  };
  expect(reconcileEditing(orphan, [])).toEqual({ ...orphan, submitting: false });
  expect(reconcileEditing(undefined, [userMessage("m1", "first")])).toBeUndefined();
});

test("reconcileEditing ends the edit when the replacement turn already started", () => {
  // Same visible fact as the test above — the target is gone — but one more user
  // message than preceded it, so the rewind went through in full and only the
  // reply to it was lost. Leaving the draft in the composer here would arm a
  // second copy of a message that has already been sent and answered.
  const editing = {
    target: "m2",
    text: "x",
    images: [],
    submitting: true,
    precedingUserMessages: 1,
  };
  const settled = reconcileEditing(editing, [
    userMessage("m1", "first"),
    userMessage("m9", "x"),
    assistantMessage("a9", "ok"),
  ]);
  expect(settled).toBeUndefined();
});

test("an empty snapshot clears a transcript the rewind took away", () => {
  // Rewinding to the opening message and then failing to start the replacement
  // turn leaves the session genuinely empty, and the server says so by pushing a
  // snapshot with no messages. Read as "no news" it would leave the discarded
  // conversation on screen, and the next task would append to a history that
  // exists only in this browser.
  const before = session({
    entries: [userEntry("m1", "first"), userEntry("m2", "second")],
    firstUserMessage: "first",
    usage: [{ agentName: "coda", usage: { prompt_tokens: 900, completion_tokens: 0 } }],
  } as Partial<OpenedSession>);

  const after = applySnapshotToSession(before, {
    messages: [],
    approvals: [],
    providerId: "prov:model",
    reasoningEffort: null,
    turnRunning: false,
  });

  expect(after.entries).toEqual([]);
  expect(after.firstUserMessage).toBeUndefined();
  expect(after.usage).toEqual([]);
  expect(after.running).toBe(false);
  expect(after.providerId).toBe("prov:model");
});

test("an orphan draft ends the same way once its own retry turns out to have started", () => {
  // The truncation committed, the replacement never started, and the draft was
  // downgraded to an orphan. Submitting it starts an ordinary task — and that
  // reply can be lost too, so the orphan needs the same count check the named
  // edit gets. Its baseline is still the history the truncation left behind.
  const orphan = {
    target: null,
    text: "x",
    images: [],
    submitting: true,
    precedingUserMessages: 1,
  };
  expect(reconcileEditing(orphan, [userMessage("m1", "first")])).toEqual({
    ...orphan,
    submitting: false,
  });
  expect(
    reconcileEditing(orphan, [userMessage("m1", "first"), userMessage("m9", "x")]),
  ).toBeUndefined();
});

test("a snapshot keeps a user message the server has not acknowledged yet", () => {
  // Opening a session does not block the composer, so a task can go out while
  // `open_session` is still in flight and come back to a snapshot taken before
  // it. Events never carry user messages, so dropping this entry would leave
  // the reply to it hanging off nothing.
  const pending: TranscriptEntry = { id: "user-1", kind: "user", content: "just sent" };
  const after = applySnapshotToSession(
    session({ entries: [userEntry("m1", "first"), pending] } as Partial<OpenedSession>),
    {
      messages: [userMessage("m1", "first")],
      approvals: [],
      providerId: "prov:model",
      reasoningEffort: null,
      turnRunning: false,
    },
  );
  expect(after.entries.map((entry) => entry.content)).toEqual(["first", "just sent"]);
  // That snapshot was taken before the task, so its `turn_running: false` is
  // stale. Believing it would reopen the composer under an unacknowledged turn.
  expect(after.running).toBe(true);

  // But once the server confirms it, the acknowledged copy is the only one that
  // can survive — see `adoptServerMessageId`.
  const acknowledged = applySnapshotToSession(
    session({ entries: [pending] } as Partial<OpenedSession>),
    {
      messages: [userMessage("m9", "just sent")],
      approvals: [],
      providerId: "prov:model",
      reasoningEffort: null,
      turnRunning: true,
    },
  );
  expect(acknowledged.entries.map((entry) => entry.id)).toEqual(["user:m9", "user-1"]);

  // ...and adopting the id the task finally answered with is what resolves it.
  expect(adoptMessageId(acknowledged.entries, "user-1", "m9").map((entry) => entry.id)).toEqual([
    "user:m9",
  ]);
});

test("adoptMessageId re-keys the optimistic entry when nothing else claims the id", () => {
  const entries = [userEntry("m1", "first"), { id: "user-1", kind: "user", content: "x" }];
  const adopted = adoptMessageId(entries as TranscriptEntry[], "user-1", "m9");
  expect(adopted.map((entry) => entry.id)).toEqual(["user:m1", "user:m9"]);
  expect(adopted[1].messageId).toBe("m9");
});

test("a pending first task keeps the title an empty snapshot would otherwise drop", () => {
  // Same race on a brand-new session: `open_session` returns nothing because
  // the first task has not been accepted yet. The optimistic title is the only
  // one in existence — the task's reply carries an id, not a title, so nothing
  // downstream would put it back.
  const after = applySnapshotToSession(
    session({
      entries: [{ id: "user-1", kind: "user", content: "just sent" }],
      firstUserMessage: "just sent",
    } as Partial<OpenedSession>),
    {
      messages: [],
      approvals: [],
      providerId: "prov:model",
      reasoningEffort: null,
      turnRunning: false,
    },
  );
  expect(after.firstUserMessage).toBe("just sent");
  expect(after.entries.map((entry) => entry.content)).toEqual(["just sent"]);
  expect(after.running).toBe(true);
});
