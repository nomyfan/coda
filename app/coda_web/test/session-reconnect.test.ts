import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { HistoryMessage, SessionAccess } from "../src/lib/protocol.ts";
import {
  codaStore,
  compactActiveSession,
  connectServer,
  forkSession,
  openSession,
  selectCanForkSession,
  sendTask,
} from "../src/store/session.ts";

type Frame = { id?: number; method: string; params?: Record<string, unknown> };
class Socket {
  static OPEN = 1;
  static instances: Socket[] = [];
  readyState = 0;
  onopen?: () => void;
  onclose?: () => void;
  onmessage?: (event: { data: string }) => void;
  sent: Frame[] = [];
  constructor(_: string) {
    Socket.instances.push(this);
  }
  send(data: string) {
    this.sent.push(JSON.parse(data));
  }
  open() {
    this.readyState = Socket.OPEN;
    this.onopen?.();
  }
  close() {
    if (this.readyState === 3) return;
    this.readyState = 3;
    this.onclose?.();
  }
  request(method: string, sessionId?: string) {
    return this.sent.findLast(
      (frame) =>
        frame.method === method &&
        (sessionId === undefined || frame.params?.session_id === sessionId),
    );
  }
  reply(frame: Frame, result: unknown) {
    this.onmessage?.({ data: JSON.stringify({ jsonrpc: "2.0", id: frame.id, result }) });
  }
}

const server = "ws://reconnect-test";
const writable: SessionAccess = { type: "read_write" };
const readOnly: SessionAccess = { type: "read_only", reason: "model_not_configured" };
const images = ["data:image/png;base64,eA=="];
const text = "repeat this input";
const user = (id: string, attached = images): HistoryMessage => ({
  User: {
    message_id: id,
    parts: [{ type: "text", text }, ...attached.map((url) => ({ type: "image" as const, url }))],
    created_at: "2026-09-12T00:00:00Z",
  },
});
const catalog = (firstAccess = writable) => ({
  workspaces: [
    {
      id: "ws",
      path: "/workspace",
      sessions: [
        { id: "s1", name: null, has_pending_approval: false, access: firstAccess },
        { id: "s2", name: null, has_pending_approval: false, access: writable },
      ],
    },
  ],
});
const snapshot = (sessionId: string, access = writable, messages: HistoryMessage[] = []) => ({
  workspace_id: "ws",
  session_id: sessionId,
  provider_id: "p:m",
  reasoning_effort: null,
  permission_mode: "accept_edits",
  access,
  messages,
  pending_approvals: [],
  turn_running: false,
  compacting: false,
  background_tasks: [],
  background_tasks_error: null,
});

beforeEach(() => {
  vi.stubGlobal("WebSocket", Socket);
  const values = new Map<string, string>();
  vi.stubGlobal("window", {
    localStorage: {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
      removeItem: (key: string) => values.delete(key),
    },
  });
});
afterEach(async () => {
  for (const socket of Socket.instances) socket.close();
  await new Promise((resolve) => setTimeout(resolve, 0));
  codaStore.setState((state) => {
    delete state.servers[server];
    delete state.rpcMap[server];
    delete state.wsMap[server];
    state.order = state.order.filter((url) => url !== server);
    state.activeServer = undefined;
    state.activeKey = undefined;
    state.forking = {};
  });
  Socket.instances = [];
  vi.unstubAllGlobals();
});

async function connect(firstAccess = writable) {
  connectServer(server);
  const socket = Socket.instances.at(-1)!;
  socket.open();
  socket.reply(socket.request("list_workspaces")!, catalog(firstAccess));
  socket.reply(socket.request("list_providers")!, { providers: [], default_provider: "p:m" });
  await vi.waitFor(() => expect(codaStore.getState().servers[server].catalog).toHaveLength(1));
  return socket;
}
async function open(socket: Socket, id: string, messages: HistoryMessage[] = []) {
  openSession(server, "ws", id);
  socket.reply(socket.request("open_session", id)!, snapshot(id, writable, messages));
  await vi.waitFor(() =>
    expect(codaStore.getState().servers[server].sessions[`ws/${id}`].access).toEqual(writable),
  );
}

for (const access of [readOnly, writable]) {
  test.each([
    { result: "not stored", newMessage: undefined, shouldRecover: true },
    { result: "stored", newMessage: user("new"), shouldRecover: false },
    {
      result: "stored with different images",
      newMessage: user("new", ["different-image"]),
      shouldRecover: true,
    },
  ])(
    `a disconnected task is reconciled on ${access.type} reopen: $result`,
    async ({ newMessage, shouldRecover }) => {
      const first = await connect();
      // An identical older message must not be mistaken for this submission.
      await open(first, "s1", [user("old")]);
      const sending = sendTask(text, images);
      expect(first.request("task")?.params).toMatchObject({ task: text, images });
      first.close();
      await sending;
      expect(
        codaStore
          .getState()
          .servers[server].sessions["ws/s1"].entries.filter((entry) => entry.kind === "user"),
      ).toHaveLength(1);
      const reconnected = await connect(access);
      const messages = newMessage ? [user("old"), newMessage] : [user("old")];
      reconnected.reply(
        reconnected.request("open_session", "s1")!,
        snapshot("s1", access, messages),
      );
      await vi.waitFor(() =>
        expect(codaStore.getState().servers[server].sessions["ws/s1"].access).toEqual(access),
      );
      const reopened = codaStore.getState().servers[server].sessions["ws/s1"];
      expect(reopened.unsentDraft).toEqual(shouldRecover ? { text, images } : undefined);
      expect(reopened.entries.filter((entry) => entry.kind === "user")).toHaveLength(
        messages.length,
      );
      expect(reopened.running).toBe(false);
      expect(reconnected.request("task")).toBeUndefined();
    },
  );
}

test("a cached non-current session can fork using the catalog after reconnect", async () => {
  const first = await connect();
  await open(first, "s1");
  await open(first, "s2");
  first.close();
  const reconnected = await connect();
  reconnected.reply(reconnected.request("open_session", "s2")!, snapshot("s2"));
  await vi.waitFor(() =>
    expect(codaStore.getState().servers[server].sessions["ws/s2"].access).toEqual(writable),
  );
  expect(codaStore.getState().servers[server].sessions["ws/s1"].access).toBeNull();
  expect(selectCanForkSession(codaStore.getState(), server, "ws", "s1")).toBe(true);
  const forking = forkSession(server, "ws", "s1");
  const request = reconnected.request("fork_session", "s1");
  expect(request).toBeDefined();
  reconnected.reply(request!, { session_id: "copy", name: null, ...catalog() });
  await forking;
  reconnected.reply(reconnected.request("open_session", "copy")!, snapshot("copy"));
  await vi.waitFor(() => expect(codaStore.getState().activeKey).toBe("ws/copy"));
});

test("a reconnect catalog cannot authorize the current session before its snapshot", async () => {
  const first = await connect();
  await open(first, "s1");
  first.close();
  const reconnected = await connect();
  expect(selectCanForkSession(codaStore.getState(), server, "ws", "s1")).toBe(false);
  await forkSession(server, "ws", "s1");
  expect(reconnected.request("fork_session")).toBeUndefined();
  reconnected.reply(reconnected.request("open_session", "s1")!, snapshot("s1", readOnly));
  await vi.waitFor(() =>
    expect(codaStore.getState().servers[server].sessions["ws/s1"].access).toEqual(readOnly),
  );
  await forkSession(server, "ws", "s1");
  expect(reconnected.request("fork_session")).toBeUndefined();
});

test("a cached session uses the new read-only catalog after reconnect", async () => {
  const first = await connect();
  await open(first, "s1");
  await open(first, "s2");
  first.close();
  const reconnected = await connect(readOnly);
  await vi.waitFor(() =>
    expect(codaStore.getState().servers[server].catalog[0].sessions[0].access).toEqual(readOnly),
  );
  await forkSession(server, "ws", "s1");
  expect(reconnected.request("fork_session")).toBeUndefined();
});

test("an old catalog cannot authorize a fork before the reconnect catalog arrives", async () => {
  const first = await connect();
  await open(first, "s1");
  await open(first, "s2");
  first.close();
  connectServer(server);
  const reconnected = Socket.instances.at(-1)!;
  reconnected.open();
  expect(selectCanForkSession(codaStore.getState(), server, "ws", "s1")).toBe(false);
  await forkSession(server, "ws", "s1");
  expect(reconnected.request("fork_session")).toBeUndefined();
  reconnected.reply(reconnected.request("list_workspaces")!, catalog());
  await vi.waitFor(() =>
    expect(selectCanForkSession(codaStore.getState(), server, "ws", "s1")).toBe(true),
  );
});

test("an explicit task rejection is not retained as uncertain delivery", async () => {
  const socket = await connect();
  await open(socket, "s1");
  const sending = sendTask(text, images);
  socket.onmessage?.({
    data: JSON.stringify({
      jsonrpc: "2.0",
      id: socket.request("task")!.id,
      error: { code: -32006, message: "session busy" },
    }),
  });
  await sending;
  socket.close();
  const reconnected = await connect(readOnly);
  reconnected.reply(reconnected.request("open_session", "s1")!, snapshot("s1", readOnly));
  await vi.waitFor(() =>
    expect(codaStore.getState().servers[server].sessions["ws/s1"].access).toEqual(readOnly),
  );
  expect(codaStore.getState().servers[server].sessions["ws/s1"].unsentDraft).toBeUndefined();
});

test("uncertain input is kept if its previous history was removed", async () => {
  const first = await connect();
  await open(first, "s1", [user("old")]);
  const sending = sendTask(text, images);
  first.close();
  await sending;
  const reconnected = await connect(readOnly);
  // A matching message at an unknown position cannot confirm this submission.
  reconnected.reply(
    reconnected.request("open_session", "s1")!,
    snapshot("s1", readOnly, [user("different-history")]),
  );
  await vi.waitFor(() =>
    expect(codaStore.getState().servers[server].sessions["ws/s1"].access).toEqual(readOnly),
  );
  expect(codaStore.getState().servers[server].sessions["ws/s1"].unsentDraft).toEqual({
    text,
    images,
  });
});

test.each([false, true])(
  "a disconnected compact command is reconciled before draft recovery (stored: %s)",
  async (stored) => {
    const first = await connect();
    await open(first, "s1", [user("old")]);
    const compacting = compactActiveSession("keep decisions");
    expect(first.request("compact")).toBeDefined();
    first.close();
    await compacting;
    const reconnected = await connect(readOnly);
    const command: HistoryMessage = {
      User: {
        message_id: "compact-command",
        parts: [{ type: "text", text: "/compact keep decisions" }],
        created_at: "2026-09-12T00:00:00Z",
      },
    };
    reconnected.reply(
      reconnected.request("open_session", "s1")!,
      snapshot("s1", readOnly, stored ? [user("old"), command] : [user("old")]),
    );
    await vi.waitFor(() =>
      expect(codaStore.getState().servers[server].sessions["ws/s1"].access).toEqual(readOnly),
    );
    const reopened = codaStore.getState().servers[server].sessions["ws/s1"];
    expect(reopened.unsentDraft).toEqual(
      stored ? undefined : { text: "/compact keep decisions", images: [] },
    );
    expect(reopened.compacting).toBe(false);
    expect(reconnected.request("compact")).toBeUndefined();
  },
);

test("a compaction still running after reconnect is checked only once it finishes", async () => {
  const first = await connect();
  await open(first, "s1", [user("old")]);
  const compacting = compactActiveSession("keep decisions");
  first.close();
  await compacting;
  const reconnected = await connect();
  reconnected.reply(reconnected.request("open_session", "s1")!, {
    ...snapshot("s1", writable, [user("old")]),
    compacting: true,
  });
  await vi.waitFor(() =>
    expect(codaStore.getState().servers[server].sessions["ws/s1"].compacting).toBe(true),
  );
  expect(codaStore.getState().servers[server].sessions["ws/s1"].unsentDraft).toBeUndefined();
  const command: HistoryMessage = {
    User: {
      message_id: "compact-command",
      parts: [{ type: "text", text: "/compact keep decisions" }],
      created_at: "2026-09-12T00:00:00Z",
    },
  };
  reconnected.onmessage?.({
    data: JSON.stringify({
      jsonrpc: "2.0",
      method: "snapshot",
      params: snapshot("s1", writable, [user("old"), command]),
    }),
  });
  await vi.waitFor(() =>
    expect(codaStore.getState().servers[server].sessions["ws/s1"].compacting).toBe(false),
  );
  expect(codaStore.getState().servers[server].sessions["ws/s1"].unsentDraft).toBeUndefined();
  expect(reconnected.request("compact")).toBeUndefined();
});
