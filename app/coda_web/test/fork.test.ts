import { JSONRPCErrorException } from "json-rpc-2.0";
import { expect, test } from "vitest";

import {
  applySnapshotToSession,
  codaStore,
  forkSession,
  openSession,
  retryWhileNotReady,
  selectForking,
  updateForkDraft,
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

function userMessage(id: string, text: string): HistoryMessage {
  return {
    User: {
      message_id: id,
      parts: [{ type: "text", text }],
      created_at: "2026-07-31T00:00:00Z",
    },
  } as HistoryMessage;
}

test("a restored user message carries the id a fork cuts at", () => {
  const after = applySnapshotToSession(session(), {
    messages: [
      userMessage("m-user", "try this"),
      assistantMessage("m1", "mid-turn", [{ id: "call_1", name: "shell", arguments: "{}" }]),
      assistantMessage("m2", "the final answer"),
    ],
    approvals: [],
    providerId: "prov:model",
    reasoningEffort: null,
    turnRunning: false,
  });

  expect(after.entries.find((entry) => entry.kind === "user")).toMatchObject({
    messageId: "m-user",
    content: "try this",
  });
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

// A session has a fork entry on every eligible user message plus one in the
// sidebar, and the server mints a new id per request — so the guard has to be
// one flag for the source session, not one per button.
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

// The cut is the turn to branch *away* from, so its prompt is not in the copy —
// it goes to the composer instead, for the user to rewrite or resend.
test("the message a fork cuts at becomes the copy's composer draft", async () => {
  const server = "ws://seeded";
  codaStore.setState((state) => {
    state.servers[server] = {
      url: server,
      status: "connected",
      catalog: [],
      providers: [],
      sessions: {},
    };
    state.rpcMap[server] = {
      notify: () => true,
      request: (method: string) =>
        method === "fork_session"
          ? Promise.resolve({ session_id: "s2", name: null, workspaces: [] })
          : // `open_session` follows; leaving it pending keeps the test off the wire.
            new Promise(() => {}),
    } as never;
  });

  await forkSession(server, "ws", "s1", "m-cut", { text: "try it this way", images: ["img"] });

  expect(codaStore.getState().servers[server]?.sessions["ws/s2"]?.forkDraft).toEqual({
    text: "try it this way",
    images: ["img"],
  });

  updateForkDraft(server, "ws/s2", "edited after the fork", ["new-img"]);
  openSession(server, "ws", "somewhere-else");
  openSession(server, "ws", "s2");
  expect(codaStore.getState().servers[server]?.sessions["ws/s2"]?.forkDraft).toEqual({
    text: "edited after the fork",
    images: ["new-img"],
  });

  codaStore.setState((state) => {
    delete state.rpcMap[server];
    delete state.servers[server];
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
