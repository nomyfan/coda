import { useEffect } from "react";
import { useStore } from "zustand";
import type { Draft } from "immer";
import {
  approvalKey,
  type ClientMessage,
  type CompletionUsage,
  type HistoryMessage,
  type PendingApproval,
  type ProviderInfo,
  type ReasoningEffort,
  type ServerMessage,
  type ToolCall,
  type ToolCallResolution,
  type ToolMessage,
  type WireEvent,
  type WorkspaceSummary,
  callArguments,
  describeTool,
  extractShellCommand,
  outcomeText,
  outputText,
  subAgentDisplayName,
} from "@/lib/protocol";
import { useShallow } from "zustand/react/shallow";
import { create, type Store } from "@/store/utils";

export type {
  CompletionUsage,
  ProviderInfo,
  ReasoningEffort,
  WorkspaceSession,
  WorkspaceSummary,
} from "@/lib/protocol";

export type ConnectionStatus = "idle" | "connecting" | "connected" | "closed" | "error";

export type TranscriptEntry = {
  id: string;
  kind: "user" | "assistant" | "reasoning" | "tool_call" | "tool_result" | "system" | "error";
  agentName?: string;
  threadId?: string;
  title?: string;
  /** Short summary of what a tool acts on (file basename, shell command, …). */
  detail?: string;
  /** Executed shell command, shown alongside shell results. */
  command?: string;
  content: string;
  /** Image URLs attached to a user message (base64 data-URIs or HTTPS URLs). */
  images?: string[];
  status?: string;
  liveKey?: string;
  callId?: string;
  isFinalResponse?: boolean;
  /** RFC 3339 timestamps for display: message time and elapsed duration. */
  startedAt?: string | null;
  endedAt?: string | null;
};

export type ActivityEntry = {
  id: string;
  tone: "neutral" | "success" | "warning" | "danger" | "cyan";
  label: string;
  detail: string;
};

export type SessionKey = `${string}/${string}`;

export type UsageRecord = {
  agentName: string;
  usage: CompletionUsage;
};

export type OpenedSession = {
  key: SessionKey;
  workspaceId: string;
  sessionId: string;
  entries: TranscriptEntry[];
  activity: ActivityEntry[];
  approvals: PendingApproval[];
  drafts: Record<string, Record<string, ToolCallResolution>>;
  /** Per-call "always allow" patterns staged for an approval; sent to the
   * server only on submit, so the intent stays cancelable until then. */
  allowDrafts: Record<string, Record<string, string>>;
  running: boolean;
  /** Created locally via "new session" but not yet opened on the server. */
  draft?: boolean;
  /** First user task, used as the session list title before the server persists it. */
  firstUserMessage?: string;
  /** Provider this session currently uses; set from the server snapshot. */
  providerId?: string;
  /** Reasoning selection: `null` = no reasoning controls, `"none"` = thinking off. */
  reasoningEffort?: ReasoningEffort | null;
  usage: UsageRecord[];
};

/** One connected (or attempted) server, holding its own catalog and sessions. */
export type ServerState = {
  url: string;
  /** User-given display name; falls back to the URL when absent. */
  alias?: string;
  status: ConnectionStatus;
  error?: string;
  catalog: WorkspaceSummary[];
  /** Providers this server offers, for the model selector. */
  providers: ProviderInfo[];
  /** Provider new sessions default to (from the provider catalog). */
  defaultProvider?: string;
  sessions: Record<SessionKey, OpenedSession>;
};

export type ServerSummary = Omit<ServerState, "sessions">;

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

/** Session-list title for a turn that carried only images (no text). Kept in
 * sync with `IMAGE_ONLY_PREVIEW` in the server's `storage.rs` so the optimistic
 * title matches the one the server persists. */
const IMAGE_ONLY_TITLE = "[image]";

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
    allowDrafts: {},
    running: false,
    usage: [],
  };
}

function blankServer(url: string): ServerState {
  return {
    url,
    status: "idle",
    catalog: [],
    providers: [],
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

export type StoredServer = { url: string; alias?: string };

function loadStoredServers(): StoredServer[] {
  try {
    const raw = window.localStorage.getItem(serversStorageKey);
    if (raw) {
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed)) {
        return parsed
          .map((value): StoredServer | null => {
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
  return [];
}

function storeServers(servers: StoredServer[]) {
  try {
    window.localStorage.setItem(serversStorageKey, JSON.stringify(servers));
  } catch {
    // ignore storage failures (private mode, disabled storage)
  }
}

function addStored(list: StoredServer[], url: string): StoredServer[] {
  return list.some((server) => server.url === url) ? list : [...list, { url }];
}

const modelPrefsStorageKey = "coda.modelPrefs";

type ModelPref = { providerId: string; reasoningEffort: ReasoningEffort | null };

/** Last model the user picked, keyed by server URL, so new sessions reuse it. */
function loadModelPrefs(): Record<string, ModelPref> {
  // Null-prototype record: server URLs are user-provided, so a key like
  // "__proto__" must not pollute the prototype when written back.
  const prefs: Record<string, ModelPref> = Object.create(null);
  try {
    const raw = window.localStorage.getItem(modelPrefsStorageKey);
    if (raw) {
      const parsed = JSON.parse(raw);
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        Object.assign(prefs, parsed);
      }
    }
  } catch {
    // ignore malformed/blocked storage
  }
  return prefs;
}

function rememberModelPref(server: string, pref: ModelPref) {
  try {
    const prefs = loadModelPrefs();
    prefs[server] = pref;
    window.localStorage.setItem(modelPrefsStorageKey, JSON.stringify(prefs));
  } catch {
    // ignore storage failures (private mode, disabled storage)
  }
}

function liveKey(agentName: string, threadId: string) {
  return `${agentName}:${threadId}`;
}

/** Reasoning streams under its own live key so it never merges with the answer entry. */
function reasoningLiveKey(agentName: string, threadId: string) {
  return `reasoning:${liveKey(agentName, threadId)}`;
}

function addActivity(session: OpenedSession, entry: Omit<ActivityEntry, "id">): OpenedSession {
  return {
    ...session,
    activity: [{ id: newId("activity"), ...entry }, ...session.activity].slice(0, 80),
  };
}

/** Tool call arguments keyed by call id, harvested from Assistant messages. */
function collectToolArgs(messages: HistoryMessage[]): Record<string, string | null | undefined> {
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

function historyToEntries(
  message: HistoryMessage,
  index: number,
  argsById: Record<string, string | null | undefined> = {},
): TranscriptEntry[] {
  if ("System" in message) {
    return [];
  }
  if ("User" in message) {
    const textContent = message.User.parts
      .filter((p) => p.type === "text")
      .map((p) => (p as { type: "text"; text: string }).text)
      .join("");
    const images = message.User.parts
      .filter((p) => p.type === "image")
      .map((p) => (p as { type: "image"; url: string }).url);
    return [
      {
        id: `history:user:${index}`,
        kind: "user",
        content: textContent,
        images: images.length > 0 ? images : undefined,
        startedAt: message.User.created_at,
      },
    ];
  }
  if ("Assistant" in message) {
    const assistant = message.Assistant;
    const entries: TranscriptEntry[] = [];
    if (assistant.reasoning_content) {
      entries.push({
        id: `history:reasoning:${index}`,
        kind: "reasoning",
        agentName: rootName,
        title: "Thinking",
        content: assistant.reasoning_content,
        startedAt: assistant.started_at,
        endedAt: assistant.reasoning_ended_at,
      });
    }
    if (assistant.content) {
      entries.push({
        id: `history:assistant:${index}`,
        kind: "assistant",
        agentName: rootName,
        content: assistant.content,
        status: assistant.aborted ? "aborted" : undefined,
        isFinalResponse: assistant.tool_calls.length === 0,
        startedAt: assistant.started_at,
        endedAt: assistant.ended_at,
      });
    }
    return entries;
  }
  if ("Tool" in message) {
    const argumentsJson = argsById[message.Tool.id];
    return [
      toolMessageToEntry(
        message.Tool,
        `history:tool:${index}`,
        describeTool(message.Tool.name, argumentsJson),
        message.Tool.name === "shell"
          ? extractShellCommand({
              id: message.Tool.id,
              name: message.Tool.name,
              arguments: argumentsJson,
            })
          : undefined,
      ),
    ];
  }
  return [];
}

function historyUsage(messages: HistoryMessage[]): UsageRecord[] {
  return messages.flatMap((message) => {
    if ("Assistant" in message && message.Assistant.usage) {
      return [{ agentName: rootName, usage: message.Assistant.usage }];
    }
    return [];
  });
}

function toolMessageToEntry(
  message: ToolMessage,
  id = newId("tool-result"),
  detail?: string,
  command?: string,
): TranscriptEntry {
  return {
    id,
    kind: "tool_result",
    callId: message.id,
    title: message.name,
    detail,
    command,
    content: outputText(message.output),
    status: outcomeText(message.outcome),
    startedAt: message.started_at,
    endedAt: message.ended_at,
  };
}

function finishToolEntry(
  session: OpenedSession,
  event: Extract<WireEvent, { type: "tool_end" }>,
): OpenedSession {
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
    ...toolMessageToEntry(
      event.message,
      entries[index].id,
      entries[index].detail,
      entries[index].command,
    ),
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
        isFinalResponse: false,
      },
    ],
  };
}

function addOrUpdateReasoningChunk(
  session: OpenedSession,
  event: Extract<WireEvent, { type: "llm_reasoning_chunk" }>,
): OpenedSession {
  const key = reasoningLiveKey(event.agent_name, event.thread_id);
  const index = session.entries.findIndex((entry) => entry.liveKey === key);
  if (index >= 0) {
    const entries = [...session.entries];
    entries[index] = {
      ...entries[index],
      content: entries[index].content + event.content,
    };
    return { ...session, entries };
  }
  if (!event.content) {
    return session;
  }
  return {
    ...session,
    entries: [
      ...session.entries,
      {
        id: newId("reasoning"),
        kind: "reasoning",
        agentName: event.agent_name,
        threadId: event.thread_id,
        title: "Thinking",
        content: event.content,
        status: "thinking",
        liveKey: key,
        startedAt: new Date().toISOString(),
      },
    ],
  };
}

/**
 * Settle the live reasoning entry, if any. Called when answer content starts
 * or the turn ends, so a later turn on the same thread starts a fresh entry.
 */
function finishReasoning(
  session: OpenedSession,
  agentName: string,
  threadId: string,
  updates: Partial<TranscriptEntry> = {},
): OpenedSession {
  const key = reasoningLiveKey(agentName, threadId);
  const index = session.entries.findIndex((entry) => entry.liveKey === key);
  if (index < 0) {
    return session;
  }
  const entries = [...session.entries];
  entries[index] = {
    ...entries[index],
    ...updates,
    status: undefined,
    liveKey: undefined,
  };
  return { ...session, entries };
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

function finishAssistant(
  session: OpenedSession,
  event: Extract<WireEvent, { type: "llm_end" }>,
): OpenedSession {
  const key = liveKey(event.agent_name, event.thread_id);
  const isFinalResponse = event.agent_name === rootName && event.message.tool_calls.length === 0;
  if (session.entries.some((entry) => entry.liveKey === key)) {
    return finishLiveEntry(session, event.agent_name, event.thread_id, {
      status: event.message.aborted ? "aborted" : undefined,
      isFinalResponse,
      startedAt: event.message.started_at,
      endedAt: event.message.ended_at,
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
          status: event.message.aborted ? "aborted" : undefined,
          isFinalResponse,
          startedAt: event.message.started_at,
          endedAt: event.message.ended_at,
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
      // Answer content marks the end of the reasoning phase.
      return addOrUpdateAssistantChunk(
        finishReasoning(session, event.agent_name, event.thread_id, {
          endedAt: new Date().toISOString(),
        }),
        event,
      );
    case "llm_reasoning_chunk":
      return addOrUpdateReasoningChunk(session, event);
    case "llm_end": {
      // The turn is finished only when the root agent stops without requesting
      // more tools; otherwise more work (tools / sub-agents) is still pending.
      const turnComplete = event.agent_name === rootName && event.message.tool_calls.length === 0;
      const finished = {
        ...addActivity(
          finishAssistant(
            finishReasoning(session, event.agent_name, event.thread_id, {
              startedAt: event.message.started_at,
              endedAt: event.message.reasoning_ended_at,
            }),
            event,
          ),
          {
            tone: event.message.aborted ? "warning" : "success",
            label: `${event.agent_name} finished`,
            detail: event.message.usage
              ? `${
                  event.message.usage.prompt_tokens + event.message.usage.completion_tokens
                } tokens`
              : "turn complete",
          },
        ),
        running: turnComplete ? false : session.running,
      };
      return event.message.usage
        ? {
            ...finished,
            usage: [...finished.usage, { agentName: event.agent_name, usage: event.message.usage }],
          }
        : finished;
    }
    case "tool_start":
      return {
        ...addActivity(session, {
          tone: event.agent_name === rootName ? "warning" : "cyan",
          label: `${event.agent_name} tool`,
          detail: subAgentDisplayName(event.call.name),
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
            command: event.call.name === "shell" ? extractShellCommand(event.call) : undefined,
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
          detail: subAgentDisplayName(event.message.name),
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
        finishLiveEntry(
          finishReasoning(session, event.agent_name, event.thread_id),
          event.agent_name,
          event.thread_id,
        ),
        {
          tone: "warning",
          label: `${event.agent_name} aborted`,
          detail: event.target.reason,
        },
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
            status: "aborted",
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
        finishLiveEntry(
          finishReasoning(session, event.agent_name, event.thread_id),
          event.agent_name,
          event.thread_id,
        ),
        {
          tone: "danger",
          label: `${event.agent_name || "server"} error`,
          detail: event.message,
        },
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

function upsertCatalogSession(catalog: WorkspaceSummary[], workspaceId: string, sessionId: string) {
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
  sessions: Record<SessionKey, OpenedSession>,
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

type CodaStore = Store<CodaStoreState>;
type CodaDraft = Draft<CodaStoreState>;

function updateState(store: CodaStore, updater: (state: CodaDraft) => void) {
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

function draftSession(state: CodaDraft, server: string, key: SessionKey) {
  const current = state.servers[server];
  if (!current) {
    return undefined;
  }
  const { workspaceId, sessionId } = splitKey(key);
  current.sessions[key] ??= blankSession(workspaceId, sessionId);
  return current.sessions[key];
}

type SessionRestoreMessage = {
  message: ClientMessage;
  key: SessionKey;
};

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
  error?: string,
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
  workspaces: WorkspaceSummary[],
  mergeLocal = true,
) {
  updateState(store, (state) => {
    const current = state.servers[server];
    if (current) {
      current.catalog = mergeLocal ? mergeCatalog(workspaces, current.sessions) : workspaces;
    }
  });
}

function setProviderCatalog(
  store: CodaStore,
  server: string,
  providers: ProviderInfo[],
  defaultProvider: string,
) {
  updateState(store, (state) => {
    const current = state.servers[server];
    if (current) {
      current.providers = providers;
      current.defaultProvider = defaultProvider;
      for (const session of Object.values(current.sessions)) {
        if (session.draft && !session.providerId) {
          const seed = seedSelection(current);
          session.providerId = seed.providerId;
          session.reasoningEffort = seed.reasoningEffort;
        }
      }
    }
  });
}

function createDraftSession(
  store: CodaStore,
  server: string,
  workspaceId: string,
  sessionId: string,
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
    const seed = seedSelection(current);
    current.sessions[key] = {
      ...blankSession(workspaceId, sessionId),
      draft: true,
      providerId: seed.providerId,
      reasoningEffort: seed.reasoningEffort,
    };
  });
}

/**
 * Initial model selection for a freshly created (draft) session: the model the
 * user last used on this server (remembered per server URL), falling back to the
 * server's default provider at its first declared effort. Empty when the
 * provider catalog hasn't arrived yet, leaving the selector hidden until it has.
 */
function seedSelection(server: ServerState): {
  providerId?: string;
  reasoningEffort: ReasoningEffort | null;
} {
  const remembered = loadModelPrefs()[server.url];
  const rememberedProvider = remembered
    ? server.providers.find((item) => item.id === remembered.providerId)
    : undefined;
  const provider =
    rememberedProvider ??
    server.providers.find((item) => item.id === server.defaultProvider) ??
    server.providers[0];
  if (!provider) {
    return { providerId: undefined, reasoningEffort: null };
  }
  const reasoningEffort =
    rememberedProvider && remembered
      ? validEffort(provider, remembered.reasoningEffort)
      : (provider.reasoning_efforts[0] ?? null);
  return { providerId: provider.id, reasoningEffort };
}

/** Keep a remembered reasoning effort only when it still applies to the model. */
function validEffort(
  provider: ProviderInfo,
  effort: ReasoningEffort | null,
): ReasoningEffort | null {
  if (provider.reasoning_efforts.length === 0) {
    return null;
  }
  if (effort === "none" || (effort && provider.reasoning_efforts.includes(effort))) {
    return effort;
  }
  return provider.reasoning_efforts[0];
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
        workspace.sessions = workspace.sessions.filter((session) => session.id !== sessionId);
      }
    }
    const clearingActive = state.activeServer === server && state.activeKey === key;
    if (clearingActive) {
      state.activeKey = undefined;
    }
  });
}

function selectSession(store: CodaStore, server: string, workspaceId: string, sessionId: string) {
  const key = sessionKey(workspaceId, sessionId);
  updateState(store, (state) => {
    const current = state.servers[server];
    if (!current) {
      return;
    }
    state.activeServer = server;
    state.activeKey = key;
    const session = draftSession(state, server, key);
    // A history session opened this browser-session for the first time has no
    // model yet; the server doesn't persist one, so seed the remembered model
    // (or the default) and let `open_session` carry it instead of resetting.
    if (session && !session.providerId) {
      const seed = seedSelection(current);
      session.providerId = seed.providerId;
      session.reasoningEffort = seed.reasoningEffort;
    }
  });
}

function applySnapshot(
  store: CodaStore,
  server: string,
  workspaceId: string,
  sessionId: string,
  messages: HistoryMessage[],
  approvals: PendingApproval[],
  providerId: string,
  reasoningEffort: ReasoningEffort | null,
  replaceEmpty = false,
) {
  const key = sessionKey(workspaceId, sessionId);
  const argsById = collectToolArgs(messages);
  const mapped = messages.flatMap((message, index) => historyToEntries(message, index, argsById));
  const usage = historyUsage(messages);
  const hasHistory = messages.length > 0;
  updateState(store, (state) => {
    const current = state.servers[server];
    if (!current) {
      return;
    }
    current.status = "connected";
    current.catalog = upsertCatalogSession(current.catalog, workspaceId, sessionId);
    const session = draftSession(state, server, key);
    if (!session) {
      return;
    }
    session.draft = false;
    session.providerId = providerId;
    session.reasoningEffort = reasoningEffort;
    session.usage = usage;
    if (hasHistory || replaceEmpty) {
      session.entries = mapped;
      session.approvals = approvals;
      session.drafts = {};
      session.allowDrafts = {};
      session.running = false;
    }
    if (replaceEmpty && !hasHistory) {
      session.firstUserMessage = undefined;
    }
  });
}

function setSessionModel(
  store: CodaStore,
  server: string,
  key: SessionKey,
  providerId: string,
  reasoningEffort: ReasoningEffort | null,
) {
  updateState(store, (state) => {
    const session = draftSession(state, server, key);
    if (session) {
      session.providerId = providerId;
      session.reasoningEffort = reasoningEffort;
    }
  });
}

function applyEvent(
  store: CodaStore,
  server: string,
  workspaceId: string,
  sessionId: string,
  event: WireEvent,
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
  error?: string | null,
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
  content: string,
  images?: string[],
) {
  updateState(store, (state) => {
    const current = state.servers[server];
    const session = draftSession(state, server, key);
    if (!current || !session) {
      return;
    }
    const { workspaceId, sessionId } = splitKey(key);
    // Fall back to an image placeholder when the turn has no text, so the session
    // shows a title in the list (instead of the raw id) and isn't dropped from
    // the optimistic catalog, which keys on a non-empty title.
    const firstUserMessage =
      session.firstUserMessage ??
      (content || (images && images.length > 0 ? IMAGE_ONLY_TITLE : ""));
    session.draft = false;
    session.running = true;
    session.firstUserMessage = firstUserMessage;
    session.entries.push({
      id: newId("user"),
      kind: "user",
      content,
      images: images && images.length > 0 ? images : undefined,
      startedAt: new Date().toISOString(),
    });
    current.catalog = upsertCatalogTitled(
      current.catalog,
      workspaceId,
      sessionId,
      firstUserMessage,
    );
  });
}

function setDraftResolution(
  store: CodaStore,
  server: string,
  key: SessionKey,
  approval: PendingApproval,
  call: ToolCall,
  resolution: ToolCallResolution,
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

function clearDraftResolution(
  store: CodaStore,
  server: string,
  key: SessionKey,
  approval: PendingApproval,
  call: ToolCall,
) {
  updateState(store, (state) => {
    const session = draftSession(state, server, key);
    if (!session) {
      return;
    }
    const approvalId = approvalKey(approval);
    const draft = session.drafts[approvalId];
    if (draft) {
      delete draft[call.id];
      if (Object.keys(draft).length === 0) {
        delete session.drafts[approvalId];
      }
    }
  });
}

function setAllowDraftPattern(
  store: CodaStore,
  server: string,
  key: SessionKey,
  approval: PendingApproval,
  call: ToolCall,
  pattern: string | null,
) {
  updateState(store, (state) => {
    const session = draftSession(state, server, key);
    if (!session) {
      return;
    }
    const approvalId = approvalKey(approval);
    const value = pattern?.trim();
    if (value) {
      session.allowDrafts[approvalId] ??= {};
      session.allowDrafts[approvalId][call.id] = value;
    } else if (session.allowDrafts[approvalId]) {
      delete session.allowDrafts[approvalId][call.id];
    }
  });
}

function clearApprovalState(
  store: CodaStore,
  server: string,
  key: SessionKey,
  approval: PendingApproval,
) {
  updateState(store, (state) => {
    const session = draftSession(state, server, key);
    if (!session) {
      return;
    }
    const approvalId = approvalKey(approval);
    delete session.drafts[approvalId];
    delete session.allowDrafts[approvalId];
    session.approvals = session.approvals.filter((item) => approvalKey(item) !== approvalId);
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

/**
 * Build an `open_session` command, carrying the session's chosen model when it
 * has one (a draft seeds it locally) so the server opens on that provider rather
 * than the default.
 */
function openMessage(session: OpenedSession): ClientMessage {
  return {
    type: "open_session",
    workspace_id: session.workspaceId,
    session_id: session.sessionId,
    ...(session.providerId
      ? {
          provider_id: session.providerId,
          reasoning_effort: session.reasoningEffort ?? null,
        }
      : {}),
  };
}

/**
 * The single, app-wide store. Lives at module scope (not per-component) so any
 * component can subscribe to just the slice it needs via `useCodaStore`, and
 * actions are plain functions that close over it — no hook, no prop drilling.
 */
export const codaStore = create<CodaStoreState>(initialStoreState);

// --- Actions (plain functions, stable identity) ------------------------------

function send(server: string, message: ClientMessage) {
  const socket = currentSocket(codaStore, server);
  if (socket?.readyState === WebSocket.OPEN) {
    socket.send(encode(message));
    return true;
  }
  setServerStatus(codaStore, server, "error", "Connection closed");
  return false;
}

function currentActive() {
  const snapshot = codaStore.getState();
  const server = snapshot.activeServer;
  const key = snapshot.activeKey;
  if (!server || !key) {
    return undefined;
  }
  const session = snapshot.servers[server]?.sessions[key];
  return session ? { server, session } : undefined;
}

function activeSessionToRestore(server: string): SessionRestoreMessage | undefined {
  const snapshot = codaStore.getState();
  if (snapshot.activeServer !== server || !snapshot.activeKey) {
    return undefined;
  }
  const session = snapshot.servers[server]?.sessions[snapshot.activeKey];
  return session?.draft
    ? undefined
    : {
        message: openMessage(session),
        key: snapshot.activeKey,
      };
}

export function connectServer(rawUrl: string) {
  const server = rawUrl.trim();
  if (!server) {
    return;
  }
  const sessionToRestore = activeSessionToRestore(server);
  closeSocket(codaStore, server);
  const stored = loadStoredServers();
  storeServers(addStored(stored, server));
  markConnecting(codaStore, server, stored.find((entry) => entry.url === server)?.alias);

  const socket = new WebSocket(normalizeWsUrl(server));
  setSocket(codaStore, server, socket);
  let replaceNextCatalog = true;

  socket.onopen = () => {
    setServerStatus(codaStore, server, "connected");
    socket.send(encode({ type: "list_workspaces" }));
    socket.send(encode({ type: "list_providers" }));
    if (sessionToRestore) {
      socket.send(encode(sessionToRestore.message));
    }
  };
  socket.onclose = () => {
    if (currentSocket(codaStore, server) === socket) {
      setServerStatus(codaStore, server, "closed");
    }
  };
  socket.onerror = () => setServerStatus(codaStore, server, "error", "WebSocket connection failed");
  socket.onmessage = (event: MessageEvent<string>) => {
    try {
      const message = JSON.parse(event.data) as ServerMessage;
      if (message.type === "workspace_catalog") {
        setCatalog(codaStore, server, message.workspaces, !replaceNextCatalog);
        replaceNextCatalog = false;
        return;
      }
      if (message.type === "provider_catalog") {
        setProviderCatalog(codaStore, server, message.providers, message.default_provider);
        return;
      }
      if (message.type === "snapshot") {
        applySnapshot(
          codaStore,
          server,
          message.workspace_id,
          message.session_id,
          message.messages,
          message.pending_approvals ?? [],
          message.provider_id,
          message.reasoning_effort ?? null,
          sessionToRestore?.key === sessionKey(message.workspace_id, message.session_id),
        );
        return;
      }
      if (message.type === "model_changed") {
        setSessionModel(
          codaStore,
          server,
          sessionKey(message.workspace_id, message.session_id),
          message.provider_id,
          message.reasoning_effort ?? null,
        );
        return;
      }
      if (message.type === "event") {
        applyEvent(codaStore, server, message.workspace_id, message.session_id, message.event);
        return;
      }
      addAllowResultActivity(
        codaStore,
        server,
        message.workspace_id,
        message.pattern,
        message.error,
      );
    } catch (error) {
      setServerStatus(
        codaStore,
        server,
        "error",
        error instanceof Error ? error.message : "Invalid server message",
      );
    }
  };
}

export function removeServer(rawUrl: string) {
  const server = rawUrl.trim();
  if (!server) {
    return;
  }
  closeSocket(codaStore, server);
  removeSocket(codaStore, server);
  storeServers(loadStoredServers().filter((entry) => entry.url !== server));
  removeServerState(codaStore, server);
}

export function disconnectServer(rawUrl: string) {
  const server = rawUrl.trim();
  if (!server) {
    return;
  }
  closeSocket(codaStore, server);
  removeSocket(codaStore, server);
  setServerStatus(codaStore, server, "closed");
}

export function renameServer(rawUrl: string, rawAlias: string) {
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
  setServerAlias(codaStore, server, alias);
}

export function newSession(server: string, workspaceId: string) {
  const workspace = workspaceId.trim();
  if (!server || !workspace) {
    return;
  }
  const current = codaStore.getState().servers[server];
  const reusable = current
    ? Object.values(current.sessions).find(
        (session) =>
          session.draft && session.workspaceId === workspace && session.entries.length === 0,
      )
    : undefined;
  const sessionId = reusable?.sessionId ?? freshSessionId();
  closeActiveSession(server, sessionKey(workspace, sessionId));
  createDraftSession(codaStore, server, workspace, sessionId);
}

export function deleteSession(server: string, workspaceId: string, sessionId: string) {
  const workspace = workspaceId.trim();
  const session = sessionId.trim();
  if (!server || !workspace || !session) {
    return;
  }
  const key = sessionKey(workspace, session);
  const local = codaStore.getState().servers[server]?.sessions[key];
  if (!local?.draft) {
    send(server, {
      type: "delete_session",
      workspace_id: workspace,
      session_id: session,
    });
  }
  deleteSessionState(codaStore, server, key);
}

/**
 * Ask the server to close the currently-active session when switching away to
 * `nextKey`, freeing its runtime memory. The server decides the timing: an idle
 * session is torn down at once, one with a turn still running is torn down when
 * that turn settles (so background work isn't aborted), and reopening before
 * then cancels it. Drafts are skipped — they were never opened on the server.
 * The local transcript is kept; reopening re-sends `open_session` and the server
 * restores it from its persisted checkpoint.
 */
function closeActiveSession(nextServer?: string, nextKey?: SessionKey) {
  const snapshot = codaStore.getState();
  const server = snapshot.activeServer;
  const key = snapshot.activeKey;
  if (!server || !key || (server === nextServer && key === nextKey)) {
    return;
  }
  const session = snapshot.servers[server]?.sessions[key];
  if (!session || session.draft) {
    return;
  }
  send(server, {
    type: "close_session",
    workspace_id: session.workspaceId,
    session_id: session.sessionId,
  });
}

export function openSession(server: string, workspaceId: string, sessionId: string) {
  const workspace = workspaceId.trim();
  const session = sessionId.trim();
  if (!server || !workspace || !session) {
    return;
  }
  const local = codaStore.getState().servers[server]?.sessions[sessionKey(workspace, session)];
  closeActiveSession(server, sessionKey(workspace, session));
  selectSession(codaStore, server, workspace, session);
  if (!local?.draft) {
    const opened =
      local ?? codaStore.getState().servers[server]?.sessions[sessionKey(workspace, session)];
    if (opened) {
      send(server, openMessage(opened));
    }
  }
}

/** Deselect whatever session is currently shown in the center pane (e.g. when
 * switching into the new-session composer). */
export function clearActiveSession() {
  closeActiveSession();
  updateState(codaStore, (state) => {
    state.activeServer = undefined;
    state.activeKey = undefined;
  });
}

export function sendTask(task: string, images: string[] = []) {
  const text = task.trim();
  const active = currentActive();
  if (!text && images.length === 0) {
    return;
  }
  if (!active) {
    return;
  }
  if (active.session.draft) {
    send(active.server, openMessage(active.session));
  }
  if (
    send(active.server, {
      type: "task",
      workspace_id: active.session.workspaceId,
      session_id: active.session.sessionId,
      task: text,
      images: images.length > 0 ? images : undefined,
    })
  ) {
    appendUserMessage(codaStore, active.server, active.session.key, text, images);
  }
}

export function sendTaskToNewSession(
  server: string,
  workspaceId: string,
  task: string,
  providerId?: string,
  reasoningEffort: ReasoningEffort | null = null,
  images: string[] = [],
) {
  const workspace = workspaceId.trim();
  const text = task.trim();
  if (!server || !workspace || (!text && images.length === 0)) {
    return;
  }
  const current = codaStore.getState().servers[server];
  const reusable = current
    ? Object.values(current.sessions).find(
        (session) =>
          session.draft && session.workspaceId === workspace && session.entries.length === 0,
      )
    : undefined;
  const sessionId = reusable?.sessionId ?? freshSessionId();
  const key = sessionKey(workspace, sessionId);
  closeActiveSession(server, key);
  createDraftSession(codaStore, server, workspace, sessionId);
  if (providerId) {
    setSessionModel(codaStore, server, key, providerId, reasoningEffort);
  }
  const session = codaStore.getState().servers[server]?.sessions[key];
  if (!session) {
    return;
  }
  send(server, openMessage(session));
  if (
    send(server, {
      type: "task",
      workspace_id: workspace,
      session_id: sessionId,
      task: text,
      images: images.length > 0 ? images : undefined,
    })
  ) {
    appendUserMessage(codaStore, server, key, text, images);
  }
}

export function abort() {
  const active = currentActive();
  if (active) {
    send(active.server, {
      type: "abort",
      workspace_id: active.session.workspaceId,
      session_id: active.session.sessionId,
    });
  }
}

/** Stage (or clear, with `null`) an "always allow" pattern for a call. The
 * pattern is only sent to the server on submit, so the choice is cancelable. */
export function setAllowDraft(approval: PendingApproval, call: ToolCall, pattern: string | null) {
  const active = currentActive();
  if (!active) {
    return;
  }
  setAllowDraftPattern(codaStore, active.server, active.session.key, approval, call, pattern);
}

export function setModel(providerId: string, reasoningEffort: ReasoningEffort | null) {
  const active = currentActive();
  if (!active) {
    return;
  }
  rememberModelPref(active.server, { providerId, reasoningEffort });
  if (active.session.draft) {
    setSessionModel(codaStore, active.server, active.session.key, providerId, reasoningEffort);
    return;
  }
  send(active.server, {
    type: "set_model",
    workspace_id: active.session.workspaceId,
    session_id: active.session.sessionId,
    provider_id: providerId,
    reasoning_effort: reasoningEffort,
  });
}

export function draftCall(
  approval: PendingApproval,
  call: ToolCall,
  resolution: ToolCallResolution,
) {
  const active = currentActive();
  if (!active) {
    return;
  }
  setDraftResolution(codaStore, active.server, active.session.key, approval, call, resolution);
}

export function clearDraftCall(approval: PendingApproval, call: ToolCall) {
  const active = currentActive();
  if (!active) {
    return;
  }
  clearDraftResolution(codaStore, active.server, active.session.key, approval, call);
}

export function submitApprovals() {
  const active = currentActive();
  if (!active) {
    return;
  }
  for (const approval of active.session.approvals) {
    const approvalId = approvalKey(approval);
    const draft = active.session.drafts[approvalId] ?? {};
    const complete = approval.calls.every((item) => draft[item.id]);
    if (!complete) {
      continue;
    }
    // Persist staged "always allow" patterns for approved calls only.
    const allow = active.session.allowDrafts[approvalId] ?? {};
    for (const item of approval.calls) {
      const pattern = allow[item.id];
      if (pattern && draft[item.id] === "Execute") {
        send(active.server, {
          type: "add_allow_pattern",
          workspace_id: active.session.workspaceId,
          pattern,
        });
      }
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
    clearApprovalState(codaStore, active.server, active.session.key, approval);
  }
}

// --- Selectors ---------------------------------------------------------------
// Stable empties so default-valued selectors keep referential identity and
// don't force re-renders under `useSyncExternalStore`.

const EMPTY_ENTRIES: TranscriptEntry[] = [];
const EMPTY_APPROVALS: PendingApproval[] = [];
const EMPTY_DRAFTS: Record<string, Record<string, ToolCallResolution>> = {};
const EMPTY_ALLOW_DRAFTS: Record<string, Record<string, string>> = {};
const EMPTY_PROVIDERS: ProviderInfo[] = [];

function activeServerOf(state: CodaStoreState): ServerState | undefined {
  return state.activeServer ? state.servers[state.activeServer] : undefined;
}

function activeSessionOf(state: CodaStoreState): OpenedSession | undefined {
  const server = activeServerOf(state);
  return server && state.activeKey ? server.sessions[state.activeKey] : undefined;
}

export const selectServers = (state: CodaStoreState): ServerState[] =>
  state.order
    .map((url) => state.servers[url])
    .filter((server): server is ServerState => Boolean(server));

let cachedServerSummaries: ServerSummary[] = [];

function summaryMatchesServer(summary: ServerSummary, server: ServerState): boolean {
  return (
    summary.url === server.url &&
    summary.alias === server.alias &&
    summary.status === server.status &&
    summary.error === server.error &&
    summary.catalog === server.catalog &&
    summary.providers === server.providers &&
    summary.defaultProvider === server.defaultProvider
  );
}

export const selectServerSummaries = (state: CodaStoreState): ServerSummary[] => {
  if (
    state.order.length === cachedServerSummaries.length &&
    state.order.every((url, index) => {
      const server = state.servers[url];
      return Boolean(server) && summaryMatchesServer(cachedServerSummaries[index], server);
    })
  ) {
    return cachedServerSummaries;
  }
  const previousByUrl = new Map(cachedServerSummaries.map((server) => [server.url, server]));
  const next = state.order.flatMap((url) => {
    const server = state.servers[url];
    if (!server) {
      return [];
    }
    const previous = previousByUrl.get(url);
    if (previous && summaryMatchesServer(previous, server)) {
      return [previous];
    }
    return [
      {
        url: server.url,
        alias: server.alias,
        status: server.status,
        error: server.error,
        catalog: server.catalog,
        providers: server.providers,
        defaultProvider: server.defaultProvider,
      },
    ];
  });
  cachedServerSummaries = next;
  return next;
};

export const selectActiveServer = (state: CodaStoreState) => state.activeServer;
export const selectActiveKey = (state: CodaStoreState) => state.activeKey;
export const selectActiveEntries = (state: CodaStoreState) =>
  activeSessionOf(state)?.entries ?? EMPTY_ENTRIES;
/** Whether the active session's history carries any image attachment, so the
 * model selection must stay on a vision-capable model. */
export const selectActiveHasImages = (state: CodaStoreState): boolean =>
  (activeSessionOf(state)?.entries ?? EMPTY_ENTRIES).some(
    (entry) => (entry.images?.length ?? 0) > 0,
  );
export const selectActiveRunning = (state: CodaStoreState) =>
  activeSessionOf(state)?.running ?? false;
export const selectActiveApprovals = (state: CodaStoreState) =>
  activeSessionOf(state)?.approvals ?? EMPTY_APPROVALS;
export const selectActiveDrafts = (state: CodaStoreState) =>
  activeSessionOf(state)?.drafts ?? EMPTY_DRAFTS;
export const selectActiveAllowDrafts = (state: CodaStoreState) =>
  activeSessionOf(state)?.allowDrafts ?? EMPTY_ALLOW_DRAFTS;
export const selectActiveApprovalCount = (state: CodaStoreState) =>
  activeSessionOf(state)?.approvals.length ?? 0;
export const selectActiveWorkspace = (state: CodaStoreState) => activeSessionOf(state)?.workspaceId;
/** Title of the active session (its first user message), for the header
 * breadcrumb. `undefined` while a blank/draft session has no message yet. */
export const selectActiveSessionTitle = (state: CodaStoreState): string | undefined => {
  const session = activeSessionOf(state);
  if (!session || session.draft) {
    return undefined;
  }
  const server = activeServerOf(state);
  const workspace = server?.catalog.find((ws) => ws.id === session.workspaceId);
  const summary = workspace?.sessions.find((item) => item.id === session.sessionId);
  const title = summary?.first_user_message ?? session.firstUserMessage;
  return title?.trim() || session.sessionId;
};
export const selectActiveDraftFlag = (state: CodaStoreState) =>
  activeSessionOf(state)?.draft ?? false;
export const selectActiveStatus = (state: CodaStoreState): ConnectionStatus =>
  activeServerOf(state)?.status ?? "idle";
export const selectActiveProviders = (state: CodaStoreState): ProviderInfo[] =>
  activeServerOf(state)?.providers ?? EMPTY_PROVIDERS;
export const selectActiveProviderId = (state: CodaStoreState) => activeSessionOf(state)?.providerId;
export const selectActiveReasoningEffort = (state: CodaStoreState) =>
  activeSessionOf(state)?.reasoningEffort ?? null;
const EMPTY_USAGE: UsageRecord[] = [];
export const selectActiveUsage = (state: CodaStoreState) =>
  activeSessionOf(state)?.usage ?? EMPTY_USAGE;

/** Subscribe to a slice of the store; re-renders only when that slice changes. */
export function useCodaStore<T>(selector: (state: CodaStoreState) => T): T {
  return useStore(codaStore, selector);
}

/** Subscribe to a computed slice with shallow equality (for arrays/objects). */
export function useCodaShallow<T>(selector: (state: CodaStoreState) => T): T {
  return useStore(codaStore, useShallow(selector));
}

/**
 * Auto-connect stored servers once, and close sockets on teardown. Mount once,
 * at the app root. Resets `autoConnected` on cleanup so React StrictMode's
 * mount→unmount→mount cycle correctly reconnects.
 */
export function useCodaBootstrap() {
  useEffect(() => {
    if (codaStore.getState().autoConnected) {
      return;
    }
    markAutoConnected(codaStore);
    for (const { url } of loadStoredServers()) {
      connectServer(url);
    }
  }, []);

  useEffect(
    () => () => {
      updateState(codaStore, (state) => {
        state.autoConnected = false;
      });
      for (const socket of Object.values(codaStore.getState().wsMap)) {
        socket.close();
      }
    },
    [],
  );
}
