import { expect, test } from "vitest";

import {
  finalAssistantIndexOf,
  interruptedTurnCut,
  transcriptRenderItems,
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

test("a mid-turn reply stays in the process list even when a tool run is aborted", () => {
  const entries = [
    reply({ isFinalResponse: false }),
    step({ id: "e2", kind: "tool_result" }),
    step({ id: "e3", kind: "system", status: "aborted", content: "Tool calls interrupted" }),
  ];

  expect(finalAssistantIndexOf(entries)).toBe(-1);
});

// That turn has no reply to fork from, so the only thing left naming it is the
// message that opened it — which is what the group has to carry down.
test("a turn is tagged with the message that opened it", () => {
  const items = transcriptRenderItems([
    { id: "u1", kind: "user", messageId: "m-user", content: "go" } as TranscriptEntry,
    reply({ isFinalResponse: false }),
    step({ id: "e2", kind: "tool_result" }),
    step({ id: "e3", kind: "system", status: "aborted", content: "Tool calls interrupted" }),
  ]);

  expect(items.map((item) => (item.type === "turn" ? item.openedBy : item.entry.id))).toEqual([
    "u1",
    "m-user",
  ]);
});

// Interrupting a tool run whose tools all finish cleanly first stores nothing
// that says so: normal tool results, no reply, no marker. Reading "is this turn
// over" off the entries would leave this one forkable live and not after a
// reload — and shimmering as if it were still working.
test("a turn stored with no sign of the interrupt is still forkable", () => {
  const entries = [
    reply({ isFinalResponse: false }),
    step({ id: "e2", kind: "tool_result", status: "auto" }),
  ];

  expect(interruptedTurnCut(entries, "m-user", true)).toBe("m-user");
  expect(interruptedTurnCut(entries, "m-user", false)).toBeUndefined();
});

test("a turn that ended on a reply forks from the reply, not from what opened it", () => {
  expect(interruptedTurnCut([step(), reply()], "m-user", true)).toBeUndefined();
});

test("a turn with no user message before it has nothing to name it", () => {
  const items = transcriptRenderItems([
    reply({ isFinalResponse: false }),
    step({ id: "e2", kind: "tool_result" }),
  ]);

  expect(items).toEqual([{ type: "turn", id: "turn:e1", entries: expect.any(Array) }]);
});
