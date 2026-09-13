import { afterEach, expect, test, vi } from "vitest";
import { createElement, type ReactNode } from "react";
import { renderToStaticMarkup } from "react-dom/server";

vi.mock("../src/store/session.ts", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../src/store/session.ts")>();
  return {
    ...actual,
    useCodaStore: (selector: Parameters<typeof actual.useCodaStore>[0]) =>
      selector(actual.codaStore.getState()),
  };
});
// Expose menu items during server rendering so their disabled state is testable.
vi.mock("../src/components/ui/select.tsx", async () => {
  const { createElement } = await import("react");
  const container = ({ children }: { children?: ReactNode }) =>
    createElement("div", null, children);
  return {
    Select: container,
    SelectContent: container,
    SelectGroup: container,
    SelectLabel: container,
    SelectSeparator: container,
    SelectTrigger: container,
    SelectValue: container,
    SelectItem: ({
      value,
      disabled,
      children,
    }: {
      value: string;
      disabled?: boolean;
      children: ReactNode;
    }) => createElement("button", { "data-model-id": value, disabled }, children),
  };
});
vi.mock("../src/components/sidebar.tsx", () => ({ Sidebar: () => null }));
vi.mock("../src/components/transcript.tsx", () => ({ Transcript: () => null }));
vi.mock("../src/components/theme-toggle.tsx", () => ({ ThemeToggle: () => null }));

import App from "../src/App.tsx";
import { applySnapshotToSession, codaStore, type OpenedSession } from "../src/store/session.ts";
import type { HistoryMessage, ProviderInfo } from "../src/lib/protocol.ts";

const server = "ws://model-recovery-selection";
const at = "2026-09-13T00:00:00Z";
const image = "data:image/png;base64,eA==";
const messages: HistoryMessage[] = [
  { User: { message_id: "old-user", parts: [{ type: "image", url: image }], created_at: at } },
  {
    Assistant: {
      message_id: "old-answer",
      content: "Described the image",
      tool_calls: [],
      usage: null,
      reasoning_content: null,
      reasoning_ended_at: null,
      aborted: false,
      started_at: at,
      ended_at: at,
    },
  },
  {
    Compaction: {
      message_id: "summary",
      outcome: { type: "summary", cutoff: "old-answer" },
      content: "Summary of the image",
      created_at: at,
    },
  },
];
const providers: ProviderInfo[] = [
  {
    id: "p:text",
    provider: "p",
    model: "Text",
    family: "f",
    input_modalities: ["text"],
    context_window: 1000,
    reasoning_efforts: [],
  },
  {
    id: "p:vision",
    provider: "p",
    model: "Vision",
    family: "f",
    input_modalities: ["text", "image"],
    context_window: 1000,
    reasoning_efforts: [],
  },
];

function mount(draftImages: string[], candidates = ["p:text", "p:vision"]) {
  const base: OpenedSession = {
    key: "ws/s1",
    workspaceId: "ws",
    sessionId: "s1",
    access: { type: "read_only", reason: "model_not_configured" },
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
    backgroundTasks: [],
    permissionMode: "accept_edits",
    usage: [],
    unsentDraft: { text: "continue", images: draftImages },
  };
  const session = applySnapshotToSession(base, {
    messages,
    approvals: [],
    access: base.access!,
    providerId: "p:removed-preview",
    reasoningEffort: null,
    modelFamily: "f",
    modelCandidates: candidates,
    permissionMode: "accept_edits",
    turnRunning: false,
  });
  // Compaction keeps the old image visible in the transcript.
  expect(session.entries.some((entry) => entry.images?.length)).toBe(true);
  codaStore.setState((state) => {
    state.activeServer = server;
    state.activeKey = "ws/s1";
    state.order = [server];
    state.servers[server] = {
      url: server,
      status: "connected",
      providers,
      sessions: { "ws/s1": session },
      catalog: [
        {
          id: "ws",
          path: "/workspace",
          sessions: [{ id: "s1", name: null, access: session.access! }],
        },
      ],
    };
  });
  return renderToStaticMarkup(createElement(App));
}

function item(html: string, id: string) {
  const tag = html.match(new RegExp(`<button[^>]*data-model-id="${id}"[^>]*>`))?.[0];
  expect(tag).toBeDefined();
  return tag!;
}

afterEach(() => {
  codaStore.setState((state) => {
    delete state.servers[server];
    state.activeServer = undefined;
    state.activeKey = undefined;
    state.order = [];
  });
});

test("compacted historical images do not disable server-approved text replacements", () => {
  expect(item(mount([]), "p:text")).not.toContain('disabled=""');
});

test("unsubmitted draft images still restrict server-approved replacements", () => {
  const html = mount([image]);
  expect(item(html, "p:text")).toContain('disabled=""');
  expect(item(html, "p:vision")).not.toContain('disabled=""');
});

test("the selector never enables a model excluded by the server", () => {
  expect(item(mount([], ["p:vision"]), "p:text")).toContain('disabled=""');
});
