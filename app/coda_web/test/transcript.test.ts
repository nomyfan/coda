import { expect, test } from "vitest";

import {
  finalAssistantIndexOf,
  groupProcessItems,
  processStepCount,
  processWorkMs,
} from "../src/components/transcript.tsx";
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

test("steps count tool calls, not reasoning or intermediate prose", () => {
  const entries = [
    step({ id: "r1", kind: "reasoning" }),
    step({ id: "t1", kind: "tool_result" }),
    reply({ id: "a1", isFinalResponse: false }),
  ];

  expect(processStepCount(groupProcessItems(entries))).toBe(1);
});

test("a call stranded mid-flight by an abort still counts as a step", () => {
  const entries = [step({ id: "t1", kind: "tool_call", status: "running" })];

  expect(processStepCount(groupProcessItems(entries))).toBe(1);
});

test("a rejected call never ran, so it doesn't count as a step", () => {
  // A denied approval gets no `tool_start` — just a result-only entry (see
  // `finishToolEntry`'s index<0 branch) carrying status "rejected".
  const entries = [step({ id: "t1", kind: "tool_result", status: "rejected" })];

  expect(processStepCount(groupProcessItems(entries))).toBe(0);
});

test("a turn where every requested call was denied has zero steps, not one", () => {
  const entries = [
    step({ id: "t1", kind: "tool_result", status: "rejected" }),
    step({ id: "t2", kind: "tool_result", status: "rejected" }),
  ];

  expect(processStepCount(groupProcessItems(entries))).toBe(0);
});

test("a sub-agent invocation counts itself plus its nested tool calls, not one flat group", () => {
  const entries = [
    step({ id: "call1", kind: "tool_result", title: "agent__researcher", agentName: "coda" }),
    step({ id: "r1", kind: "reasoning", agentName: "researcher" }),
    step({ id: "t1", kind: "tool_result", agentName: "researcher", title: "read_file" }),
    step({ id: "t2", kind: "tool_result", agentName: "researcher", title: "grep" }),
  ];

  const items = groupProcessItems(entries);
  expect(items).toHaveLength(1);
  // 1 for the invocation itself + 2 nested tool calls; the reasoning step doesn't count.
  expect(processStepCount(items)).toBe(3);
});

test("a sub-agent calling a sub-agent still flattens into one group's step count", () => {
  const entries = [
    step({ id: "call1", kind: "tool_result", title: "agent__researcher", agentName: "coda" }),
    step({
      id: "call2",
      kind: "tool_result",
      title: "agent__fact_checker",
      agentName: "researcher",
    }),
    step({ id: "f1", kind: "tool_result", agentName: "fact_checker", title: "search" }),
  ];

  const items = groupProcessItems(entries);
  expect(items).toHaveLength(1);
  // 1 for the outer invocation + 1 for the nested invocation + 1 for its own tool call.
  expect(processStepCount(items)).toBe(3);
});
