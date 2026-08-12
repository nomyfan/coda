import { expect, test } from "vitest";

import { codaStore, submitApprovals, type OpenedSession } from "../src/store/session.ts";
import type { PendingApproval } from "../src/lib/protocol.ts";

function approval(...callIds: string[]): PendingApproval {
  return {
    thread_id: "s1",
    agent_name: "coda",
    parent_message_id: "m-batch-1",
    calls: callIds.map((id) => ({ id, name: "ls", arguments: "{}" })),
    suspended_at: "2026-08-12T00:00:00Z",
    suggested_shell_allow_patterns: {},
  } as PendingApproval;
}

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
    evicted: false,
    usage: [],
    ...overrides,
  } as OpenedSession;
}

// A resume answers one batch of calls, and the server treats a call the
// decision doesn't name as rejected — so a decision sent twice lands on
// whatever the agent suspended on next and rejects that batch wholesale. The
// approval only leaves the store once its allow-pattern writes have settled,
// which is exactly the window a second click falls into.
test("a second submit of the same approval while one is in flight is dropped", async () => {
  const server = "ws://approvals";
  let releaseAllowWrite: (value: unknown) => void = () => {};
  const allowWrite = new Promise((resolve) => (releaseAllowWrite = resolve));
  const resumes: unknown[] = [];

  codaStore.setState((state) => {
    state.servers[server] = {
      url: server,
      status: "connected",
      catalog: [],
      providers: [],
      sessions: {
        "ws/s1": session({
          approvals: [approval("call_a1")],
          drafts: { "coda:s1": { call_a1: "Execute" } },
          allowDrafts: { "coda:s1": { call_a1: "ls *" } },
        }),
      },
    };
    state.activeServer = server;
    state.activeKey = "ws/s1";
    state.rpcMap[server] = {
      notify: (_method: string, params: unknown) => {
        resumes.push(params);
        return true;
      },
      // Holds the submit open at its one await point, the way a real
      // `add_allow_pattern` round trip does.
      request: () => allowWrite,
    } as never;
  });

  // Both clicks land inside the window: the first submit is parked on its
  // allow-pattern write, and the approval it will answer is still in the store.
  const first = submitApprovals();
  const second = submitApprovals();
  releaseAllowWrite(undefined);
  await Promise.all([first, second]);

  expect(resumes).toHaveLength(1);
  // The batch the decision answers has to travel with it — without it the
  // server cannot tell this resume from one meant for an earlier batch.
  expect(resumes[0]).toMatchObject({ decision: { parent_message_id: "m-batch-1" } });
  expect(codaStore.getState().servers[server]?.sessions["ws/s1"]?.approvals).toEqual([]);

  // With the approval gone the guard is released, and nothing is left to send.
  await submitApprovals();
  expect(resumes).toHaveLength(1);

  codaStore.setState((state) => {
    delete state.rpcMap[server];
    delete state.servers[server];
    state.activeServer = null;
    state.activeKey = null;
  });
});
