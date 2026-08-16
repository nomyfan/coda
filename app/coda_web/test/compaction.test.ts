import { expect, test } from "vitest";

import { parseCompactCommand } from "../src/lib/compact-command.ts";
import type { HistoryMessage } from "../src/lib/protocol.ts";
import {
  applySnapshotToSession,
  codaStore,
  compactActiveSession,
  type OpenedSession,
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
    usage: [],
    ...overrides,
  };
}

test("only a whole composer input is a compact command", () => {
  expect(parseCompactCommand("/compact")).toBe("");
  expect(parseCompactCommand("/compact 只保留架构决策")).toBe("只保留架构决策");
  expect(parseCompactCommand("  /compact\tkeep decisions  ")).toBe("keep decisions");
  expect(parseCompactCommand("please /compact this")).toBeNull();
  expect(parseCompactCommand("/compacted")).toBeNull();
  expect(parseCompactCommand("x/compact")).toBeNull();
});

test("a compaction custom message becomes a distinct transcript separator", () => {
  const message: HistoryMessage = {
    Custom: {
      message_id: "summary-1",
      kind: "compaction",
      role: "User",
      content: "Keep the storage invariants and continue with the web client.",
      created_at: "2026-08-16T00:00:00Z",
    },
  };
  const after = applySnapshotToSession(session(), {
    messages: [message],
    approvals: [],
    providerId: "provider:model",
    reasoningEffort: null,
    permissionMode: "accept_edits",
    turnRunning: false,
    compacting: false,
  });

  expect(after.entries).toEqual([
    expect.objectContaining({
      id: "compaction:summary-1",
      kind: "compaction",
      title: "Context compacted",
      content: "Keep the storage invariants and continue with the web client.",
    }),
  ]);
});

test("the authoritative snapshot exposes compaction as busy without inventing a turn", () => {
  const after = applySnapshotToSession(session(), {
    messages: [],
    approvals: [],
    providerId: "provider:model",
    reasoningEffort: null,
    permissionMode: "accept_edits",
    turnRunning: false,
    compacting: true,
  });

  expect(after.running).toBe(false);
  expect(after.compacting).toBe(true);
});

test("compactActiveSession calls compact instead of task", async () => {
  const server = "ws://compaction-test";
  let request: { method: string; params: unknown } | undefined;
  codaStore.setState((state) => {
    state.activeServer = server;
    state.activeKey = "ws/s1";
    state.servers[server] = {
      url: server,
      status: "connected",
      catalog: [],
      providers: [],
      sessions: { "ws/s1": session() },
    };
    state.rpcMap[server] = {
      request: (method: string, params: unknown) => {
        request = { method, params };
        return Promise.resolve({ outcome: "applied" });
      },
    } as never;
  });

  await compactActiveSession("keep decisions");

  expect(request).toEqual({
    method: "compact",
    params: { workspace_id: "ws", session_id: "s1", instructions: "keep decisions" },
  });
  codaStore.setState((state) => {
    delete state.servers[server];
    delete state.rpcMap[server];
    state.activeServer = undefined;
    state.activeKey = undefined;
  });
});
