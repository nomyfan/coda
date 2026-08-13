import { expect, test } from "vitest";

import {
  codaStore,
  selectSessionListServers,
  type OpenedSession,
  type ServerState,
} from "../src/store/session.ts";

function openedSession(overrides: Partial<OpenedSession> = {}): OpenedSession {
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
    running: true,
    evicted: false,
    usage: [],
    ...overrides,
  };
}

test("streamed transcript chunks keep the session-list selection stable", () => {
  const url = "ws://performance-test";
  const catalog = [{ id: "ws", path: "/tmp/ws", sessions: [] }];
  const session = openedSession();
  const server: ServerState = {
    url,
    status: "connected",
    catalog,
    providers: [],
    sessions: { "ws/s1": session },
  };
  const initial = codaStore.getState();
  const beforeState = { ...initial, order: [url], servers: { [url]: server } };
  const before = selectSessionListServers(beforeState);

  const afterChunk = selectSessionListServers({
    ...beforeState,
    servers: {
      [url]: {
        ...server,
        sessions: {
          "ws/s1": {
            ...session,
            entries: [
              {
                id: "reasoning-1",
                kind: "reasoning",
                content: "more thinking",
                status: "thinking",
              },
            ],
          },
        },
      },
    },
  });

  expect(afterChunk).toBe(before);
  expect(afterChunk[0]).toBe(before[0]);

  const afterRun = selectSessionListServers({
    ...beforeState,
    servers: {
      [url]: {
        ...server,
        sessions: { "ws/s1": { ...session, running: false } },
      },
    },
  });

  expect(afterRun).not.toBe(before);
  expect(afterRun[0].sessions["ws/s1"]).toEqual({ running: false, approvalCount: 0 });
});
