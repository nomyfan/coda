import { beforeEach, expect, test } from "vitest";

import {
  forgetSessionPreset,
  initialSessionPreset,
  rememberSessionPreset,
} from "../src/store/permission-presets.ts";
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
    permissionPreset: "accept_edits",
    usage: [],
    ...overrides,
  } as OpenedSession;
}

test("remembers a preset per session, not per workspace", () => {
  rememberSessionPreset("ws://one", "alpha", "s1", "yolo");

  expect(initialSessionPreset("ws://one", "alpha", "s1")).toBe("yolo");
  // A sibling session in the same workspace is untouched: switching inside one
  // conversation must not change what the next one starts on.
  expect(initialSessionPreset("ws://one", "alpha", "s2")).toBe("accept_edits");
  // Neither is the same session id on another server.
  expect(initialSessionPreset("ws://two", "alpha", "s1")).toBe("accept_edits");
});

test("an unremembered session opens on the default", () => {
  expect(initialSessionPreset("ws://one", "alpha", "never-seen")).toBe("accept_edits");
});

test("malformed storage is ignored rather than trusted", () => {
  values.set("coda.permissionPresets", JSON.stringify({ "ws://one": { "alpha:s1": "yolo" } }));
  expect(initialSessionPreset("ws://one", "alpha", "s1")).toBe("accept_edits");
});

test("forgetting a session drops its memory", () => {
  rememberSessionPreset("ws://one", "alpha", "s1", "explore");
  forgetSessionPreset("ws://one", "alpha", "s1");

  expect(initialSessionPreset("ws://one", "alpha", "s1")).toBe("accept_edits");
});

test("the memory is capped, dropping the least recently written first", () => {
  for (let index = 0; index < 205; index += 1) {
    rememberSessionPreset("ws://one", "alpha", `s${index}`, "explore");
  }

  // The five oldest are gone; the newest survive.
  expect(initialSessionPreset("ws://one", "alpha", "s0")).toBe("accept_edits");
  expect(initialSessionPreset("ws://one", "alpha", "s4")).toBe("accept_edits");
  expect(initialSessionPreset("ws://one", "alpha", "s5")).toBe("explore");
  expect(initialSessionPreset("ws://one", "alpha", "s204")).toBe("explore");
});

// The reconnect case: a session that kept running while this client was away
// answers with the posture it is actually executing under, and that wins over
// whatever the browser had remembered for it.
test("a snapshot's preset replaces the local one", () => {
  const applied = applySnapshotToSession(session({ permissionPreset: "explore" }), {
    messages: [],
    approvals: [],
    providerId: "provider:model",
    reasoningEffort: null,
    permissionPreset: "yolo",
    turnRunning: true,
  });

  expect(applied.permissionPreset).toBe("yolo");
});
