import { JSONRPCErrorException } from "json-rpc-2.0";
import { expect, test } from "vitest";

import {
  applySnapshotToSession,
  codaStore,
  forkSession,
  retryWhileNotReady,
  selectForking,
  forkKey,
  type OpenedSession,
} from "../src/store/session.ts";
import { RpcCode, type HistoryMessage } from "../src/lib/protocol.ts";

function session(overrides: Partial<OpenedSession> = {}): OpenedSession {
  return {
    key: "ws/s1",
    workspaceId: "ws",
    sessionId: "s1",
    entries: [],
    activity: [],
    approvals: [],
    pendingCallInfo: {},
    drafts: {},
    allowDrafts: {},
    running: false,
    evicted: false,
    usage: [],
    ...overrides,
  } as OpenedSession;
}

function assistantMessage(id: string, content: string, toolCalls: unknown[] = []): HistoryMessage {
  return {
    Assistant: {
      message_id: id,
      content,
      tool_calls: toolCalls,
      usage: null,
      reasoning_content: null,
      reasoning_ended_at: null,
      aborted: false,
      started_at: "2026-07-31T00:00:00Z",
      ended_at: "2026-07-31T00:00:01Z",
    },
  } as HistoryMessage;
}

test("a restored final reply carries the id a fork cuts at", () => {
  const after = applySnapshotToSession(session(), {
    messages: [
      assistantMessage("m1", "mid-turn", [{ id: "call_1", name: "shell", arguments: "{}" }]),
      assistantMessage("m2", "the final answer"),
    ],
    approvals: [],
    providerId: "prov:model",
    reasoningEffort: null,
    turnRunning: false,
  });

  const assistants = after.entries.filter((entry) => entry.kind === "assistant");
  expect(assistants.map((entry) => [entry.messageId, entry.isFinalResponse])).toEqual([
    ["m1", false],
    ["m2", true],
  ]);
});

test("a fork retries once when the database has not caught up", async () => {
  let attempts = 0;
  const forked = await retryWhileNotReady(async () => {
    attempts += 1;
    if (attempts === 1) {
      throw new JSONRPCErrorException("not stored yet", RpcCode.FORK_NOT_READY);
    }
    return "s2";
  });

  expect(forked).toBe("s2");
  expect(attempts).toBe(2);
});

// A session has a fork entry on every reply plus one in the sidebar, and the
// server mints a new id per request — so the guard has to be one flag for the
// source session, not one per button.
test("a second fork of the same session while one is in flight is dropped", async () => {
  const server = "ws://server";
  const key = forkKey(server, "ws", "s1");
  let sent = 0;
  let fail: (err: unknown) => void = () => {};
  const inFlight = new Promise((_resolve, reject) => (fail = reject));
  codaStore.setState((state) => {
    state.rpcMap[server] = {
      request: () => {
        sent += 1;
        return inFlight;
      },
    } as never;
  });

  const first = forkSession(server, "ws", "s1");
  expect(selectForking(key)(codaStore.getState())).toBe(true);

  await expect(forkSession(server, "ws", "s1", "m2")).resolves.toBeUndefined();
  expect(sent).toBe(1);

  fail(new Error("boom"));
  await expect(first).rejects.toThrow();
  expect(selectForking(key)(codaStore.getState())).toBe(false);

  codaStore.setState((state) => {
    delete state.rpcMap[server];
  });
});

test("a fork does not retry any other failure", async () => {
  // The server has no request de-duplication, so retrying a failure that may
  // have written something would mint a second copy.
  let attempts = 0;
  await expect(
    retryWhileNotReady(async () => {
      attempts += 1;
      throw new JSONRPCErrorException("not idle", RpcCode.SESSION_NOT_IDLE);
    }),
  ).rejects.toThrow("not idle");

  expect(attempts).toBe(1);
});
