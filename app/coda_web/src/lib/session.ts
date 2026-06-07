import { useCallback, useEffect, useReducer, useRef } from "react";
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

export type { WorkspaceSession, WorkspaceSummary } from "./protocol";

export type ConnectionStatus = "idle" | "connecting" | "connected" | "closed" | "error";

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

type Action =
  | { type: "connecting"; server: string; alias?: string }
  | { type: "set_alias"; server: string; alias?: string }
  | { type: "connected"; server: string }
  | { type: "closed"; server: string }
  | { type: "error"; server: string; error: string }
  | { type: "remove_server"; server: string }
  | { type: "catalog"; server: string; workspaces: WorkspaceSummary[] }
  | { type: "new_session"; server: string; workspaceId: string; sessionId: string }
  | { type: "delete_session"; server: string; key: SessionKey }
  | { type: "select"; server: string; workspaceId: string; sessionId: string }
  | {
      type: "snapshot";
      server: string;
      workspaceId: string;
      sessionId: string;
      messages: HistoryMessage[];
      approvals: PendingApproval[];
    }
  | { type: "event"; server: string; workspaceId: string; sessionId: string; event: WireEvent }
  | { type: "allow_result"; server: string; workspaceId: string; pattern: string; error?: string | null }
  | { type: "append_user"; server: string; key: SessionKey; content: string }
  | {
      type: "draft_resolution";
      server: string;
      key: SessionKey;
      approval: PendingApproval;
      call: ToolCall;
      resolution: ToolCallResolution;
    }
  | { type: "clear_approval"; server: string; key: SessionKey; approval: PendingApproval };

const rootName = "coda";

function newId(prefix: string) {
  return `${prefix}:${Date.now().toString(36)}:${Math.random().toString(36).slice(2)}`;
}

function freshSessionId() {
  return globalThis.crypto?.randomUUID?.() ?? `session-${Date.now().toString(36)}`;
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
            if (value && typeof value === "object" && typeof value.url === "string" && value.url.trim()) {
              const alias = typeof value.alias === "string" && value.alias.trim() ? value.alias.trim() : undefined;
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

function updateServer(state: CodaState, server: string, updater: (server: ServerState) => ServerState): CodaState {
  const current = state.servers[server];
  if (!current) {
    return state;
  }
  return {
    ...state,
    servers: {
      ...state.servers,
      [server]: updater(current),
    },
  };
}

function updateServerSession(
  state: CodaState,
  server: string,
  key: SessionKey,
  updater: (session: OpenedSession) => OpenedSession,
): CodaState {
  return updateServer(state, server, (current) => {
    const { workspaceId, sessionId } = splitKey(key);
    const session = current.sessions[key] ?? blankSession(workspaceId, sessionId);
    return {
      ...current,
      sessions: {
        ...current.sessions,
        [key]: updater(session),
      },
    };
  });
}

function addActivity(session: OpenedSession, entry: Omit<ActivityEntry, "id">): OpenedSession {
  return {
    ...session,
    activity: [{ id: newId("activity"), ...entry }, ...session.activity].slice(0, 80),
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

function finishToolEntry(session: OpenedSession, event: Extract<WireEvent, { type: "tool_end" }>): OpenedSession {
  const index = session.entries.findIndex((entry) => entry.callId === event.message.id);
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
    ...toolMessageToEntry(event.message, entries[index].id, entries[index].detail),
    agentName: event.agent_name,
    threadId: event.thread_id,
  };
  return { ...session, entries };
}

function addOrUpdateAssistantChunk(
  session: OpenedSession,
  event: Extract<WireEvent, { type: "llm_chunk" }>,
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
  updates: Partial<TranscriptEntry> = {},
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

function finishAssistant(session: OpenedSession, event: Extract<WireEvent, { type: "llm_end" }>): OpenedSession {
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

function upsertApproval(approvals: PendingApproval[], approval: PendingApproval) {
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
      const turnComplete = event.agent_name === rootName && event.message.tool_calls.length === 0;
      return {
        ...addActivity(finishAssistant(session, event), {
          tone: event.message.aborted ? "warning" : "success",
          label: `${event.agent_name} finished`,
          detail: event.message.usage
            ? `${event.message.usage.prompt_tokens + event.message.usage.completion_tokens} tokens`
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
      const updated = addActivity(finishLiveEntry(session, event.agent_name, event.thread_id), {
        tone: "warning",
        label: `${event.agent_name} aborted`,
        detail: event.target.reason,
      });
      return {
        ...updated,
        entries: [
          ...updated.entries,
          {
            id: newId("aborted"),
            kind: "system",
            agentName: event.agent_name,
            threadId: event.thread_id,
            content: event.target.reason === "generation" ? "Generation interrupted" : "Tool calls interrupted",
          },
        ],
        running: false,
      };
    }
    case "error": {
      const updated = addActivity(finishLiveEntry(session, event.agent_name, event.thread_id), {
        tone: "danger",
        label: `${event.agent_name || "server"} error`,
        detail: event.message,
      });
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

function upsertCatalogSession(catalog: WorkspaceSummary[], workspaceId: string, sessionId: string) {
  return catalog.map((workspace) => {
    if (workspace.id !== workspaceId || workspace.sessions.some((session) => session.id === sessionId)) {
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
  title: string,
): WorkspaceSummary[] {
  return catalog.map((workspace) => {
    if (workspace.id !== workspaceId) {
      return workspace;
    }
    const index = workspace.sessions.findIndex((session) => session.id === sessionId);
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
      sessions: [{ id: sessionId, updated_at_ms: Date.now(), first_user_message: title }, ...workspace.sessions],
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
  sessions: Record<SessionKey, OpenedSession>,
): WorkspaceSummary[] {
  return incoming.map((workspace) => {
    const present = new Set(workspace.sessions.map((session) => session.id));
    const filled = workspace.sessions.map((session) => {
      if (session.first_user_message) {
        return session;
      }
      const local = sessions[sessionKey(workspace.id, session.id)];
      return local?.firstUserMessage ? { ...session, first_user_message: local.firstUserMessage } : session;
    });
    const extras = Object.values(sessions)
      .filter(
        (session) =>
          !session.draft &&
          session.workspaceId === workspace.id &&
          Boolean(session.firstUserMessage) &&
          !present.has(session.sessionId),
      )
      .map((session) => ({
        id: session.sessionId,
        updated_at_ms: Date.now(),
        first_user_message: session.firstUserMessage ?? null,
      }));
    return { ...workspace, sessions: [...extras, ...filled] };
  });
}

function reducer(state: CodaState, action: Action): CodaState {
  switch (action.type) {
    case "connecting": {
      const existing = state.servers[action.server];
      return {
        ...state,
        order: existing ? state.order : [...state.order, action.server],
        servers: {
          ...state.servers,
          [action.server]: {
            ...(existing ?? blankServer(action.server)),
            alias: action.alias ?? existing?.alias,
            status: "connecting",
            error: undefined,
          },
        },
      };
    }
    case "set_alias":
      return updateServer(state, action.server, (server) => ({ ...server, alias: action.alias }));
    case "connected":
      return updateServer(state, action.server, (server) => ({ ...server, status: "connected", error: undefined }));
    case "closed":
      return updateServer(state, action.server, (server) => ({ ...server, status: "closed" }));
    case "error":
      return updateServer(state, action.server, (server) => ({ ...server, status: "error", error: action.error }));
    case "remove_server": {
      if (!state.servers[action.server]) {
        return state;
      }
      const { [action.server]: _removed, ...servers } = state.servers;
      const clearingActive = state.activeServer === action.server;
      return {
        ...state,
        servers,
        order: state.order.filter((url) => url !== action.server),
        activeServer: clearingActive ? undefined : state.activeServer,
        activeKey: clearingActive ? undefined : state.activeKey,
      };
    }
    case "catalog":
      return updateServer(state, action.server, (server) => ({
        ...server,
        catalog: mergeCatalog(action.workspaces, server.sessions),
      }));
    case "new_session": {
      const key = sessionKey(action.workspaceId, action.sessionId);
      return updateServer(
        { ...state, activeServer: action.server, activeKey: key },
        action.server,
        (server) => {
          // Drop other empty drafts in this workspace so repeated "+" clicks
          // don't stack blank sessions.
          const sessions: Record<SessionKey, OpenedSession> = {};
          for (const [existingKey, session] of Object.entries(server.sessions) as [SessionKey, OpenedSession][]) {
            if (
              existingKey !== key &&
              session.draft &&
              session.workspaceId === action.workspaceId &&
              session.entries.length === 0
            ) {
              continue;
            }
            sessions[existingKey] = session;
          }
          sessions[key] = { ...blankSession(action.workspaceId, action.sessionId), draft: true };
          return { ...server, sessions };
        },
      );
    }
    case "delete_session": {
      const { workspaceId, sessionId } = splitKey(action.key);
      const next = updateServer(state, action.server, (server) => {
        const { [action.key]: _removed, ...sessions } = server.sessions;
        return {
          ...server,
          sessions,
          catalog: server.catalog.map((workspace) =>
            workspace.id === workspaceId
              ? { ...workspace, sessions: workspace.sessions.filter((session) => session.id !== sessionId) }
              : workspace,
          ),
        };
      });
      const clearingActive = state.activeServer === action.server && state.activeKey === action.key;
      return clearingActive ? { ...next, activeKey: undefined } : next;
    }
    case "select": {
      const key = sessionKey(action.workspaceId, action.sessionId);
      return updateServerSession({ ...state, activeServer: action.server, activeKey: key }, action.server, key, (session) => session);
    }
    case "snapshot": {
      const key = sessionKey(action.workspaceId, action.sessionId);
      const argsById = collectToolArgs(action.messages);
      const mapped = action.messages
        .map((message, index) => historyToEntry(message, index, argsById))
        .filter((entry): entry is TranscriptEntry => Boolean(entry));
      // A brand-new session opened on first send returns an empty history; don't
      // wipe the locally-appended user message / running state in that case.
      const hasHistory = action.messages.length > 0;
      const withCatalog = updateServer(state, action.server, (server) => ({
        ...server,
        status: "connected",
        catalog: upsertCatalogSession(server.catalog, action.workspaceId, action.sessionId),
      }));
      return updateServerSession(withCatalog, action.server, key, (session) => ({
        ...session,
        draft: false,
        entries: hasHistory ? mapped : session.entries,
        approvals: hasHistory ? action.approvals : session.approvals,
        drafts: hasHistory ? {} : session.drafts,
        running: hasHistory ? false : session.running,
      }));
    }
    case "event": {
      const key = sessionKey(action.workspaceId, action.sessionId);
      return updateServerSession(state, action.server, key, (session) => reduceEvent(session, action.event));
    }
    case "allow_result": {
      const server = state.activeServer;
      const key = state.activeKey;
      if (!server || server !== action.server || !key) {
        return state;
      }
      if (splitKey(key).workspaceId !== action.workspaceId) {
        return state;
      }
      return updateServerSession(state, server, key, (session) =>
        addActivity(session, {
          tone: action.error ? "danger" : "success",
          label: action.error ? "allow pattern failed" : "allow pattern saved",
          detail: action.error || action.pattern,
        }),
      );
    }
    case "append_user":
      return updateServer(state, action.server, (server) => {
        const { workspaceId, sessionId } = splitKey(action.key);
        const previous = server.sessions[action.key] ?? blankSession(workspaceId, sessionId);
        const firstUserMessage = previous.firstUserMessage ?? action.content;
        const session: OpenedSession = {
          ...previous,
          draft: false,
          running: true,
          firstUserMessage,
          entries: [
            ...previous.entries,
            {
              id: newId("user"),
              kind: "user",
              content: action.content,
            },
          ],
        };
        return {
          ...server,
          sessions: { ...server.sessions, [action.key]: session },
          catalog: upsertCatalogTitled(server.catalog, workspaceId, sessionId, firstUserMessage),
        };
      });
    case "draft_resolution":
      return updateServerSession(state, action.server, action.key, (session) => {
        const key = approvalKey(action.approval);
        const current = session.drafts[key] ?? {};
        return {
          ...session,
          drafts: {
            ...session.drafts,
            [key]: {
              ...current,
              [action.call.id]: action.resolution,
            },
          },
        };
      });
    case "clear_approval":
      return updateServerSession(state, action.server, action.key, (session) => {
        const key = approvalKey(action.approval);
        const { [key]: _removed, ...drafts } = session.drafts;
        return {
          ...session,
          drafts,
          approvals: session.approvals.filter((approval) => approvalKey(approval) !== key),
        };
      });
  }
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
  const [state, dispatch] = useReducer(reducer, initialState);
  const wsMapRef = useRef<Record<string, WebSocket>>({});
  const stateRef = useRef(state);

  useEffect(() => {
    stateRef.current = state;
  }, [state]);

  useEffect(
    () => () => {
      for (const socket of Object.values(wsMapRef.current)) {
        socket.close();
      }
    },
    [],
  );

  const send = useCallback((server: string, message: ClientMessage) => {
    const socket = wsMapRef.current[server];
    if (socket?.readyState === WebSocket.OPEN) {
      socket.send(encode(message));
      return true;
    }
    dispatch({ type: "error", server, error: "Connection closed" });
    return false;
  }, []);

  const connectServer = useCallback((rawUrl: string) => {
    const server = rawUrl.trim();
    if (!server) {
      return;
    }
    wsMapRef.current[server]?.close();
    const stored = loadStoredServers();
    storeServers(addStored(stored, server));
    dispatch({ type: "connecting", server, alias: stored.find((entry) => entry.url === server)?.alias });

    const socket = new WebSocket(normalizeWsUrl(server));
    wsMapRef.current[server] = socket;

    socket.onopen = () => {
      dispatch({ type: "connected", server });
      socket.send(encode({ type: "list_workspaces" }));
    };
    socket.onclose = () => {
      if (wsMapRef.current[server] === socket) {
        dispatch({ type: "closed", server });
      }
    };
    socket.onerror = () => dispatch({ type: "error", server, error: "WebSocket connection failed" });
    socket.onmessage = (event: MessageEvent<string>) => {
      try {
        const message = JSON.parse(event.data) as ServerMessage;
        if (message.type === "workspace_catalog") {
          dispatch({ type: "catalog", server, workspaces: message.workspaces });
          return;
        }
        if (message.type === "snapshot") {
          dispatch({
            type: "snapshot",
            server,
            workspaceId: message.workspace_id,
            sessionId: message.session_id,
            messages: message.messages,
            approvals: message.pending_approvals ?? [],
          });
          return;
        }
        if (message.type === "event") {
          dispatch({
            type: "event",
            server,
            workspaceId: message.workspace_id,
            sessionId: message.session_id,
            event: message.event,
          });
          return;
        }
        dispatch({
          type: "allow_result",
          server,
          workspaceId: message.workspace_id,
          pattern: message.pattern,
          error: message.error,
        });
      } catch (error) {
        dispatch({
          type: "error",
          server,
          error: error instanceof Error ? error.message : "Invalid server message",
        });
      }
    };
  }, []);

  const removeServer = useCallback((rawUrl: string) => {
    const server = rawUrl.trim();
    if (!server) {
      return;
    }
    wsMapRef.current[server]?.close();
    delete wsMapRef.current[server];
    storeServers(loadStoredServers().filter((entry) => entry.url !== server));
    dispatch({ type: "remove_server", server });
  }, []);

  const disconnectServer = useCallback((rawUrl: string) => {
    const server = rawUrl.trim();
    if (!server) {
      return;
    }
    wsMapRef.current[server]?.close();
    delete wsMapRef.current[server];
    dispatch({ type: "closed", server });
  }, []);

  const renameServer = useCallback((rawUrl: string, rawAlias: string) => {
    const server = rawUrl.trim();
    if (!server) {
      return;
    }
    const alias = rawAlias.trim() || undefined;
    const stored = loadStoredServers();
    const next = stored.some((entry) => entry.url === server)
      ? stored.map((entry) => (entry.url === server ? { ...entry, alias } : entry))
      : [...stored, { url: server, alias }];
    storeServers(next);
    dispatch({ type: "set_alias", server, alias });
  }, []);

  // Auto-connect once on load to every server the user has added.
  const autoConnected = useRef(false);
  useEffect(() => {
    if (autoConnected.current) {
      return;
    }
    autoConnected.current = true;
    for (const { url } of loadStoredServers()) {
      connectServer(url);
    }
  }, [connectServer]);

  const currentActive = useCallback(() => {
    const snapshot = stateRef.current;
    const server = snapshot.activeServer;
    const key = snapshot.activeKey;
    if (!server || !key) {
      return undefined;
    }
    const session = snapshot.servers[server]?.sessions[key];
    return session ? { server, session } : undefined;
  }, []);

  const newSession = useCallback((server: string, workspaceId: string) => {
    const workspace = workspaceId.trim();
    if (!server || !workspace) {
      return;
    }
    const current = stateRef.current.servers[server];
    const reusable = current
      ? Object.values(current.sessions).find(
          (session) => session.draft && session.workspaceId === workspace && session.entries.length === 0,
        )
      : undefined;
    const sessionId = reusable?.sessionId ?? freshSessionId();
    dispatch({ type: "new_session", server, workspaceId: workspace, sessionId });
  }, []);

  const deleteSession = useCallback(
    (server: string, workspaceId: string, sessionId: string) => {
      const workspace = workspaceId.trim();
      const session = sessionId.trim();
      if (!server || !workspace || !session) {
        return;
      }
      const key = sessionKey(workspace, session);
      const local = stateRef.current.servers[server]?.sessions[key];
      // Drafts only exist locally; nothing to delete on the server.
      if (!local?.draft) {
        send(server, { type: "delete_session", workspace_id: workspace, session_id: session });
      }
      dispatch({ type: "delete_session", server, key });
    },
    [send],
  );

  const openSession = useCallback(
    (server: string, workspaceId: string, sessionId: string) => {
      const workspace = workspaceId.trim();
      const session = sessionId.trim();
      if (!server || !workspace || !session) {
        return;
      }
      const local = stateRef.current.servers[server]?.sessions[sessionKey(workspace, session)];
      dispatch({ type: "select", server, workspaceId: workspace, sessionId: session });
      // Drafts have not been created server-side yet; opening happens lazily on
      // the first task so repeated clicks don't spawn blank sessions.
      if (!local?.draft) {
        send(server, { type: "open_session", workspace_id: workspace, session_id: session });
      }
    },
    [send],
  );

  const sendTask = useCallback(
    (task: string) => {
      const text = task.trim();
      const active = currentActive();
      if (!text || !active) {
        return;
      }
      // A draft session is created on the server lazily, right before its first task.
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
        dispatch({ type: "append_user", server: active.server, key: active.session.key, content: text });
      }
    },
    [send, currentActive],
  );

  const sendTaskToNewSession = useCallback(
    (server: string, workspaceId: string, task: string) => {
      const workspace = workspaceId.trim();
      const text = task.trim();
      if (!server || !workspace || !text) {
        return;
      }
      const current = stateRef.current.servers[server];
      const reusable = current
        ? Object.values(current.sessions).find(
            (session) => session.draft && session.workspaceId === workspace && session.entries.length === 0,
          )
        : undefined;
      const sessionId = reusable?.sessionId ?? freshSessionId();
      const key = sessionKey(workspace, sessionId);
      dispatch({ type: "new_session", server, workspaceId: workspace, sessionId });
      send(server, { type: "open_session", workspace_id: workspace, session_id: sessionId });
      if (send(server, { type: "task", workspace_id: workspace, session_id: sessionId, task: text })) {
        dispatch({ type: "append_user", server, key, content: text });
      }
    },
    [send],
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
        send(active.server, { type: "add_allow_pattern", workspace_id: active.session.workspaceId, pattern: value });
      }
    },
    [send, currentActive],
  );

  const draftCall = useCallback(
    (approval: PendingApproval, call: ToolCall, resolution: ToolCallResolution) => {
      const active = currentActive();
      if (!active) {
        return;
      }
      dispatch({ type: "draft_resolution", server: active.server, key: active.session.key, approval, call, resolution });
    },
    [currentActive],
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
      dispatch({ type: "clear_approval", server: active.server, key: active.session.key, approval });
    }
  }, [send, currentActive]);

  const activeServerState = state.activeServer ? state.servers[state.activeServer] : undefined;
  const activeSession = activeServerState && state.activeKey ? activeServerState.sessions[state.activeKey] : undefined;
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
