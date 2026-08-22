import { expect, test } from "vitest";

import { showsToolEntryText } from "../src/components/transcript.tsx";
import { type AssistantMessage, type ToolMessage, toolDisplayName } from "../src/lib/protocol.ts";
import { applySnapshotToSession, reduceEvent, type OpenedSession } from "../src/store/session.ts";

const code = 'const raw = await tools.read_file({ file_path: "README.md" });\nreturn raw;';

function session(): OpenedSession {
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
  } as OpenedSession;
}

function result(): ToolMessage {
  return {
    message_id: "tool-message",
    id: "call-1",
    name: "run_javascript",
    output: { Ok: '{"ok":true}' },
    outcome: "Auto",
    started_at: "2026-08-22T10:00:01Z",
    ended_at: "2026-08-22T10:00:02Z",
  };
}

test("a live run_javascript result keeps the generated script", () => {
  const started = reduceEvent(session(), {
    type: "tool_start",
    agent_name: "coda",
    thread_id: "thread-1",
    call: {
      id: "call-1",
      name: "run_javascript",
      arguments: JSON.stringify({ code }),
    },
  });

  const finished = reduceEvent(started, {
    type: "tool_end",
    agent_name: "coda",
    thread_id: "thread-1",
    message: result(),
  });

  expect(toolDisplayName("run_javascript")).toBe("Run code");
  expect(started.activity[0]?.detail).toBe("Run code");
  expect(finished.entries).toHaveLength(1);
  expect(finished.entries[0]).toMatchObject({
    kind: "tool_result",
    title: "run_javascript",
    script: code,
    content: '{"ok":true}',
  });
});

test("a snapshot restores the generated script beside its result", () => {
  const assistant: AssistantMessage = {
    message_id: "assistant-message",
    content: "",
    tool_calls: [
      {
        id: "call-1",
        name: "run_javascript",
        arguments: JSON.stringify({ code }),
      },
    ],
    started_at: "2026-08-22T10:00:00Z",
    ended_at: "2026-08-22T10:00:01Z",
  };
  const restored = applySnapshotToSession(session(), {
    messages: [{ Assistant: assistant }, { Tool: result() }],
    approvals: [],
    providerId: "provider:model",
    reasoningEffort: null,
    turnRunning: false,
  });

  expect(restored.entries).toHaveLength(1);
  expect(restored.entries[0]).toMatchObject({
    kind: "tool_result",
    title: "run_javascript",
    script: code,
    content: '{"ok":true}',
  });
});

test("PTC text is hidden while running and retained beside file diffs when complete", () => {
  expect(
    showsToolEntryText({
      id: "running",
      kind: "tool_call",
      content: JSON.stringify({ code }),
      script: code,
    }),
  ).toBe(false);
  expect(
    showsToolEntryText({
      id: "complete",
      kind: "tool_result",
      content: '{"ok":true}',
      script: code,
      artifacts: [
        {
          type: "file_diff",
          path: "README.md",
          operation: "modify",
          patch: "@@ -1 +1 @@",
        },
      ],
    }),
  ).toBe(true);
});
