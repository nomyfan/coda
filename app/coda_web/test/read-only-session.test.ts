import { afterEach, expect, test, vi } from "vitest";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
vi.mock("../src/store/session.ts", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../src/store/session.ts")>();
  return {
    ...actual,
    useCodaStore: (selector: Parameters<typeof actual.useCodaStore>[0]) =>
      selector(actual.codaStore.getState()),
  };
});
import { Composer } from "../src/components/composer.tsx";
import { ApprovalPanel } from "../src/components/approval-panel.tsx";
import { ModelSelector } from "../src/components/model-selector.tsx";
import type { PendingApproval, SessionAccess } from "../src/lib/protocol.ts";
import {
  abort,
  applySnapshotToSession,
  beginEdit,
  codaStore,
  compactActiveSession,
  draftCall,
  forkSession,
  getBackgroundTaskResult,
  killBackgroundTask,
  openSession,
  renameSession,
  rewindTurn,
  selectCanRewind,
  sendTask,
  setAllowDraft,
  setModel,
  setPermissionMode,
  submitApprovals,
  type OpenedSession,
} from "../src/store/session.ts";

const server = "ws://read-only-test";
const access: SessionAccess = { type: "read_only", reason: "model_not_configured" };
const approval: PendingApproval = {
  task_id: null,
  pid: "s1",
  agent_name: "coda",
  agent_path: ["coda"],
  parent_message_id: "assistant",
  suspended_at: "2026-09-12T00:00:00Z",
  calls: [{ id: "call", name: "shell", arguments: '{"command":"echo test"}' }],
  suggested_shell_allow_patterns: { call: "echo *" },
};

function session(): OpenedSession {
  return {
    access,
    key: "ws/s1",
    workspaceId: "ws",
    sessionId: "s1",
    entries: [],
    activity: [],
    approvals: [approval],
    pendingCallInfo: {},
    generationSpans: {},
    drafts: {},
    allowDrafts: {},
    running: false,
    compacting: false,
    backgroundTasks: [],
    evicted: false,
    permissionMode: "accept_edits",
    usage: [],
    providerId: "removed:model",
    reasoningEffort: "old-effort",
  };
}

function mount(opened = session()) {
  const request = vi.fn(async (method: string) =>
    method === "rename_session" ? { name: "Renamed" } : { state: "unknown" },
  );
  const notify = vi.fn(() => true);
  codaStore.setState((state) => {
    state.activeServer = server;
    state.activeKey = opened.key;
    state.servers[server] = {
      url: server,
      status: "connected",
      providers: [],
      sessions: { [opened.key]: opened },
      catalog: [
        {
          id: "ws",
          path: "/workspace",
          sessions: [{ id: "s1", name: null, has_pending_approval: true, access }],
        },
      ],
    };
    state.rpcMap[server] = { request, notify } as never;
  });
  return { request, notify };
}

afterEach(() => {
  codaStore.setState((state) => {
    delete state.servers[server];
    delete state.rpcMap[server];
    state.activeServer = undefined;
    state.activeKey = undefined;
  });
});

test("read-only actions cannot send RPCs, change settings, or apply approvals", async () => {
  for (const available of [access, null]) {
    const { request, notify } = mount({ ...session(), access: available });
    await sendTask("continue");
    await compactActiveSession("summary");
    beginEdit("user");
    await rewindTurn("rewrite");
    setModel("new:model", null);
    setPermissionMode("yolo");
    draftCall(approval, approval.calls[0], "Execute");
    setAllowDraft(approval, approval.calls[0], "echo *");
    await submitApprovals();
    abort();
    killBackgroundTask("bg_1");
    await forkSession(server, "ws", "s1");
    expect(request).not.toHaveBeenCalled();
    expect(notify).not.toHaveBeenCalled();
    expect(selectCanRewind(codaStore.getState())).toBe(false);
    expect(codaStore.getState().servers[server].sessions["ws/s1"]).toMatchObject({
      providerId: "removed:model",
      permissionMode: "accept_edits",
      drafts: {},
      allowDrafts: {},
      approvals: [approval],
    });
  }
});

test("read-only history retains pending approvals and preserves recovered drafts", () => {
  const applied = applySnapshotToSession(
    {
      ...session(),
      running: true,
      unsentDraft: { text: "unsent text", images: ["data:image/png;base64,eA=="] },
    },
    {
      access,
      providerId: "removed:model",
      reasoningEffort: "old-effort",
      permissionMode: "accept_edits",
      messages: [],
      approvals: [approval],
      turnRunning: false,
      backgroundTasks: [],
    },
  );
  expect(applied.running).toBe(false);
  expect(applied.entries).toEqual([]);
  expect(applied.unsentDraft).toEqual({
    text: "unsent text",
    images: ["data:image/png;base64,eA=="],
  });
  expect(applied.approvals).toEqual([approval]);
  const recovered = applySnapshotToSession(applied, {
    access: { type: "read_write" },
    providerId: "removed:model",
    reasoningEffort: "old-effort",
    permissionMode: "accept_edits",
    messages: [],
    approvals: [approval],
    turnRunning: false,
  });
  expect(recovered.access).toEqual({ type: "read_write" });
});

test("rename and archived result reads remain available", async () => {
  const { request } = mount();
  await renameSession(server, "ws", "s1", "Renamed");
  await getBackgroundTaskResult("bg_1");
  expect(request.mock.calls.map(([method]) => method)).toEqual([
    "rename_session",
    "get_task_result",
  ]);
});

test("reopening disables writes until a read-only snapshot arrives", async () => {
  const { request } = mount({ ...session(), access: { type: "read_write" }, approvals: [] });
  let resolve!: (value: unknown) => void;
  request.mockImplementation(
    () =>
      new Promise((done) => {
        resolve = done;
      }) as never,
  );
  openSession(server, "ws", "s1");
  expect(codaStore.getState().servers[server].sessions["ws/s1"].access).toBeNull();
  await sendTask("must wait");
  expect(request).toHaveBeenCalledTimes(1);
  resolve({
    workspace_id: "ws",
    session_id: "s1",
    messages: [],
    pending_approvals: [approval],
    provider_id: "removed:model",
    access,
    background_tasks_error: null,
  });
  await vi.waitFor(() =>
    expect(codaStore.getState().servers[server].sessions["ws/s1"].access).toEqual(access),
  );
});

test("composer and approval panel keep readable content while disabling mutations", () => {
  mount();
  const html = renderToStaticMarkup(
    createElement(Composer, {
      writable: false,
      access,
      status: "connected",
      running: false,
      compacting: false,
      approvalPending: true,
      starting: false,
      evicted: false,
      workspace: "ws",
      selectingTarget: false,
      permissionMode: "accept_edits",
      providers: [],
      providerId: "removed:model",
      reasoningEffort: "old-effort",
      usage: [],
      sessionHasImages: true,
      serverUrl: server,
      workspaceId: "ws",
      unsentDraft: { text: "keep this draft", images: [] },
      onForkDraftChange: vi.fn(),
      onSetModel: vi.fn(),
      onSetPermissionMode: vi.fn(),
      onSend: vi.fn(),
      onAbort: vi.fn(),
      onCancelEdit: vi.fn(),
    }),
  );
  expect(html).toContain("This conversation is read-only");
  expect(html).toContain("removed:model");
  expect(html).toContain("keep this draft");
  expect(html).toMatch(/<textarea[^>]*readOnly=""/);
  const panel = renderToStaticMarkup(createElement(ApprovalPanel));
  expect(panel).toMatch(/<fieldset[^>]*disabled=""/);
  expect(panel).toContain("echo test");
  expect(panel).toContain("Submit");
});

test("unsupported saved reasoning effort stays visible", () => {
  const html = renderToStaticMarkup(
    createElement(ModelSelector, {
      providers: [
        {
          id: "p:m",
          provider: "p",
          model: "m",
          context_window: 1000,
          reasoning_efforts: ["low"],
          input_modalities: ["text"],
        },
      ],
      providerId: "p:m",
      reasoningEffort: "removed-effort",
      disabled: true,
      modelLocked: true,
      requireImageModel: false,
      serverUrl: server,
      workspaceId: "ws",
      onSetModel: vi.fn(),
    }),
  );
  expect(html).toContain("removed-effort");
});
