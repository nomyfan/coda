import { beforeEach, expect, test } from "vitest";

import {
  forgetSessionMode,
  initialSessionMode,
  rememberSessionMode,
} from "../src/store/permission-modes.ts";
import { applySnapshotToSession, type OpenedSession } from "../src/store/session.ts";

const values = new Map<string, string>();
globalThis.window = {
  localStorage: {
    getItem(key: string) {
      return values.get(key) ?? null;
    },
    setItem(key: string, value: string) {
      values.set(key, value);
    },
  },
} as unknown as Window & typeof globalThis;

beforeEach(() => values.clear());

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
    permissionMode: "accept_edits",
    usage: [],
    ...overrides,
  } as OpenedSession;
}

test("remembers a mode per session, not per workspace", () => {
  rememberSessionMode("ws://one", "alpha", "s1", "yolo");

  expect(initialSessionMode("ws://one", "alpha", "s1")).toBe("yolo");
  // A sibling session in the same workspace is untouched: switching inside one
  // conversation must not change what the next one starts on.
  expect(initialSessionMode("ws://one", "alpha", "s2")).toBe("accept_edits");
  // Neither is the same session id on another server.
  expect(initialSessionMode("ws://two", "alpha", "s1")).toBe("accept_edits");
});

test("an unremembered session opens on the default", () => {
  expect(initialSessionMode("ws://one", "alpha", "never-seen")).toBe("accept_edits");
});

test("malformed storage is ignored rather than trusted", () => {
  values.set("coda.permissionModes", JSON.stringify({ "ws://one": { "alpha:s1": "yolo" } }));
  expect(initialSessionMode("ws://one", "alpha", "s1")).toBe("accept_edits");
});

test("forgetting a session drops its memory", () => {
  rememberSessionMode("ws://one", "alpha", "s1", "explore");
  forgetSessionMode("ws://one", "alpha", "s1");

  expect(initialSessionMode("ws://one", "alpha", "s1")).toBe("accept_edits");
});

test("the memory is capped, dropping the least recently written first", () => {
  for (let index = 0; index < 205; index += 1) {
    rememberSessionMode("ws://one", "alpha", `s${index}`, "explore");
  }

  // The five oldest are gone; the newest survive.
  expect(initialSessionMode("ws://one", "alpha", "s0")).toBe("accept_edits");
  expect(initialSessionMode("ws://one", "alpha", "s4")).toBe("accept_edits");
  expect(initialSessionMode("ws://one", "alpha", "s5")).toBe("explore");
  expect(initialSessionMode("ws://one", "alpha", "s204")).toBe("explore");
});

// The reconnect case: a session that kept running while this client was away
// answers with the posture it is actually executing under, and that wins over
// whatever the browser had remembered for it.
test("a snapshot's mode replaces the local one", () => {
  const applied = applySnapshotToSession(session({ permissionMode: "explore" }), {
    messages: [],
    approvals: [],
    providerId: "provider:model",
    reasoningEffort: null,
    permissionMode: "yolo",
    turnRunning: true,
  });

  expect(applied.permissionMode).toBe("yolo");
});

// A fork is handed its parent's mode a moment before it is opened, and that
// handoff must not depend on storage: `rememberSessionMode` swallows a blocked
// or full localStorage, and reading the default back would open the fork with
// permissions the parent never had.
test("an unrecorded session falls back to what the caller already knows", () => {
  expect(initialSessionMode("ws://one", "alpha", "forked", "explore")).toBe("explore");
  // A record still wins over the caller's guess.
  rememberSessionMode("ws://one", "alpha", "forked", "yolo");
  expect(initialSessionMode("ws://one", "alpha", "forked", "explore")).toBe("yolo");
});
