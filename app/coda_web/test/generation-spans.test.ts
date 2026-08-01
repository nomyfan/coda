import { expect, test } from "vitest";

import { processWorkMs } from "../src/components/transcript.tsx";
import {
  applyRewound,
  applySnapshotToSession,
  reduceEvent,
  type OpenedSession,
} from "../src/store/session.ts";
import type { AssistantMessage, HistoryMessage, ToolMessage } from "../src/lib/protocol.ts";

const at = (seconds: number) => new Date(Date.UTC(2026, 0, 1, 0, 0, seconds)).toISOString();

/** A generation that asked for one tool call and produced nothing else — no
 * prose, no reasoning, so nothing but the call it asked for records its time. */
function toolCallOnly(id: string, callId: string, from: number, to: number): AssistantMessage {
  return {
    message_id: id,
    content: "",
    tool_calls: [{ id: callId, name: "shell", arguments: '{"command":"ls"}' }],
    usage: null,
    reasoning_content: null,
    reasoning_ended_at: null,
    aborted: false,
    started_at: at(from),
    ended_at: at(to),
  } as AssistantMessage;
}

function toolReply(id: string, callId: string, from: number, to: number): ToolMessage {
  return {
    message_id: id,
    id: callId,
    name: "shell",
    output: { Ok: "done" },
    outcome: "Auto",
    started_at: at(from),
    ended_at: at(to),
  } as ToolMessage;
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
    generationSpans: {},
    drafts: {},
    allowDrafts: {},
    running: false,
    evicted: false,
    usage: [],
    ...overrides,
  } as OpenedSession;
}

function snapshot(messages: HistoryMessage[], turnRunning = false) {
  return {
    messages,
    approvals: [],
    providerId: "p",
    reasoningEffort: null,
    turnRunning,
  };
}

test("a tool-call-only round still counts as work", () => {
  // 2s generating the call, 1s running it.
  const after = applySnapshotToSession(
    session(),
    snapshot([
      { Assistant: toolCallOnly("a1", "call_1", 0, 2) },
      { Tool: toolReply("t1", "call_1", 2, 3) },
    ]),
  );

  expect(processWorkMs(after.entries)).toBe(3_000);
});

// With thinking off, every round of a tool-using turn is generation + tool and
// nothing else, so the generations are most of the turn.
test("a turn of nothing but tool-call rounds accounts for all of itself", () => {
  const messages: HistoryMessage[] = [];
  for (let round = 0; round < 5; round += 1) {
    const from = round * 2;
    messages.push({ Assistant: toolCallOnly(`a${round}`, `call_${round}`, from, from + 1.5) });
    messages.push({ Tool: toolReply(`t${round}`, `call_${round}`, from + 1.5, from + 2) });
  }

  const after = applySnapshotToSession(session(), snapshot(messages));

  expect(processWorkMs(after.entries)).toBe(10_000);
});

test("calls from one generation share its span instead of repeating it", () => {
  // Two tools from the same 2s generation, running 1s each back to back:
  // 2s + 1s + 1s, not 2s counted twice.
  const generation = {
    ...toolCallOnly("a1", "call_1", 0, 2),
    tool_calls: [
      { id: "call_1", name: "shell", arguments: null },
      { id: "call_2", name: "shell", arguments: null },
    ],
  } as AssistantMessage;
  const after = applySnapshotToSession(
    session(),
    snapshot([
      { Assistant: generation },
      { Tool: toolReply("t1", "call_1", 2, 3) },
      { Tool: toolReply("t2", "call_2", 3, 4) },
    ]),
  );

  expect(processWorkMs(after.entries)).toBe(4_000);
});

// A turn interrupted by a dropped connection: the snapshot carries the
// generation, but the call it asked for only reports back over the live stream.
test("a tool_end after a reconnect still finds its generation span", () => {
  const reattached = applySnapshotToSession(
    session(),
    snapshot([{ Assistant: toolCallOnly("a1", "call_1", 0, 2) }], true),
  );
  expect(reattached.generationSpans.call_1).toEqual({ startedAt: at(0), endedAt: at(2) });

  const after = reduceEvent(reattached, {
    type: "tool_end",
    agent_name: "coda",
    thread_id: "t",
    message: toolReply("t1", "call_1", 2, 3),
  });

  expect(processWorkMs(after.entries)).toBe(3_000);
  // Consumed, so a later snapshot is the only thing that can put it back.
  expect(after.generationSpans.call_1).toBeUndefined();
});

test("a live tool_end uses the span its llm_end recorded", () => {
  const generated = reduceEvent(session(), {
    type: "llm_end",
    agent_name: "coda",
    thread_id: "t",
    message: toolCallOnly("a1", "call_1", 0, 2),
  });
  const started = reduceEvent(generated, {
    type: "tool_start",
    agent_name: "coda",
    thread_id: "t",
    call: { id: "call_1", name: "shell", arguments: null },
  });
  const after = reduceEvent(started, {
    type: "tool_end",
    agent_name: "coda",
    thread_id: "t",
    message: toolReply("t1", "call_1", 2, 3),
  });

  expect(processWorkMs(after.entries)).toBe(3_000);
});

test("a rewind drops spans for the calls it discarded", () => {
  const before = session({
    generationSpans: { call_gone: { startedAt: at(0), endedAt: at(2) } },
  });

  const after = applyRewound(before, {
    messages: [{ Assistant: toolCallOnly("a1", "call_kept", 0, 2) }],
    messageId: "m2",
    text: "again",
    images: [],
  });

  expect(after.generationSpans.call_gone).toBeUndefined();
  expect(after.generationSpans.call_kept).toEqual({ startedAt: at(0), endedAt: at(2) });
});
