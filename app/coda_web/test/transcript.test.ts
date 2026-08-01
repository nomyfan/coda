import { expect, test } from "vitest";

import { finalAssistantIndexOf, processWorkMs } from "../src/components/transcript.tsx";
import type { TranscriptEntry } from "../src/store/session.ts";

function reply(overrides: Partial<TranscriptEntry> = {}): TranscriptEntry {
  return {
    id: "e1",
    kind: "assistant",
    agentName: "coda",
    content: "the answer",
    messageId: "m1",
    isFinalResponse: true,
    ...overrides,
  } as TranscriptEntry;
}

function step(overrides: Partial<TranscriptEntry> = {}): TranscriptEntry {
  return {
    id: "e0",
    kind: "tool_call",
    agentName: "coda",
    content: "",
    ...overrides,
  } as TranscriptEntry;
}

// An aborted generation emits three things in a row: the partial reply, the
// abort marker, and the error the interrupted run returns.
test("an abort does not hide the partial reply it follows", () => {
  const entries = [
    step(),
    reply(),
    step({ id: "e2", kind: "system", status: "aborted", content: "Generation interrupted" }),
    step({ id: "e3", kind: "error", content: "Aborted by user" }),
  ];

  expect(finalAssistantIndexOf(entries)).toBe(1);
});

test("a turn that goes on past its reply has no answer to lift out", () => {
  const entries = [reply(), step({ id: "e2" })];

  expect(finalAssistantIndexOf(entries)).toBe(-1);
});

test("an aborted turn with nothing streamed has no reply at all", () => {
  const entries = [
    step(),
    step({ id: "e2", kind: "system", status: "aborted", content: "Generation interrupted" }),
  ];

  expect(finalAssistantIndexOf(entries)).toBe(-1);
});

test("a turn interrupted during its tool calls ends on a tool result", () => {
  const entries = [
    reply({ isFinalResponse: false }),
    step({ id: "e2", kind: "tool_result" }),
    step({ id: "e3", kind: "system", status: "aborted", content: "Tool calls interrupted" }),
  ];

  expect(finalAssistantIndexOf(entries)).toBe(-1);
});

/** A step running from `from` to `to` seconds past a fixed epoch. */
function timed(id: string, from: number, to: number): TranscriptEntry {
  const at = (seconds: number) => new Date(Date.UTC(2026, 0, 1, 0, 0, seconds)).toISOString();
  return step({ id, kind: "tool_result", startedAt: at(from), endedAt: at(to) });
}

test("overlapping tool calls count the shared stretch once", () => {
  // Three tools fired together, the longest running 10s: 10s of work, not 24s.
  const entries = [timed("t1", 0, 10), timed("t2", 1, 6), timed("t3", 2, 11)];

  expect(processWorkMs(entries)).toBe(11_000);
});

test("idle time between steps is not work", () => {
  // 2s of tool, five minutes waiting for an approval, 3s of tool.
  const entries = [timed("t1", 0, 2), timed("t2", 302, 305)];

  expect(processWorkMs(entries)).toBe(5_000);
});

test("a step that touches the previous one leaves no gap", () => {
  const entries = [timed("t1", 0, 4), timed("t2", 4, 9)];

  expect(processWorkMs(entries)).toBe(9_000);
});

test("a step nested inside a longer one adds nothing", () => {
  // What a sub-agent looks like: its own steps sit inside the `agent__*` call.
  const entries = [timed("agent__probe", 0, 30), timed("inner", 5, 12)];

  expect(processWorkMs(entries)).toBe(30_000);
});

test("steps without both timestamps sit out", () => {
  const entries = [
    step({ id: "running", kind: "tool_call", status: "running" }),
    timed("t1", 0, 3),
    step({ id: "bad", kind: "tool_result", startedAt: "not a date", endedAt: "also not" }),
  ];

  expect(processWorkMs(entries)).toBe(3_000);
});

test("a turn with nothing timed has no duration to show", () => {
  expect(processWorkMs([step({ id: "running", kind: "tool_call", status: "running" })])).toBe(
    undefined,
  );
});
