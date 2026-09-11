import { expect, test, vi } from "vitest";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";

import type {
  HistoryMessage,
  TaskNoticeMessage,
  TaskSummary,
  TaskResult,
} from "../src/lib/protocol.ts";
import { orderTasks, TaskResultContent } from "../src/components/background-tasks.tsx";
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
    access: { type: "read_write" },
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
    access: { type: "read_write" },
    turnRunning: false,
    compacting: false,
  });

  expect(after.entries).toEqual([
    expect.objectContaining({
      id: "task-notice:notice-1",
      kind: "task_notice",
      title: "Background task exited with code 0",
      detail: "cargo build --release",
      taskOutcomes: finishedNotice.outcomes,
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
      pid: "s1",
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
    access: { type: "read_write" },
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
    access: { type: "read_write" },
    turnRunning: false,
    compacting: false,
  });
  expect(resynced.backgroundTasks).toEqual([running]);
});

test("running tasks sort ahead of settled ones, newest first", () => {
  const task = (id: string, running: boolean, startedAt: string): TaskSummary => ({
    id,
    kind: { kind: "shell", command: id },
    task_status: running ? "Running" : { Exited: { code: 0, at: startedAt } },
    parent_task_id: null,
    subtree_active: running,
    result_available: false,
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
    access: { type: "read_write" },
    turnRunning: false,
    compacting: false,
  });

  expect(after.entries).toEqual([
    expect.objectContaining({
      kind: "task_notice",
      title: "2 background task updates",
      // A single command would be misleading when the notice covers two.
      detail: undefined,
      taskOutcomes: merged.TaskNotice.outcomes,
    }),
  ]);
});

test("an active shell stays immediately below its completed subagent parent", () => {
  const task = (id: string, startedAt: string): TaskSummary => ({
    id,
    kind: { kind: "shell", command: id },
    command: id,
    task_status: "Running",
    description: "",
    agent_name: "worker",
    status: "running",
    running: true,
    subtree_active: true,
    parent_task_id: null,
    result_available: false,
    started_at: startedAt,
  });
  const parent: TaskSummary = {
    ...task("parent", "2026-09-01T00:00:00Z"),
    kind: { kind: "subagent", agent_name: "worker" },
    running: false,
    result_available: true,
    task_status: { Completed: { at: "2026-09-01T00:00:01Z" } },
  };
  const child = { ...task("child", "2026-09-01T00:00:03Z"), parent_task_id: "parent" };
  const other = task("other", "2026-09-01T00:00:02Z");
  expect(orderTasks([child, other, parent]).map((task) => task.id)).toEqual([
    "other",
    "parent",
    "child",
  ]);
});

test("shell results render each stream as literal text with overwrite counts", () => {
  const result: TaskResult = {
    state: "available",
    status: { Exited: { code: 1, at: "2026-09-06T00:00:00Z" } },
    output: {
      kind: "shell",
      stdout: "**literal** <script>unsafe()</script>",
      stderr: "build failed",
      stdout_overwritten: 7,
      stderr_overwritten: 0,
    },
  };
  const html = renderToStaticMarkup(createElement(TaskResultContent, { result }));
  expect(html).toContain("Exited with code 1");
  expect(html).toContain("stdout");
  expect(html).toContain("stderr");
  expect(html).toContain("7 bytes of earlier output were overwritten.");
  expect(html).toContain("**literal** &lt;script&gt;unsafe()&lt;/script&gt;");
  expect(html).not.toContain("<script>");
  expect(html).toContain("build failed");
});

test("subagent answers still render as Markdown", () => {
  const result: TaskResult = {
    state: "available",
    status: { Completed: { at: "2026-09-06T00:00:00Z" } },
    output: { kind: "subagent", answer: "**done**" },
  };
  const html = renderToStaticMarkup(createElement(TaskResultContent, { result }));
  expect(html).toContain("<strong>done</strong>");
});

test("empty shell output, expiration, missing tasks and read errors are distinct", () => {
  const results: [TaskResult, string][] = [
    [
      {
        state: "available",
        status: { Exited: { code: 0, at: "2026-09-06T00:00:00Z" } },
        output: {
          kind: "shell",
          stdout: "",
          stderr: "",
          stdout_overwritten: 0,
          stderr_overwritten: 0,
        },
      },
      "(no output)",
    ],
    [{ state: "expired", status: { Killed: { at: "2026-09-06T00:00:00Z" } } }, "Result expired"],
    [{ state: "unknown" }, "Task not found"],
    [{ state: "error", message: "Could not read output" }, "Could not read output"],
  ];
  for (const [result, expected] of results) {
    const html = renderToStaticMarkup(createElement(TaskResultContent, { result }));
    expect(html).toContain(expected);
  }
});
