import { expect, test } from "vitest";

import { parseCompactCommand } from "../src/lib/compact-command.ts";
import type { HistoryMessage } from "../src/lib/protocol.ts";
import {
  applySnapshotToSession,
  beginEdit,
  codaStore,
  compactActiveSession,
  deleteSession,
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

function assistantMessage(id: string, promptTokens: number): HistoryMessage {
  return {
    Assistant: {
      message_id: id,
      content: "done",
      tool_calls: [],
      usage: {
        prompt_tokens: promptTokens,
        completion_tokens: 10,
        total_tokens: promptTokens + 10,
      },
      started_at: "2026-08-16T00:00:00Z",
      ended_at: "2026-08-16T00:00:01Z",
    },
  };
}

function compactionMessage(id: string): HistoryMessage {
  return {
    Custom: {
      message_id: id,
      kind: "compaction",
      role: "User",
      content: "Summary",
      created_at: "2026-08-16T00:00:02Z",
    },
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
      sessions: {
        "ws/s1": session({
          entries: [{ id: "user:1", kind: "user", messageId: "m1", content: "do the thing" }],
        }),
      },
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

test("compactActiveSession shows the /compact line before the summary lands", async () => {
  const server = "ws://compaction-optimistic";
  let resolveRequest!: (result: { outcome: "applied" }) => void;
  codaStore.setState((state) => {
    state.activeServer = server;
    state.activeKey = "ws/s1";
    state.servers[server] = {
      url: server,
      status: "connected",
      catalog: [],
      providers: [],
      sessions: {
        "ws/s1": session({
          entries: [{ id: "user:1", kind: "user", messageId: "m1", content: "do the thing" }],
        }),
      },
    };
    state.rpcMap[server] = {
      request: () =>
        new Promise<{ outcome: "applied" }>((resolve) => {
          resolveRequest = resolve;
        }),
    } as never;
  });

  const pending = compactActiveSession("keep decisions");
  expect(codaStore.getState().servers[server]?.sessions["ws/s1"]?.entries).toEqual([
    expect.objectContaining({
      id: "user:1",
      kind: "user",
      messageId: "m1",
      content: "do the thing",
    }),
    expect.objectContaining({
      id: expect.any(String),
      kind: "user",
      pendingCompact: true,
      content: "/compact keep decisions",
    }),
  ]);
  // The optimistic copy is not a turn: it must not offer the abort button.
  expect(codaStore.getState().servers[server]?.sessions["ws/s1"]?.running).toBe(false);
  expect(codaStore.getState().servers[server]?.sessions["ws/s1"]?.compacting).toBe(false);

  resolveRequest({ outcome: "applied" });
  await pending;
  // `applied` keeps the line — the end-of-compaction snapshot retires it.
  expect(codaStore.getState().servers[server]?.sessions["ws/s1"]?.entries).toHaveLength(2);
  codaStore.setState((state) => {
    delete state.servers[server];
    delete state.rpcMap[server];
    state.activeServer = undefined;
    state.activeKey = undefined;
  });
});

test("a rejected compaction drops its optimistic line", async () => {
  const server = "ws://compaction-rejected";
  codaStore.setState((state) => {
    state.activeServer = server;
    state.activeKey = "ws/s1";
    state.servers[server] = {
      url: server,
      status: "connected",
      catalog: [],
      providers: [],
      sessions: {
        "ws/s1": session({
          entries: [{ id: "user:1", kind: "user", messageId: "m1", content: "do the thing" }],
        }),
      },
    };
    state.rpcMap[server] = {
      request: () =>
        Promise.resolve({
          outcome: "abandoned",
          stale: true,
          reason: "the conversation changed while it was being summarized",
        }),
    } as never;
  });

  await compactActiveSession("keep decisions");

  const after = codaStore.getState().servers[server]?.sessions["ws/s1"];
  expect(after?.entries).toEqual([
    expect.objectContaining({
      id: "user:1",
      kind: "user",
      messageId: "m1",
      content: "do the thing",
    }),
  ]);
  expect(after?.activity).toEqual([expect.objectContaining({ label: "compaction not applied" })]);
  codaStore.setState((state) => {
    delete state.servers[server];
    delete state.rpcMap[server];
    state.activeServer = undefined;
    state.activeKey = undefined;
  });
});

test("an RPC error drops the optimistic /compact line too", async () => {
  const server = "ws://compaction-error";
  codaStore.setState((state) => {
    state.activeServer = server;
    state.activeKey = "ws/s1";
    state.servers[server] = {
      url: server,
      status: "connected",
      catalog: [],
      providers: [],
      sessions: {
        "ws/s1": session({
          entries: [{ id: "user:1", kind: "user", messageId: "m1", content: "do the thing" }],
        }),
      },
    };
    state.rpcMap[server] = {
      request: () => Promise.reject(new Error("SESSION_NOT_IDLE")),
    } as never;
  });

  await compactActiveSession("");

  const after = codaStore.getState().servers[server]?.sessions["ws/s1"];
  expect(after?.entries).toEqual([
    expect.objectContaining({
      id: "user:1",
      kind: "user",
      messageId: "m1",
      content: "do the thing",
    }),
  ]);
  expect(after?.activity).toEqual([expect.objectContaining({ label: "compaction rejected" })]);
  codaStore.setState((state) => {
    delete state.servers[server];
    delete state.rpcMap[server];
    state.activeServer = undefined;
    state.activeKey = undefined;
  });
});

test("the start-of-compaction snapshot keeps the optimistic line without running a turn", () => {
  const before = session({
    entries: [
      { id: "user:1", kind: "user", messageId: "m1", content: "do the thing" },
      { id: "user:opt", kind: "user", pendingCompact: true, content: "/compact keep decisions" },
    ],
  });
  const after = applySnapshotToSession(before, {
    messages: [
      {
        User: {
          message_id: "m1",
          parts: [{ type: "text", text: "do the thing" }],
          created_at: "2026-08-16T00:00:00Z",
        },
      },
    ],
    approvals: [],
    providerId: "provider:model",
    reasoningEffort: null,
    permissionMode: "accept_edits",
    turnRunning: false,
    compacting: true,
  });

  expect(after.compacting).toBe(true);
  expect(after.running).toBe(false);
  expect(after.entries).toEqual([
    expect.objectContaining({ id: "user:m1", messageId: "m1" }),
    expect.objectContaining({
      id: "user:opt",
      kind: "user",
      pendingCompact: true,
      content: "/compact keep decisions",
    }),
  ]);
});

test("a repeated /compact line stays optimistic until its compaction finishes", () => {
  const before = session({
    entries: [
      {
        id: "user:old-command",
        kind: "user",
        messageId: "old-command",
        content: "/compact keep decisions",
      },
      {
        id: "user:opt",
        kind: "user",
        pendingCompact: true,
        content: "/compact keep decisions",
      },
    ],
  });
  const after = applySnapshotToSession(before, {
    messages: [
      {
        User: {
          message_id: "old-command",
          parts: [{ type: "text", text: "/compact keep decisions" }],
          created_at: "2026-08-16T00:00:00Z",
        },
      },
      compactionMessage("old-summary"),
    ],
    approvals: [],
    providerId: "provider:model",
    reasoningEffort: null,
    permissionMode: "accept_edits",
    turnRunning: false,
    compacting: true,
  });

  expect(after.entries).toEqual([
    expect.objectContaining({ id: "user:old-command", messageId: "old-command" }),
    expect.objectContaining({ id: "compaction:old-summary" }),
    expect.objectContaining({
      id: "user:opt",
      pendingCompact: true,
      content: "/compact keep decisions",
    }),
  ]);
});

test("the end-of-compaction snapshot retires the optimistic copy by content", () => {
  const before = session({
    entries: [
      { id: "user:1", kind: "user", messageId: "m1", content: "do the thing" },
      { id: "user:opt", kind: "user", pendingCompact: true, content: "/compact keep decisions" },
    ],
  });
  const after = applySnapshotToSession(before, {
    messages: [
      {
        User: {
          message_id: "m1",
          parts: [{ type: "text", text: "do the thing" }],
          created_at: "2026-08-16T00:00:00Z",
        },
      },
      {
        User: {
          message_id: "cmd-1",
          parts: [{ type: "text", text: "/compact keep decisions" }],
          created_at: "2026-08-16T00:00:01Z",
        },
      },
    ],
    approvals: [],
    providerId: "provider:model",
    reasoningEffort: null,
    permissionMode: "accept_edits",
    turnRunning: false,
    compacting: false,
  });

  expect(after.entries).toEqual([
    expect.objectContaining({ id: "user:m1", messageId: "m1" }),
    expect.objectContaining({
      id: "user:cmd-1",
      messageId: "cmd-1",
      content: "/compact keep decisions",
    }),
  ]);
  expect(after.entries.some((entry) => entry.pendingCompact)).toBe(false);
});

test("a successful compaction clears usage until the next assistant response", () => {
  const compacted = applySnapshotToSession(session(), {
    messages: [assistantMessage("old-assistant", 900), compactionMessage("summary")],
    approvals: [],
    providerId: "provider:model",
    reasoningEffort: null,
    permissionMode: "accept_edits",
    turnRunning: false,
    compacting: false,
  });
  expect(compacted.usage).toEqual([]);

  const continued = applySnapshotToSession(compacted, {
    messages: [
      assistantMessage("old-assistant", 900),
      compactionMessage("summary"),
      assistantMessage("new-assistant", 100),
    ],
    approvals: [],
    providerId: "provider:model",
    reasoningEffort: null,
    permissionMode: "accept_edits",
    turnRunning: false,
    compacting: false,
  });
  expect(continued.usage).toEqual([
    {
      agentName: "coda",
      usage: { prompt_tokens: 100, completion_tokens: 10, total_tokens: 110 },
    },
  ]);
});

test("compactActiveSession does not call the server when the conversation is empty", async () => {
  const server = "ws://compaction-empty";
  let called = false;
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
      request: () => {
        called = true;
        return Promise.resolve({ outcome: "applied" });
      },
    } as never;
  });

  await compactActiveSession("");

  expect(called).toBe(false);
  expect(codaStore.getState().servers[server]?.sessions["ws/s1"]?.activity).toEqual([
    expect.objectContaining({ label: "nothing to compact" }),
  ]);
  codaStore.setState((state) => {
    delete state.servers[server];
    delete state.rpcMap[server];
    state.activeServer = undefined;
    state.activeKey = undefined;
  });
});

test("deleteSession is a no-op while the session is compacting", () => {
  const server = "ws://compaction-delete";
  let called = false;
  codaStore.setState((state) => {
    state.activeServer = server;
    state.activeKey = "ws/s1";
    state.servers[server] = {
      url: server,
      status: "connected",
      catalog: [],
      providers: [],
      sessions: { "ws/s1": session({ compacting: true }) },
    };
    state.rpcMap[server] = {
      request: () => {
        called = true;
        return Promise.resolve({ workspaces: [] });
      },
    } as never;
  });

  deleteSession(server, "ws", "s1");

  expect(called).toBe(false);
  expect(codaStore.getState().servers[server]?.sessions["ws/s1"]?.deleting).toBeUndefined();
  codaStore.setState((state) => {
    delete state.servers[server];
    delete state.rpcMap[server];
    state.activeServer = undefined;
    state.activeKey = undefined;
  });
});

test("a /compact user message cannot be pulled back into the composer", () => {
  const server = "ws://compaction-edit";
  codaStore.setState((state) => {
    state.activeServer = server;
    state.activeKey = "ws/s1";
    state.servers[server] = {
      url: server,
      status: "connected",
      catalog: [],
      providers: [],
      sessions: {
        "ws/s1": session({
          entries: [
            {
              id: "user:1",
              kind: "user",
              messageId: "cmd",
              content: "/compact keep decisions",
            },
          ],
        }),
      },
    };
  });

  beginEdit("cmd");

  expect(codaStore.getState().servers[server]?.sessions["ws/s1"]?.editing).toBeUndefined();
  codaStore.setState((state) => {
    delete state.servers[server];
    state.activeServer = undefined;
    state.activeKey = undefined;
  });
});
