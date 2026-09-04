import { expect, test, vi } from "vitest";

import type { HistoryMessage, TaskNoticeMessage, TaskSummary } from "../src/lib/protocol.ts";
import { orderTasks } from "../src/components/background-tasks.tsx";
import { transcriptRenderItems } from "../src/components/transcript.tsx";
import {
  appendTaskNotice,
  applyEvent,
  applySnapshotToSession,
  codaStore,
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

const finishedNotice: TaskNoticeMessage = {
  message_id: "notice-1",
  outcomes: [
    {
      type: "finished",
      task_id: "bg_1",
      command: "cargo build --release",
      status: "exited with code 0",
    },
  ],
  content: "Background task bg_1 finished: exited with code 0.\nCommand: cargo build --release",
  created_at: "2026-08-29T00:00:00Z",
};

const finished: HistoryMessage = { TaskNotice: finishedNotice };

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

test("a task notice flushes the previous turn before it is appended", () => {
  const server = "ws://task-notice-order";
  vi.stubGlobal(
    "requestAnimationFrame",
    vi.fn(() => 1),
  );
  vi.stubGlobal("cancelAnimationFrame", vi.fn());
  codaStore.setState((state) => {
    state.servers[server] = {
      url: server,
      status: "connected",
      catalog: [],
      providers: [],
      sessions: { "ws/s1": session({ running: true }) },
    };
  });

  try {
    applyEvent(server, "ws", "s1", {
      type: "llm_end",
      agent_name: "coda",
      thread_id: "s1",
      message: {
        message_id: "answer-1",
        content: "done",
        tool_calls: [],
        usage: null,
        reasoning_content: null,
        reasoning_ended_at: null,
        aborted: false,
        started_at: "2026-08-29T00:00:00Z",
        ended_at: "2026-08-29T00:00:01Z",
      },
    });
    expect(codaStore.getState().servers[server].sessions["ws/s1"].entries).toEqual([]);

    appendTaskNotice(codaStore, server, "ws", "s1", finishedNotice);

    expect(
      codaStore.getState().servers[server].sessions["ws/s1"].entries.map((entry) => entry.kind),
    ).toEqual(["assistant", "task_notice"]);
  } finally {
    codaStore.setState((state) => {
      delete state.servers[server];
    });
    vi.unstubAllGlobals();
  }
});

test("the task list arrives with the snapshot and is replaced by its own push", () => {
  const running: TaskSummary = {
    id: "bg_1",
    command: "cargo build",
    description: "build",
    agent_name: "coda",
    status: "running",
    running: true,
    started_at: "2026-08-29T00:00:00Z",
  };

  const attached = applySnapshotToSession(session(), {
    messages: [],
    approvals: [],
    providerId: "provider:model",
    reasoningEffort: null,
    permissionMode: "accept_edits",
    turnRunning: false,
    compacting: false,
    backgroundTasks: [running],
  });
  expect(attached.backgroundTasks).toEqual([running]);

  // A snapshot that carries no list (an older server, or a resync composed
  // before the registry existed) must not wipe what the pushes have said.
  const resynced = applySnapshotToSession(attached, {
    messages: [],
    approvals: [],
    providerId: "provider:model",
    reasoningEffort: null,
    permissionMode: "accept_edits",
    turnRunning: false,
    compacting: false,
  });
  expect(resynced.backgroundTasks).toEqual([running]);
});

test("running tasks sort ahead of settled ones, newest first", () => {
  const task = (id: string, running: boolean, startedAt: string): TaskSummary => ({
    id,
    command: id,
    description: "",
    agent_name: "coda",
    status: running ? "running" : "exited with code 0",
    running,
    started_at: startedAt,
  });
  const ordered = orderTasks([
    task("old-done", false, "2026-08-29T00:00:00Z"),
    task("new-done", false, "2026-08-29T00:00:02Z"),
    task("running", true, "2026-08-29T00:00:01Z"),
  ]);
  expect(ordered.map((t) => t.id)).toEqual(["running", "new-done", "old-done"]);
});

test("one notice covering several tasks is titled by count, not by the first one", () => {
  const merged: HistoryMessage = {
    TaskNotice: {
      message_id: "notice-2",
      outcomes: [
        { type: "finished", task_id: "bg_1", command: "cargo build", status: "exited with code 0" },
        { type: "finished", task_id: "bg_2", command: "cargo test", status: "killed" },
      ],
      content: "…two of them…",
      created_at: "2026-08-29T00:00:00Z",
    },
  };
  const after = applySnapshotToSession(session(), {
    messages: [merged],
    approvals: [],
    providerId: "provider:model",
    reasoningEffort: null,
    permissionMode: "accept_edits",
    turnRunning: false,
    compacting: false,
  });

  expect(after.entries).toEqual([
    expect.objectContaining({
      kind: "task_notice",
      title: "2 background task updates",
      // A single command would be misleading when the notice covers two.
      detail: undefined,
    }),
  ]);
});
