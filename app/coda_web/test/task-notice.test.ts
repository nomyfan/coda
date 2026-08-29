import { expect, test } from "vitest";

import type { HistoryMessage } from "../src/lib/protocol.ts";
import { transcriptRenderItems } from "../src/components/transcript.tsx";
import {
  applySnapshotToSession,
  type OpenedSession,
  type TranscriptEntry,
} from "../src/store/session.ts";

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
    compacting: false,
    evicted: false,
    permissionMode: "accept_edits",
    ...overrides,
  } as OpenedSession;
}

const finished: HistoryMessage = {
  TaskNotice: {
    message_id: "notice-1",
    outcome: {
      type: "finished",
      task_id: "bg_1",
      command: "cargo build --release",
      status: "exited with code 0",
    },
    content: "Background task bg_1 finished: exited with code 0.\nCommand: cargo build --release",
    created_at: "2026-08-29T00:00:00Z",
  },
};

test("a finished background task renders as a notice, not a user bubble", () => {
  const after = applySnapshotToSession(session(), {
    messages: [finished],
    approvals: [],
    providerId: "provider:model",
    reasoningEffort: null,
    permissionMode: "accept_edits",
    turnRunning: false,
    compacting: false,
  });

  expect(after.entries).toEqual([
    expect.objectContaining({
      id: "task-notice:notice-1",
      kind: "task_notice",
      title: "Background task exited with code 0",
      detail: "cargo build --release",
    }),
  ]);
});

test("a notice opens a turn, so it is never folded into the previous one", () => {
  const entries: TranscriptEntry[] = [
    { id: "u1", kind: "user", content: "build it" },
    { id: "a1", kind: "assistant", content: "starting" },
    {
      id: "task-notice:notice-1",
      kind: "task_notice",
      title: "Background task exited with code 0",
      content: "…",
    },
    { id: "a2", kind: "assistant", content: "the build passed" },
  ];

  const items = transcriptRenderItems(entries);
  const notice = items.find((item) => item.type === "entry" && item.entry.kind === "task_notice");
  expect(notice).toBeDefined();
});
