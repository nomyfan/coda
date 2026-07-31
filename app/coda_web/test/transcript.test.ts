import { expect, test } from "vitest";

import { finalAssistantIndexOf } from "../src/components/transcript.tsx";
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
