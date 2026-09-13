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

import { Transcript } from "../src/components/transcript.tsx";
import {
  applySnapshotToSession,
  codaStore,
  reduceEvent,
  type OpenedSession,
} from "../src/store/session.ts";
import type { AssistantMessage, GenerationMetadata } from "../src/lib/protocol.ts";

const initialState = codaStore.getState();
afterEach(() => codaStore.setState(initialState, true));

function session(): OpenedSession {
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
    usage: [],
    running: false,
    evicted: false,
    providerId: "current:selection",
    reasoningEffort: "low",
  };
}

function reply(generation?: GenerationMetadata): AssistantMessage {
  return {
    message_id: "reply",
    content: "answer",
    tool_calls: [],
    generation,
    started_at: "2026-09-13T00:00:00Z",
    ended_at: "2026-09-13T00:00:01Z",
  };
}

function render(session: OpenedSession): string {
  const server = "ws://generation-model";
  codaStore.setState((state) => {
    state.activeServer = server;
    state.activeKey = session.key;
    state.order = [server];
    state.servers[server] = {
      url: server,
      status: "connected",
      providers: [],
      catalog: [],
      sessions: { [session.key]: session },
    };
  });
  return renderToStaticMarkup(createElement(Transcript, { suppressed: false }));
}

test.each([
  ["upstream-version", "upstream-version"],
  ["requested-alias", "requested-alias"],
  [null, "requested-alias (requested)"],
  [undefined, "requested-alias (requested)"],
])("historical label distinguishes the reported model %s from the request", (reported, label) => {
  const message = reply({
    provider_id: "gateway",
    model_id: "requested-alias",
    reasoning_effort: "high",
    reported_model_id: reported,
  });
  const restored = applySnapshotToSession(session(), {
    messages: [{ Assistant: message }],
    approvals: [],
    access: { type: "read_write" },
    providerId: "current:selection",
    reasoningEffort: "low",
    turnRunning: false,
  });
  const html = render(restored);
  expect(html).toContain(`>${label}</span>`);
  expect(html).toContain(`Reported model: ${reported ?? "not recorded"}`);
  expect(html).toContain("Requested model: requested-alias");
  expect(html).toContain("Configured provider: gateway");
  expect(html).toContain("Requested reasoning effort: high");
  expect(html).not.toContain("current:selection");
});

test("a message with no generation metadata does not invent a historical model", () => {
  const restored = applySnapshotToSession(session(), {
    messages: [{ Assistant: reply() }],
    approvals: [],
    access: { type: "read_write" },
    providerId: "current:selection",
    reasoningEffort: null,
    turnRunning: false,
  });
  const html = render(restored);
  expect(html).toContain("answer");
  expect(html).not.toContain("Reported model:");
  expect(html).not.toContain("current:selection");
});

test.each([false, true])(
  "live completion uses the same model label as history (aborted=%s)",
  (aborted) => {
    const message = {
      ...reply({
        provider_id: "gateway",
        model_id: "requested-alias",
        reasoning_effort: null,
        reported_model_id: "live-upstream",
      }),
      aborted,
    };
    const streaming = reduceEvent(session(), {
      type: "llm_chunk",
      agent_name: "coda",
      pid: "root",
      content: message.content,
    });
    expect(render(streaming)).not.toContain("Reported model:");
    const finished = reduceEvent(streaming, {
      type: "llm_end",
      agent_name: "coda",
      pid: "root",
      message,
    });
    const html = render(finished);
    expect(html).toContain(">live-upstream</span>");
    expect(html).toContain("Requested model: requested-alias");
    expect(html).toContain("Requested reasoning effort: not specified");
  },
);
