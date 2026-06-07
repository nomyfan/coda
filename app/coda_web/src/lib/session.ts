import { useCallback, useEffect } from "react";
import { useStore } from "zustand";
import type { Draft } from "immer";
import {
  approvalKey,
  type ClientMessage,
  type CompletionUsage,
  type HistoryMessage,
  type PendingApproval,
  type ServerMessage,
  type ToolCall,
  type ToolCallResolution,
  type ToolMessage,
  type WireEvent,
  type WorkspaceSummary,
  callArguments,
  describeTool,
  outcomeText,
  outputText,
} from "./protocol";
import { useImmutableRef } from "@callcc/toolkit-js/react/useImmutableRef";
import { create, type Store } from "@/store/utils";

export type { WorkspaceSession, WorkspaceSummary } from "./protocol";

export type ConnectionStatus =
  | "idle"
  | "connecting"
  | "connected"
  | "closed"
  | "error";

export type TranscriptEntry = {
  id: string;
  kind: "user" | "assistant" | "tool_call" | "tool_result" | "system" | "error";
  agentName?: string;
  threadId?: string;
  title?: string;
  /** Short summary of what a tool acts on (file basename, shell command, …). */
  detail?: string;
  content: string;
  status?: string;
  usage?: CompletionUsage | null;
  liveKey?: string;
  callId?: string;
};

export type ActivityEntry = {
  id: string;
  tone: "neutral" | "success" | "warning" | "danger" | "cyan";
  label: string;
  detail: string;
};

export type SessionKey = `${string}/${string}`;

export type OpenedSession = {
  key: SessionKey;
  workspaceId: string;
  sessionId: string;
  entries: TranscriptEntry[];
  activity: ActivityEntry[];
  approvals: PendingApproval[];
  drafts: Record<string, Record<string, ToolCallResolution>>;
  running: boolean;
  /** Created locally via "new session" but not yet opened on the server. */
  draft?: boolean;
  /** First user task, used as the session list title before the server persists it. */
  firstUserMessage?: string;
};

/** One connected (or attempted) server, holding its own catalog and sessions. */
export type ServerState = {
  url: string;
  /** User-given display name; falls back to the URL when absent. */
  alias?: string;
  status: ConnectionStatus;
  error?: string;
  catalog: WorkspaceSummary[];
  sessions: Record<SessionKey, OpenedSession>;
};

type CodaState = {
  servers: Record<string, ServerState>;
  /** Stable ordering of `servers` for rendering. */
  order: string[];
  /** The server whose session is currently shown in the center pane. */
  activeServer?: string;
  /** The active session within `activeServer`. */
  activeKey?: SessionKey;
};

type SessionRuntimeState = {
  wsMap: Record<string, WebSocket>;
  autoConnected: boolean;
};

type CodaStoreState = CodaState & SessionRuntimeState;

const rootName = "coda";

function newId(prefix: string) {
  return `${prefix}:${Date.now().toString(36)}:${Math.random()
    .toString(36)
    .slice(2)}`;
}

function freshSessionId() {
  return (
    globalThis.crypto?.randomUUID?.() ?? `session-${Date.now().toString(36)}`
  );
}

function sessionKey(workspaceId: string, sessionId: string): SessionKey {
  return `${workspaceId}/${sessionId}`;
}

function splitKey(key: SessionKey) {
  const index = key.indexOf("/");
  return {
    workspaceId: key.slice(0, index),
    sessionId: key.slice(index + 1),
  };
}

function blankSession(workspaceId: string, sessionId: string): OpenedSession {
  return {
    key: sessionKey(workspaceId, sessionId),
    workspaceId,
    sessionId,
    entries: [],
    activity: [],
    approvals: [],
    drafts: {},
    running: false,
  };
}

function blankServer(url: string): ServerState {
  return {
    url,
    status: "idle",
    catalog: [],
    sessions: {},
  };
}

const initialState: CodaState = {
  servers: {},
  order: [],
};

function initialStoreState(): CodaStoreState {
  return {
    ...initialState,
    wsMap: {},
    autoConnected: false,
  };
}

const serversStorageKey = "coda.servers";
const legacyServerKey = "coda.serverUrl";

export type StoredServer = { url: string; alias?: string };

function loadStoredServers(): StoredServer[] {
  try {
    const raw = window.localStorage.getItem(serversStorageKey);
    if (raw) {
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed)) {
        return parsed
          .map((value): StoredServer | null => {
            // Current format: { url, alias? }. Legacy format: bare URL string.
            if (typeof value === "string" && value.trim()) {
              return { url: value.trim() };
            }
            if (
              value &&
              typeof value === "object" &&
              typeof value.url === "string" &&
              value.url.trim()
            ) {
              const alias =
                typeof value.alias === "string" && value.alias.trim()
                  ? value.alias.trim()
                  : undefined;
              return { url: value.url.trim(), alias };
            }
            return null;
          })
          .filter((value): value is StoredServer => value !== null);
      }
    }
  } catch {
    // ignore malformed/blocked storage
  }
  try {
    const legacy = window.localStorage.getItem(legacyServerKey);
    if (legacy && legacy.trim()) {
      return [{ url: legacy.trim() }];
    }
  } catch {
    // ignore
  }
  return [];
}

function storeServers(servers: StoredServer[]) {
  try {
    window.localStorage.setItem(serversStorageKey, JSON.stringify(servers));
    window.localStorage.removeItem(legacyServerKey);
  } catch {
    // ignore storage failures (private mode, disabled storage)
  }
}

function addStored(list: StoredServer[], url: string): StoredServer[] {
  return list.some((server) => server.url === url) ? list : [...list, { url }];
}

function liveKey(agentName: string, threadId: string) {
  return `${agentName}:${threadId}`;
}

function addActivity(
  session: OpenedSession,
  entry: Omit<ActivityEntry, "id">
): OpenedSession {
  return {
    ...session,
    activity: [{ id: newId("activity"), ...entry }, ...session.activity].slice(
      0,
      80
    ),
  };
}

/** Tool call arguments keyed by call id, harvested from Assistant messages. */
function collectToolArgs(
  messages: HistoryMessage[]
): Record<string, string | null | undefined> {
  const map: Record<string, string | null | undefined> = {};
  for (const message of messages) {
    if ("Assistant" in message) {
      for (const call of message.Assistant.tool_calls) {
        map[call.id] = call.arguments;
      }
    }
  }
  return map;
}

function historyToEntry(
  message: HistoryMessage,
  index: number,
  argsById: Record<string, string | null | undefined> = {}
): TranscriptEntry | null {
  if ("System" in message) {
    return null;
  }
  if ("User" in message) {
    return {
      id: `history:user:${index}`,
      kind: "user",
      content: message.User,
    };
  }
  if ("Assistant" in message) {
    const assistant = message.Assistant;
    if (!assistant.content) {
      return null;
    }
    return {
      id: `history:assistant:${index}`,
      kind: "assistant",
      agentName: rootName,
      content: assistant.content,
      usage: assistant.usage,
      status: assistant.aborted ? "aborted" : undefined,
    };
  }
  if ("Tool" in message) {
    return toolMessageToEntry(
      message.Tool,
      `history:tool:${index}`,
      describeTool(message.Tool.name, argsById[message.Tool.id])
    );
  }
  return null;
}

function toolMessageToEntry(
  message: ToolMessage,
  id = newId("tool-result"),
  detail?: string
): TranscriptEntry {
  return {
    id,
    kind: "tool_result",
    callId: message.id,
    title: message.name,
    detail,
    content: outputText(message.output),
    status: outcomeText(message.outcome),
  };
}

function finishToolEntry(
  session: OpenedSession,
  event: Extract<WireEvent, { type: "tool_end" }>
): OpenedSession {
  const index = session.entries.findIndex(
    (entry) => entry.callId === event.message.id
  );
  if (index < 0) {
    return {
      ...session,
      entries: [...session.entries, toolMessageToEntry(event.message)],
    };
  }
  const entries = [...session.entries];
  // Carry over the detail derived from the call arguments at tool_start; the
  // tool_end message itself doesn't include them.
  entries[index] = {
    ...toolMessageToEntry(
      event.message,
      entries[index].id,
      entries[index].detail
    ),
    agentName: event.agent_name,
    threadId: event.thread_id,
  };
  return { ...session, entries };
}

function addOrUpdateAssistantChunk(
  session: OpenedSession,
  event: Extract<WireEvent, { type: "llm_chunk" }>
): OpenedSession {
  const key = liveKey(event.agent_name, event.thread_id);
  const index = session.entries.findIndex((entry) => entry.liveKey === key);
  if (index >= 0) {
    const entries = [...session.entries];
    entries[index] = {
      ...entries[index],
      content: entries[index].content + event.content,
    };
    return { ...session, entries };
  }
  // Providers often emit a leading empty content chunk before a pure tool-call
  // turn; don't seed an empty assistant bubble for it.
  if (!event.content) {
    return session;
  }
  return {
    ...session,
    entries: [
      ...session.entries,
      {
        id: newId("assistant"),
        kind: "assistant",
        agentName: event.agent_name,
        threadId: event.thread_id,
        content: event.content,
        liveKey: key,
      },
    ],
  };
}

function finishLiveEntry(
  session: OpenedSession,
  agentName: string,
  threadId: string,
  updates: Partial<TranscriptEntry> = {}
): OpenedSession {
  const key = liveKey(agentName, threadId);
  const index = session.entries.findIndex((entry) => entry.liveKey === key);
  if (index < 0) {
    return session;
  }
  const entries = [...session.entries];
  entries[index] = {
    ...entries[index],
    ...updates,
    liveKey: undefined,
  };
  return { ...session, entries };
}

function finishAssistant(
  session: OpenedSession,
  event: Extract<WireEvent, { type: "llm_end" }>
): OpenedSession {
  const key = liveKey(event.agent_name, event.thread_id);
  if (session.entries.some((entry) => entry.liveKey === key)) {
    return finishLiveEntry(session, event.agent_name, event.thread_id, {
      usage: event.message.usage,
      status: event.message.aborted ? "aborted" : undefined,
    });
  }
  if (event.message.content) {
    return {
      ...session,
      entries: [
        ...session.entries,
        {
          id: newId("assistant"),
          kind: "assistant",
          agentName: event.agent_name,
          threadId: event.thread_id,
          content: event.message.content,
          usage: event.message.usage,
          status: event.message.aborted ? "aborted" : undefined,
        },
      ],
    };
  }
  return session;
}

function upsertApproval(
  approvals: PendingApproval[],
  approval: PendingApproval
) {
  const key = approvalKey(approval);
  const index = approvals.findIndex((item) => approvalKey(item) === key);
  if (index >= 0) {
    const next = [...approvals];
    next[index] = approval;
    return next;
  }
  return [...approvals, approval];
}

function reduceEvent(session: OpenedSession, event: WireEvent): OpenedSession {
  switch (event.type) {
    case "llm_start":
      return {
        ...addActivity(session, {
          tone: event.agent_name === rootName ? "neutral" : "cyan",
          label: `${event.agent_name} started`,
          detail: event.model,
        }),
        running: true,
      };
    case "llm_chunk":
      return addOrUpdateAssistantChunk(session, event);
    case "llm_end": {
      // The turn is finished only when the root agent stops without requesting
      // more tools; otherwise more work (tools / sub-agents) is still pending.
      const turnComplete =
        event.agent_name === rootName && event.message.tool_calls.length === 0;
      return {
        ...addActivity(finishAssistant(session, event), {
          tone: event.message.aborted ? "warning" : "success",
          label: `${event.agent_name} finished`,
          detail: event.message.usage
            ? `${
                event.message.usage.prompt_tokens +
                event.message.usage.completion_tokens
              } tokens`
            : "turn complete",
        }),
        running: turnComplete ? false : session.running,
      };
    }
    case "tool_start":
      return {
        ...addActivity(session, {
          tone: event.agent_name === rootName ? "warning" : "cyan",
          label: `${event.agent_name} tool`,
          detail: event.call.name,
        }),
        running: true,
        entries: [
          ...session.entries,
          {
            id: newId("tool-call"),
            kind: "tool_call",
            agentName: event.agent_name,
            threadId: event.thread_id,
            callId: event.call.id,
            title: event.call.name,
            detail: describeTool(event.call.name, event.call.arguments),
            content: callArguments(event.call),
            status: "running",
          },
        ],
      };
    case "tool_end":
      return {
        ...addActivity(finishToolEntry(session, event), {
          tone: "success",
          label: "tool finished",
          detail: event.message.name,
        }),
      };
    case "suspended":
      return {
        ...addActivity(session, {
          tone: "warning",
          label: "approval required",
          detail: `${event.approval.calls.length} call(s) from ${event.agent_name}`,
        }),
        approvals: upsertApproval(session.approvals, event.approval),
        running: false,
      };
    case "aborted": {
      const updated = addActivity(
        finishLiveEntry(session, event.agent_name, event.thread_id),
        {
          tone: "warning",
          label: `${event.agent_name} aborted`,
          detail: event.target.reason,
        }
      );
      return {
        ...updated,
        entries: [
          ...updated.entries,
          {
            id: newId("aborted"),
            kind: "system",
            agentName: event.agent_name,
            threadId: event.thread_id,
            content:
              event.target.reason === "generation"
                ? "Generation interrupted"
                : "Tool calls interrupted",
          },
        ],
        running: false,
      };
    }
    case "error": {
      const updated = addActivity(
        finishLiveEntry(session, event.agent_name, event.thread_id),
        {
          tone: "danger",
          label: `${event.agent_name || "server"} error`,
          detail: event.message,
        }
      );
      return {
        ...updated,
        entries: [
          ...updated.entries,
          {
            id: newId("error"),
            kind: "error",
            agentName: event.agent_name,
            threadId: event.thread_id,
            content: event.message,
          },
        ],
        running: false,
      };
    }
  }
}

function upsertCatalogSession(
  catalog: WorkspaceSummary[],
  workspaceId: string,
  sessionId: string
) {
  return catalog.map((workspace) => {
    if (
      workspace.id !== workspaceId ||
      workspace.sessions.some((session) => session.id === sessionId)
    ) {
      return workspace;
    }
    return {
      ...workspace,
      sessions: [{ id: sessionId, updated_at_ms: null }, ...workspace.sessions],
    };
  });
}

/** Insert (or title) a session in the catalog so the list shows its name right away. */
function upsertCatalogTitled(
  catalog: WorkspaceSummary[],
  workspaceId: string,
  sessionId: string,
  title: string
): WorkspaceSummary[] {
  return catalog.map((workspace) => {
    if (workspace.id !== workspaceId) {
      return workspace;
    }
    const index = workspace.sessions.findIndex(
      (session) => session.id === sessionId
    );
    if (index >= 0) {
      const sessions = [...workspace.sessions];
      const session = sessions[index];
      sessions[index] = {
        ...session,
        updated_at_ms: Date.now(),
        first_user_message: session.first_user_message ?? title,
      };
      return { ...workspace, sessions };
    }
    return {
      ...workspace,
      sessions: [
        { id: sessionId, updated_at_ms: Date.now(), first_user_message: title },
        ...workspace.sessions,
      ],
    };
  });
}

/**
 * Reconcile a server-sent catalog with locally-known sessions: keep titles the
 * server hasn't persisted yet, and keep just-sent sessions the server hasn't
 * listed yet, so a freshly created session doesn't flicker out of the list.
 */
function mergeCatalog(
  incoming: WorkspaceSummary[],
  sessions: Record<SessionKey, OpenedSession>
): WorkspaceSummary[] {
  return incoming.map((workspace) => {
    const present = new Set(workspace.sessions.map((session) => session.id));
    const filled = workspace.sessions.map((session) => {
      if (session.first_user_message) {
        return session;
      }
      const local = sessions[sessionKey(workspace.id, session.id)];
      return local?.firstUserMessage
        ? { ...session, first_user_message: local.firstUserMessage }
        : session;
    });
    const extras = Object.values(sessions)
      .filter(
        (session) =>
          !session.draft &&
          session.workspaceId === workspace.id &&
          Boolean(session.firstUserMessage) &&
          !present.has(session.sessionId)
      )
      .map((session) => ({
        id: session.sessionId,
        updated_at_ms: Date.now(),
        first_user_message: session.firstUserMessage ?? null,
      }));
    return { ...workspace, sessions: [...extras, ...filled] };
  });
}

type CodaStore = Store<CodaStoreState>;
type CodaDraft = Draft<CodaStoreState>;

function updateState(
  store: CodaStore,
  updater: (state: CodaDraft) => void
) {
  store.setState(updater);
}

function currentSocket(store: CodaStore, server: string) {
  return store.getState().wsMap[server];
}

function setSocket(store: CodaStore, server: string, socket: WebSocket) {
  updateState(store, (state) => {
    state.wsMap[server] = socket;
  });
}

function closeSocket(store: CodaStore, server: string) {
  currentSocket(store, server)?.close();
}

function removeSocket(store: CodaStore, server: string) {
  updateState(store, (state) => {
    delete state.wsMap[server];
  });
}

function markAutoConnected(store: CodaStore) {
  updateState(store, (state) => {
    state.autoConnected = true;
  });
}

function draftSession(
  state: CodaDraft,
  server: string,
  key: SessionKey
) {
  const current = state.servers[server];
  if (!current) {
    return undefined;
  }
  const { workspaceId, sessionId } = splitKey(key);
  current.sessions[key] ??= blankSession(workspaceId, sessionId);
  return current.sessions[key];
}

function markConnecting(store: CodaStore, server: string, alias?: string) {
  updateState(store, (state) => {
    const existing = state.servers[server];
    if (!existing) {
      state.order.push(server);
      state.servers[server] = blankServer(server);
    }
    state.servers[server].alias = alias ?? existing?.alias;
    state.servers[server].status = "connecting";
    state.servers[server].error = undefined;
  });
}

function setServerAlias(store: CodaStore, server: string, alias?: string) {
  updateState(store, (state) => {
    const current = state.servers[server];
    if (current) {
      current.alias = alias;
    }
  });
}

function setServerStatus(
  store: CodaStore,
  server: string,
  status: ConnectionStatus,
  error?: string
) {
  updateState(store, (state) => {
    const current = state.servers[server];
    if (current) {
      current.status = status;
      current.error = status === "error" ? error : undefined;
    }
  });
}

function removeServerState(store: CodaStore, server: string) {
  updateState(store, (state) => {
    if (!state.servers[server]) {
      return;
    }
    const clearingActive = state.activeServer === server;
    delete state.servers[server];
    state.order = state.order.filter((url) => url !== server);
    if (clearingActive) {
      state.activeServer = undefined;
      state.activeKey = undefined;
    }
  });
}

function setCatalog(
  store: CodaStore,
  server: string,
  workspaces: WorkspaceSummary[]
) {
  updateState(store, (state) => {
    const current = state.servers[server];
    if (current) {
      current.catalog = mergeCatalog(workspaces, current.sessions);
    }
  });
}

function createDraftSession(
  store: CodaStore,
  server: string,
  workspaceId: string,
  sessionId: string
) {
  const key = sessionKey(workspaceId, sessionId);
  updateState(store, (state) => {
    const current = state.servers[server];
    if (!current) {
      return;
    }
    state.activeServer = server;
    state.activeKey = key;
    for (const [existingKey, session] of Object.entries(current.sessions) as [
      SessionKey,
      OpenedSession,
    ][]) {
      if (
        existingKey !== key &&
        session.draft &&
        session.workspaceId === workspaceId &&
        session.entries.length === 0
      ) {
        delete current.sessions[existingKey];
      }
    }
    current.sessions[key] = {
      ...blankSession(workspaceId, sessionId),
      draft: true,
    };
  });
}

function deleteSessionState(store: CodaStore, server: string, key: SessionKey) {
  updateState(store, (state) => {
    const current = state.servers[server];
    if (!current) {
      return;
    }
    const { workspaceId, sessionId } = splitKey(key);
    delete current.sessions[key];
    for (const workspace of current.catalog) {
      if (workspace.id === workspaceId) {
        workspace.sessions = workspace.sessions.filter(
          (session) => session.id !== sessionId
        );
      }
    }
    const clearingActive = state.activeServer === server && state.activeKey === key;
    if (clearingActive) {
      state.activeKey = undefined;
    }
  });
}

function selectSession(
  store: CodaStore,
  server: string,
  workspaceId: string,
  sessionId: string
) {
  const key = sessionKey(workspaceId, sessionId);
  updateState(store, (state) => {
    if (state.servers[server]) {
      state.activeServer = server;
      state.activeKey = key;
      draftSession(state, server, key);
    }
  });
}

function applySnapshot(
  store: CodaStore,
  server: string,
  workspaceId: string,
  sessionId: string,
  messages: HistoryMessage[],
  approvals: PendingApproval[]
) {
  const key = sessionKey(workspaceId, sessionId);
  const argsById = collectToolArgs(messages);
  const mapped = messages
    .map((message, index) => historyToEntry(message, index, argsById))
    .filter((entry): entry is TranscriptEntry => Boolean(entry));
  const hasHistory = messages.length > 0;
  updateState(store, (state) => {
    const current = state.servers[server];
    if (!current) {
      return;
    }
    current.status = "connected";
    current.catalog = upsertCatalogSession(
      current.catalog,
      workspaceId,
      sessionId
    );
    const session = draftSession(state, server, key);
    if (!session) {
      return;
    }
    session.draft = false;
    if (hasHistory) {
      session.entries = mapped;
      session.approvals = approvals;
      session.drafts = {};
      session.running = false;
    }
  });
}

function applyEvent(
  store: CodaStore,
  server: string,
  workspaceId: string,
  sessionId: string,
  event: WireEvent
) {
  const key = sessionKey(workspaceId, sessionId);
  updateState(store, (state) => {
    const session = draftSession(state, server, key);
    if (session) {
      state.servers[server].sessions[key] = reduceEvent(session, event);
    }
  });
}

function addAllowResultActivity(
  store: CodaStore,
  server: string,
  workspaceId: string,
  pattern: string,
  error?: string | null
) {
  updateState(store, (state) => {
    if (state.activeServer !== server || !state.activeKey) {
      return;
    }
    if (splitKey(state.activeKey).workspaceId !== workspaceId) {
      return;
    }
    const session = draftSession(state, server, state.activeKey);
    if (session) {
      state.servers[server].sessions[state.activeKey] = addActivity(session, {
        tone: error ? "danger" : "success",
        label: error ? "allow pattern failed" : "allow pattern saved",
        detail: error || pattern,
      });
    }
  });
}

function appendUserMessage(
  store: CodaStore,
  server: string,
  key: SessionKey,
  content: string
) {
  updateState(store, (state) => {
    const current = state.servers[server];
    const session = draftSession(state, server, key);
    if (!current || !session) {
      return;
    }
    const { workspaceId, sessionId } = splitKey(key);
    const firstUserMessage = session.firstUserMessage ?? content;
    session.draft = false;
    session.running = true;
    session.firstUserMessage = firstUserMessage;
    session.entries.push({ id: newId("user"), kind: "user", content });
    current.catalog = upsertCatalogTitled(
      current.catalog,
      workspaceId,
      sessionId,
      firstUserMessage
    );
  });
}

function setDraftResolution(
  store: CodaStore,
  server: string,
  key: SessionKey,
  approval: PendingApproval,
  call: ToolCall,
  resolution: ToolCallResolution
) {
  updateState(store, (state) => {
    const session = draftSession(state, server, key);
    if (!session) {
      return;
    }
    const approvalId = approvalKey(approval);
    session.drafts[approvalId] ??= {};
    session.drafts[approvalId][call.id] = resolution;
  });
}

function clearApprovalState(
  store: CodaStore,
  server: string,
  key: SessionKey,
  approval: PendingApproval
) {
  updateState(store, (state) => {
    const session = draftSession(state, server, key);
    if (!session) {
      return;
    }
    const approvalId = approvalKey(approval);
    delete session.drafts[approvalId];
    session.approvals = session.approvals.filter(
      (item) => approvalKey(item) !== approvalId
    );
  });
}

function normalizeWsUrl(input: string) {
  const trimmed = input.trim();
  if (trimmed) {
    const base = trimmed.replace(/\/$/, "");
    const wsBase = base.startsWith("http://")
      ? base.replace(/^http:\/\//, "ws://")
      : base.startsWith("https://")
      ? base.replace(/^https:\/\//, "wss://")
      : base;
    // Don't double-append when the user already pasted the `/ws` endpoint.
    return wsBase.endsWith("/ws") ? wsBase : `${wsBase}/ws`;
  }
  const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${protocol}//${window.location.host}/ws`;
}

function encode(message: ClientMessage) {
  return JSON.stringify(message);
}

export function useCodaSession() {
  const store = useImmutableRef(() =>
    create<CodaStoreState>(initialStoreState)
  );
  const state = useStore(store);

  useEffect(
    () => () => {
      for (const socket of Object.values(store.getState().wsMap)) {
        socket.close();
      }
    },
    [store]
  );

  const send = useCallback(
    (server: string, message: ClientMessage) => {
      const socket = currentSocket(store, server);
      if (socket?.readyState === WebSocket.OPEN) {
        socket.send(encode(message));
        return true;
      }
      setServerStatus(store, server, "error", "Connection closed");
      return false;
    },
    [store]
  );

  const connectServer = useCallback(
    (rawUrl: string) => {
      const server = rawUrl.trim();
      if (!server) {
        return;
      }
      closeSocket(store, server);
      const stored = loadStoredServers();
      storeServers(addStored(stored, server));
      markConnecting(
        store,
        server,
        stored.find((entry) => entry.url === server)?.alias
      );

      const socket = new WebSocket(normalizeWsUrl(server));
      setSocket(store, server, socket);

      socket.onopen = () => {
        setServerStatus(store, server, "connected");
        socket.send(encode({ type: "list_workspaces" }));
      };
      socket.onclose = () => {
        if (currentSocket(store, server) === socket) {
          setServerStatus(store, server, "closed");
        }
      };
      socket.onerror = () =>
        setServerStatus(store, server, "error", "WebSocket connection failed");
      socket.onmessage = (event: MessageEvent<string>) => {
        try {
          const message = JSON.parse(event.data) as ServerMessage;
          if (message.type === "workspace_catalog") {
            setCatalog(store, server, message.workspaces);
            return;
          }
          if (message.type === "snapshot") {
            applySnapshot(
              store,
              server,
              message.workspace_id,
              message.session_id,
              message.messages,
              message.pending_approvals ?? []
            );
            return;
          }
          if (message.type === "event") {
            applyEvent(
              store,
              server,
              message.workspace_id,
              message.session_id,
              message.event
            );
            return;
          }
          addAllowResultActivity(
            store,
            server,
            message.workspace_id,
            message.pattern,
            message.error
          );
        } catch (error) {
          setServerStatus(
            store,
            server,
            "error",
            error instanceof Error ? error.message : "Invalid server message"
          );
        }
      };
    },
    [store]
  );

  const removeServer = useCallback(
    (rawUrl: string) => {
      const server = rawUrl.trim();
      if (!server) {
        return;
      }
      closeSocket(store, server);
      removeSocket(store, server);
      storeServers(loadStoredServers().filter((entry) => entry.url !== server));
      removeServerState(store, server);
    },
    [store]
  );

  const disconnectServer = useCallback(
    (rawUrl: string) => {
      const server = rawUrl.trim();
      if (!server) {
        return;
      }
      closeSocket(store, server);
      removeSocket(store, server);
      setServerStatus(store, server, "closed");
    },
    [store]
  );

  const renameServer = useCallback(
    (rawUrl: string, rawAlias: string) => {
      const server = rawUrl.trim();
      if (!server) {
        return;
      }
      const alias = rawAlias.trim() || undefined;
      const stored = loadStoredServers();
      const next = stored.some((entry) => entry.url === server)
        ? stored.map((entry) =>
            entry.url === server ? { ...entry, alias } : entry
          )
        : [...stored, { url: server, alias }];
      storeServers(next);
      setServerAlias(store, server, alias);
    },
    [store]
  );

  useEffect(() => {
    const runtime = store.getState();
    if (runtime.autoConnected) {
      return;
    }
    markAutoConnected(store);
    for (const { url } of loadStoredServers()) {
      connectServer(url);
    }
  }, [connectServer, store]);

  const currentActive = useCallback(() => {
    const snapshot = store.getState();
    const server = snapshot.activeServer;
    const key = snapshot.activeKey;
    if (!server || !key) {
      return undefined;
    }
    const session = snapshot.servers[server]?.sessions[key];
    return session ? { server, session } : undefined;
  }, [store]);

  const newSession = useCallback(
    (server: string, workspaceId: string) => {
      const workspace = workspaceId.trim();
      if (!server || !workspace) {
        return;
      }
      const current = store.getState().servers[server];
      const reusable = current
        ? Object.values(current.sessions).find(
            (session) =>
              session.draft &&
              session.workspaceId === workspace &&
              session.entries.length === 0
          )
        : undefined;
      const sessionId = reusable?.sessionId ?? freshSessionId();
      createDraftSession(store, server, workspace, sessionId);
    },
    [store]
  );

  const deleteSession = useCallback(
    (server: string, workspaceId: string, sessionId: string) => {
      const workspace = workspaceId.trim();
      const session = sessionId.trim();
      if (!server || !workspace || !session) {
        return;
      }
      const key = sessionKey(workspace, session);
      const local = store.getState().servers[server]?.sessions[key];
      if (!local?.draft) {
        send(server, {
          type: "delete_session",
          workspace_id: workspace,
          session_id: session,
        });
      }
      deleteSessionState(store, server, key);
    },
    [send, store]
  );

  const openSession = useCallback(
    (server: string, workspaceId: string, sessionId: string) => {
      const workspace = workspaceId.trim();
      const session = sessionId.trim();
      if (!server || !workspace || !session) {
        return;
      }
      const local =
        store.getState().servers[server]?.sessions[
          sessionKey(workspace, session)
        ];
      selectSession(store, server, workspace, session);
      if (!local?.draft) {
        send(server, {
          type: "open_session",
          workspace_id: workspace,
          session_id: session,
        });
      }
    },
    [send, store]
  );

  const sendTask = useCallback(
    (task: string) => {
      const text = task.trim();
      const active = currentActive();
      if (!text || !active) {
        return;
      }
      if (active.session.draft) {
        send(active.server, {
          type: "open_session",
          workspace_id: active.session.workspaceId,
          session_id: active.session.sessionId,
        });
      }
      if (
        send(active.server, {
          type: "task",
          workspace_id: active.session.workspaceId,
          session_id: active.session.sessionId,
          task: text,
        })
      ) {
        appendUserMessage(store, active.server, active.session.key, text);
      }
    },
    [send, currentActive, store]
  );

  const sendTaskToNewSession = useCallback(
    (server: string, workspaceId: string, task: string) => {
      const workspace = workspaceId.trim();
      const text = task.trim();
      if (!server || !workspace || !text) {
        return;
      }
      const current = store.getState().servers[server];
      const reusable = current
        ? Object.values(current.sessions).find(
            (session) =>
              session.draft &&
              session.workspaceId === workspace &&
              session.entries.length === 0
          )
        : undefined;
      const sessionId = reusable?.sessionId ?? freshSessionId();
      const key = sessionKey(workspace, sessionId);
      createDraftSession(store, server, workspace, sessionId);
      send(server, {
        type: "open_session",
        workspace_id: workspace,
        session_id: sessionId,
      });
      if (
        send(server, {
          type: "task",
          workspace_id: workspace,
          session_id: sessionId,
          task: text,
        })
      ) {
        appendUserMessage(store, server, key, text);
      }
    },
    [send, store]
  );

  const abort = useCallback(() => {
    const active = currentActive();
    if (active) {
      send(active.server, {
        type: "abort",
        workspace_id: active.session.workspaceId,
        session_id: active.session.sessionId,
      });
    }
  }, [send, currentActive]);

  const addAllowPattern = useCallback(
    (pattern: string) => {
      const active = currentActive();
      const value = pattern.trim();
      if (active && value) {
        send(active.server, {
          type: "add_allow_pattern",
          workspace_id: active.session.workspaceId,
          pattern: value,
        });
      }
    },
    [send, currentActive]
  );

  const draftCall = useCallback(
    (
      approval: PendingApproval,
      call: ToolCall,
      resolution: ToolCallResolution
    ) => {
      const active = currentActive();
      if (!active) {
        return;
      }
      setDraftResolution(
        store,
        active.server,
        active.session.key,
        approval,
        call,
        resolution
      );
    },
    [currentActive, store]
  );

  const submitApprovals = useCallback(() => {
    const active = currentActive();
    if (!active) {
      return;
    }
    for (const approval of active.session.approvals) {
      const draft = active.session.drafts[approvalKey(approval)] ?? {};
      const complete = approval.calls.every((item) => draft[item.id]);
      if (!complete) {
        continue;
      }
      send(active.server, {
        type: "resume",
        workspace_id: active.session.workspaceId,
        session_id: active.session.sessionId,
        agent_name: approval.agent_name,
        thread_id: approval.thread_id,
        decision: {
          resolutions: approval.calls.map((item) => [item.id, draft[item.id]]),
        },
      });
      clearApprovalState(store, active.server, active.session.key, approval);
    }
  }, [send, currentActive, store]);

  const activeServerState = state.activeServer
    ? state.servers[state.activeServer]
    : undefined;
  const activeSession =
    activeServerState && state.activeKey
      ? activeServerState.sessions[state.activeKey]
      : undefined;
  const servers = state.order
    .map((url) => state.servers[url])
    .filter((server): server is ServerState => Boolean(server));

  return {
    servers,
    activeServer: state.activeServer,
    activeKey: state.activeKey,
    activeWorkspace: activeSession?.workspaceId,
    activeDraft: activeSession?.draft ?? false,
    status: activeServerState?.status ?? "idle",
    entries: activeSession?.entries ?? [],
    activity: activeSession?.activity ?? [],
    approvals: activeSession?.approvals ?? [],
    drafts: activeSession?.drafts ?? {},
    running: activeSession?.running ?? false,
    connectServer,
    disconnectServer,
    removeServer,
    renameServer,
    newSession,
    openSession,
    deleteSession,
    sendTask,
    sendTaskToNewSession,
    abort,
    addAllowPattern,
    draftCall,
    submitApprovals,
  };
}
