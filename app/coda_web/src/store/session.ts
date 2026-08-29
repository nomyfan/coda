import { useEffect } from "react";
import { useStore } from "zustand";
import type { Draft } from "immer";
import {
  approvalKey,
  type CompletionUsage,
  type HistoryMessage,
  type PendingApproval,
  type PermissionMode,
  DEFAULT_PERMISSION_MODE,
  type ProviderInfo,
  type ReasoningEffort,
  type RpcNotifications,
  type RpcPushes,
  type RpcRequests,
  RpcCode,
  type SkillInfo,
  type TaskNoticeOutcome,
  type TaskSummary,
  type ToolCall,
  type ToolCallResolution,
  type ToolArtifact,
  type ToolMessage,
  type WireEvent,
  type WorkspaceFile,
  type WorkspaceSession,
  type WorkspaceSummary,
  callArguments,
  describeTool,
  extractRunJavaScriptCode,
  extractShellCommand,
  outcomeText,
  outputText,
  toolDisplayName,
} from "@/lib/protocol";
import { parseCompactCommand } from "@/lib/compact-command";
import { createRpcClient, isServerError, type RpcClient } from "@/lib/rpc";
import { sessionTitle } from "@/components/session-utils";
import { initialModelSelection, rememberModelSelection } from "@/store/model-preferences";
import {
  forgetSessionMode,
  initialSessionMode,
  rememberSessionMode,
} from "@/store/permission-modes";
import { create, type Store } from "@/store/utils";

export type {
  CompletionUsage,
  PermissionMode,
  ProviderInfo,
  ReasoningEffort,
  SkillInfo,
  WorkspaceFile,
  WorkspaceSession,
  WorkspaceSummary,
} from "@/lib/protocol";

export type ConnectionStatus = "idle" | "connecting" | "connected" | "closed" | "error";

export type TranscriptEntry = {
  id: string;
  kind:
    | "user"
    | "assistant"
    | "reasoning"
    | "tool_call"
    | "tool_result"
    | "compaction"
    | "task_notice"
    | "system"
    | "error";
  /** The server's id for this message, once the server has acknowledged it.
   * On a user entry that is the condition for rewinding or forking from it. */
  messageId?: string;
  /** Marker on the optimistic copy of a `/compact` command, shown ahead of the
   * compaction's own write. Unlike a pending task entry it is not a turn — it
   * must not set `running` — and it is retired by content against the recorded
   * line, not by an id, because `compact` answers without one. */
  pendingCompact?: boolean;
  agentName?: string;
  threadId?: string;
  title?: string;
  /** Short summary of what a tool acts on (file basename, shell command, …). */
  detail?: string;
  /** Executed shell command, shown alongside shell results. */
  command?: string;
  /** Model-generated source executed by run_javascript. */
  script?: string;
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
  /** On a tool entry: when the message that asked for this call was generated.
   * Never displayed. A round producing neither prose nor reasoning leaves no
   * entry of its own, so this is the turn's only record of that time. */
  generation?: GenerationSpan;
  /** Immutable presentation data persisted with a completed tool call. */
  artifacts?: ToolArtifact[];
};

export type GenerationSpan = { startedAt: string; endedAt: string };

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
  /** Suspended calls' original arguments, keyed by call id, kept around only
   * until `tool_start`/`tool_end` resolves each one (see `finishToolEntry`). */
  pendingCallInfo: Record<string, ToolCall>;
  /** When the message behind each unanswered call was generated, keyed by call
   * id. Kept here rather than derived per render because a call outlives the
   * message that asked for it: a snapshot can carry the assistant message while
   * its `tool_end` is still to come over the live stream. */
  generationSpans: Record<string, GenerationSpan>;
  drafts: Record<string, Record<string, ToolCallResolution>>;
  /** Per-call "always allow" patterns staged for an approval; sent to the
   * server only on submit, so the intent stays cancelable until then. */
  allowDrafts: Record<string, Record<string, string>>;
  running: boolean;
  /** A compaction runs outside a normal turn but still owns the session. */
  compacting: boolean;
  /** Background tasks this session started, newest first. Pushed on every
   * change and carried on the snapshot, since tasks outlive turns and the
   * client may attach long after one started. */
  backgroundTasks: TaskSummary[];
  /** A draft session's opening `open_session` is in flight, ahead of its first
   * task. `running` can't cover this window — it isn't set until the task is
   * actually sent — so without it a second submit during the round trip would
   * open the session and start a turn twice. */
  starting?: boolean;
  /** A `delete_session` request is in flight (or tombstoned across a
   * disconnect). While set, `open`/`task`/`set_model`/repeat-`delete` are
   * no-ops for this key, and a reconnect re-sends the (idempotent) delete. */
  deleting?: boolean;
  /** Another client took over this session (latest-wins); the transcript is
   * read-only until the user takes it back by reopening. */
  evicted: boolean;
  /** Created locally via "new session" but not yet opened on the server. */
  draft?: boolean;
  /** Why the server could not store the last turn. Kept outside `entries` on
   * purpose: a persist failure drops the session, and the snapshot the client
   * reattaches with replaces `entries` wholesale — a notice living in there
   * would be wiped by the very resync it is reporting on. Cleared when the user
   * dismisses it or a later turn finishes normally. */
  persistError?: string;
  /** First user task, used as the session list title before the server persists it. */
  firstUserMessage?: string;
  /** A historical message pulled back into the composer to be rewritten.
   *
   * `target` is the message a submit would rewind to; `null` means the
   * truncation already happened but the turn that should have replaced it did
   * not start, so this is now just a draft and the next submit is an ordinary
   * task. `text`/`images` are authoritative, not a seed: they are rewritten
   * from the composer on every submit so the draft survives a remount. */
  editing?: {
    target: string | null;
    text: string;
    images: string[];
    submitting: boolean;
    /** How many user messages sit before `target`, counted when the edit
     * opened. A rewind's reply can be lost to a dropped connection, and this is
     * what lets the next snapshot say whether the replacement turn started —
     * see `reconcileEditing`. */
    precedingUserMessages: number;
  };
  /** The prompt a fork branched away from, kept as this session's composer
   * draft until it is sent. */
  forkDraft?: { text: string; images: string[] };
  /** Provider this session currently uses; set from the server snapshot. */
  providerId?: string;
  /** Reasoning selection: `null` = no reasoning controls, `"none"` = thinking off. */
  reasoningEffort?: ReasoningEffort | null;
  /** How much this session may do unattended. Seeded from what this browser
   * remembers for *this session* (or the default for a new one), then replaced
   * by whatever the server's snapshot reports — a session already running
   * elsewhere keeps its own posture. */
  permissionMode: PermissionMode;
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
  /** Sessions with a fork in flight, by `forkKey`. Shared rather than local to
   * a button because one session has many fork entries — one per eligible user
   * message, plus the sidebar — and the server mints a new id per request, so a
   * second click anywhere leaves a second copy behind. */
  forking: Record<string, true>;
};

type SessionRuntimeState = {
  wsMap: Record<string, WebSocket>;
  /** One JSON-RPC adapter per connection, replaced on each reconnect. */
  rpcMap: Record<string, CodaRpcClient>;
  autoConnected: boolean;
};

type CodaStoreState = CodaState & SessionRuntimeState;
type CodaRpcClient = RpcClient<RpcRequests, RpcNotifications, RpcPushes>;

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
    pendingCallInfo: {},
    generationSpans: {},
    drafts: {},
    allowDrafts: {},
    running: false,
    compacting: false,
    backgroundTasks: [],
    evicted: false,
    permissionMode: DEFAULT_PERMISSION_MODE,
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
  forking: {},
};

function initialStoreState(): CodaStoreState {
  return {
    ...initialState,
    wsMap: {},
    rpcMap: {},
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

function liveKey(agentName: string, threadId: string) {
  return `${agentName}:${threadId}`;
}

/** Reasoning streams under its own live key so it never merges with the answer entry. */
function reasoningLiveKey(agentName: string, threadId: string) {
  return `reasoning:${liveKey(agentName, threadId)}`;
}

/** An in-flight auto-compaction, under its own live key for the same reason. */
function compactionLiveKey(agentName: string, threadId: string) {
  return `compaction:${liveKey(agentName, threadId)}`;
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

/** Map every call in this history back to the generation that asked for it.
 *
 * Every round, not just the ones that left no entry behind: the duration math
 * unions these spans, so a redundant copy costs nothing and skipping it would
 * cost a special case. */
function collectGenerationSpans(messages: HistoryMessage[]): Record<string, GenerationSpan> {
  const map: Record<string, GenerationSpan> = {};
  for (const message of messages) {
    if (!("Assistant" in message)) {
      continue;
    }
    const span = {
      startedAt: message.Assistant.started_at,
      endedAt: message.Assistant.ended_at,
    };
    for (const call of message.Assistant.tool_calls) {
      map[call.id] = span;
    }
  }
  return map;
}

/** Entry ids are derived from the server's `message_id` so a message keeps the
 * same key across a reload or reconnect. One assistant message can yield two
 * entries (its reasoning and its answer), which the kind prefix separates. */
function historyToEntries(
  message: HistoryMessage,
  argsById: Record<string, string | null | undefined>,
  spansById: Record<string, GenerationSpan>,
): TranscriptEntry[] {
  if ("User" in message) {
    const textContent = userMessageText(message);
    const images = message.User.parts
      .filter((p) => p.type === "image")
      .map((p) => (p as { type: "image"; url: string }).url);
    return [
      {
        id: userEntryId(message.User.message_id),
        messageId: message.User.message_id,
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
        id: `reasoning:${assistant.message_id}`,
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
        id: `assistant:${assistant.message_id}`,
        kind: "assistant",
        messageId: assistant.message_id,
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
    const call = {
      id: message.Tool.id,
      name: message.Tool.name,
      arguments: argumentsJson,
    };
    return [
      {
        ...toolMessageToEntry(
          message.Tool,
          `tool:${message.Tool.message_id}`,
          describeTool(message.Tool.name, argumentsJson),
          message.Tool.name === "shell" ? extractShellCommand(call) : undefined,
          spansById[message.Tool.id],
        ),
        script: extractRunJavaScriptCode(call),
      },
    ];
  }
  if ("Compaction" in message) {
    const compaction = message.Compaction;
    if (compaction.outcome.type === "summary") {
      return [
        {
          id: `compaction:${compaction.message_id}`,
          messageId: compaction.message_id,
          kind: "compaction",
          title: "Context compacted",
          content: compaction.content,
          startedAt: compaction.created_at,
        },
      ];
    }
    return [
      {
        id: `compaction-failed:${compaction.message_id}`,
        messageId: compaction.message_id,
        kind: "error",
        title: "Compaction failed",
        content: compaction.content,
        startedAt: compaction.created_at,
      },
    ];
  }
  if ("TaskNotice" in message) {
    const notice = message.TaskNotice;
    return [
      {
        id: `task-notice:${notice.message_id}`,
        messageId: notice.message_id,
        kind: "task_notice",
        title: taskNoticeTitle(notice.outcome),
        detail: notice.outcome.type === "finished" ? notice.outcome.command : undefined,
        content: notice.content,
        startedAt: notice.created_at,
      },
    ];
  }
  return [];
}

function taskNoticeTitle(outcome: TaskNoticeOutcome): string {
  switch (outcome.type) {
    case "finished":
      return `Background task ${outcome.status}`;
    case "output_expired":
      return "Background task output expired";
    case "capped":
      return `${outcome.events} more background task events`;
  }
}

function messageIdOf(message: HistoryMessage): string {
  if ("User" in message) return message.User.message_id;
  if ("Assistant" in message) return message.Assistant.message_id;
  if ("Tool" in message) return message.Tool.message_id;
  if ("TaskNotice" in message) return message.TaskNotice.message_id;
  return message.Compaction.message_id;
}

/** Mirrors the backend's `message_view::last_summary`: a recorded `cutoff`
 * can differ from the summary's physical position for a mid-turn compaction.
 * Falls back to the summary's own index when the cutoff message is no longer
 * in the history. */
function resolveCutoffIdx(messages: HistoryMessage[], summaryIdx: number, cutoff: string): number {
  for (let index = summaryIdx - 1; index >= 0; index -= 1) {
    if (messageIdOf(messages[index]) === cutoff) {
      return index + 1;
    }
  }
  return summaryIdx + 1;
}

function historyUsage(messages: HistoryMessage[]): UsageRecord[] {
  let boundary = 0;
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if ("Compaction" in message && message.Compaction.outcome.type === "summary") {
      boundary = resolveCutoffIdx(messages, index, message.Compaction.outcome.cutoff);
      break;
    }
  }
  return messages.slice(boundary).flatMap((message) => {
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
  generation?: GenerationSpan,
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
    generation,
    artifacts: message.artifacts,
  };
}

/** Recall a suspended call's arguments to describe it once resolved: a
 * rejected call never fires `tool_start`, so `finishToolEntry` otherwise has
 * nothing but the bare tool name to show. */
function withPendingCallInfo(
  session: OpenedSession,
  calls: ToolCall[],
): OpenedSession["pendingCallInfo"] {
  if (calls.length === 0) {
    return session.pendingCallInfo;
  }
  const next = { ...session.pendingCallInfo };
  for (const call of calls) {
    next[call.id] = call;
  }
  return next;
}

function withoutPendingCallInfo(
  session: OpenedSession,
  callId: string,
): OpenedSession["pendingCallInfo"] {
  if (!(callId in session.pendingCallInfo)) {
    return session.pendingCallInfo;
  }
  const next = { ...session.pendingCallInfo };
  delete next[callId];
  return next;
}

function withGenerationSpans(
  session: OpenedSession,
  calls: ToolCall[],
  span: GenerationSpan,
): OpenedSession["generationSpans"] {
  if (calls.length === 0) {
    return session.generationSpans;
  }
  const next = { ...session.generationSpans };
  for (const call of calls) {
    next[call.id] = span;
  }
  return next;
}

function withoutGenerationSpan(
  session: OpenedSession,
  callId: string,
): OpenedSession["generationSpans"] {
  if (!(callId in session.generationSpans)) {
    return session.generationSpans;
  }
  const next = { ...session.generationSpans };
  delete next[callId];
  return next;
}

function finishToolEntry(
  session: OpenedSession,
  event: Extract<WireEvent, { type: "tool_end" }>,
): OpenedSession {
  const generation = session.generationSpans[event.message.id];
  const index = session.entries.findIndex((entry) => entry.callId === event.message.id);
  if (index < 0) {
    const call = session.pendingCallInfo[event.message.id];
    return {
      ...session,
      pendingCallInfo: withoutPendingCallInfo(session, event.message.id),
      generationSpans: withoutGenerationSpan(session, event.message.id),
      entries: [
        ...session.entries,
        {
          ...toolMessageToEntry(
            event.message,
            undefined,
            call ? describeTool(call.name, call.arguments) : undefined,
            call?.name === "shell" ? extractShellCommand(call) : undefined,
            generation,
          ),
          script: call ? extractRunJavaScriptCode(call) : undefined,
        },
      ],
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
      generation,
    ),
    script: entries[index].script,
    agentName: event.agent_name,
    threadId: event.thread_id,
  };
  return {
    ...session,
    entries,
    generationSpans: withoutGenerationSpan(session, event.message.id),
  };
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

/** Drop the pending compaction shimmer, if any: `compaction_start` has no
 * paired terminal event on this path (an aborted summarize call returns with
 * nothing to report), so `aborted` is what clears it instead. */
function discardCompactionLiveEntry(
  session: OpenedSession,
  agentName: string,
  threadId: string,
): OpenedSession {
  const key = compactionLiveKey(agentName, threadId);
  if (!session.entries.some((entry) => entry.liveKey === key)) {
    return session;
  }
  return {
    ...session,
    entries: session.entries.filter((entry) => entry.liveKey !== key),
  };
}

function finishAssistant(
  session: OpenedSession,
  event: Extract<WireEvent, { type: "llm_end" }>,
): OpenedSession {
  const key = liveKey(event.agent_name, event.thread_id);
  const isFinalResponse = event.agent_name === rootName && event.message.tool_calls.length === 0;
  if (session.entries.some((entry) => entry.liveKey === key)) {
    return finishLiveEntry(session, event.agent_name, event.thread_id, {
      messageId: event.message.message_id,
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
          messageId: event.message.message_id,
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

// Everything a pending approval keeps alive, reset together when the turn it
// belongs to settles for good.
const clearedApprovalState: Pick<
  OpenedSession,
  "approvals" | "pendingCallInfo" | "drafts" | "allowDrafts"
> = {
  approvals: [],
  pendingCallInfo: {},
  drafts: {},
  allowDrafts: {},
};

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

export function reduceEvent(session: OpenedSession, event: WireEvent): OpenedSession {
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
      // An aborted partial message is not an ending either — the `aborted`
      // event always follows it and is what actually settles that path. This
      // mirrors the server's `event_settles_turn`; disagreeing with it would
      // let a cancelled generation pass for a completed turn.
      const turnComplete =
        event.agent_name === rootName &&
        event.message.tool_calls.length === 0 &&
        !event.message.aborted;
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
        // A turn that made it all the way to a stored ending answers the
        // outstanding "the last one wasn't saved" notice.
        persistError: turnComplete ? undefined : session.persistError,
        // Recorded here rather than at `tool_start` because a call that suspends
        // for approval and is then rejected never starts at all, and this is the
        // only point that sees the generation's own timing.
        generationSpans: withGenerationSpans(session, event.message.tool_calls, {
          startedAt: event.message.started_at,
          endedAt: event.message.ended_at,
        }),
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
          detail: toolDisplayName(event.call.name),
        }),
        running: true,
        pendingCallInfo: withoutPendingCallInfo(session, event.call.id),
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
            script: extractRunJavaScriptCode(event.call),
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
          detail: toolDisplayName(event.message.name),
        }),
      };
    case "compaction_start":
      // Only the root thread's own compaction belongs in the transcript — a
      // sub-agent's is invisible there today, same as its checkpoint never
      // reaching the root's snapshot (`fold_settled_turn`, server-side).
      if (event.agent_name !== rootName) {
        return session;
      }
      return {
        ...session,
        entries: [
          ...session.entries,
          {
            id: newId("compaction"),
            kind: "compaction",
            agentName: event.agent_name,
            threadId: event.thread_id,
            content: "",
            status: "compacting",
            liveKey: compactionLiveKey(event.agent_name, event.thread_id),
          },
        ],
      };
    case "compaction_end": {
      if (event.agent_name !== rootName) {
        return session;
      }
      const failed = event.message.outcome.type === "failed";
      const [finished] = historyToEntries({ Compaction: event.message }, {}, {});
      const key = compactionLiveKey(event.agent_name, event.thread_id);
      const index = session.entries.findIndex((entry) => entry.liveKey === key);
      // Replace the pending shimmer entry in place when its `compaction_start`
      // was seen; otherwise append fresh (e.g. that event was chunk-tier and
      // got dropped under log pressure).
      const entries =
        index >= 0
          ? session.entries.map((entry, i) => (i === index ? finished : entry))
          : [...session.entries, ...(finished ? [finished] : [])];
      return addActivity(
        { ...session, entries },
        {
          tone: failed ? "warning" : "neutral",
          label: failed ? "compaction failed" : "context compacted",
          detail: event.agent_name,
        },
      );
    }
    case "suspended":
      return {
        ...addActivity(session, {
          tone: "warning",
          label: "approval required",
          detail: `${event.approval.calls.length} call(s) from ${event.agent_name}`,
        }),
        approvals: upsertApproval(session.approvals, event.approval),
        pendingCallInfo: withPendingCallInfo(session, event.approval.calls),
        running: false,
      };
    case "aborted": {
      const updated = addActivity(
        discardCompactionLiveEntry(
          finishLiveEntry(
            finishReasoning(session, event.agent_name, event.thread_id),
            event.agent_name,
            event.thread_id,
          ),
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
        // The root's abort settles the turn (mirroring the server's
        // `event_settles_turn`), and buries its pending approvals with it: a
        // decision for them has no thread left to wake, and keeping them
        // would hold the composer busy forever.
        ...(event.agent_name === rootName ? clearedApprovalState : {}),
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
        // A root error settles the turn like a root abort does; see above.
        ...(event.agent_name === rootName ? clearedApprovalState : {}),
      };
    }
    case "persist_failed":
      // Not a turn ending: `running` stays as it was, because the turn did not
      // finish — it failed to be recorded. The server drops the session right
      // after this, and the notice has to outlive that reattach, so it goes on
      // the session rather than into the transcript.
      return {
        ...addActivity(session, {
          tone: "danger",
          label: `${event.agent_name || "server"} could not save`,
          detail: event.message,
        }),
        persistError: event.message,
      };
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
      sessions: [
        { id: sessionId, name: null, updated_at_ms: null, has_pending_approval: false },
        ...workspace.sessions,
      ],
    };
  });
}

/** Overwrite fields of one session's catalog entry.
 *
 * The blunt instrument the rewind paths need, because `upsertCatalogTitled`
 * only ever *fills* a title (`?? title`). That is right for an optimistic first
 * turn racing the server, and wrong every time a rewind is involved: the message
 * the server derived its title from is the one that just went away, so the
 * stored value is the stale one and refusing to overwrite it is the bug. */
function patchCatalogSession(
  catalog: WorkspaceSummary[],
  workspaceId: string,
  sessionId: string,
  patch: Partial<WorkspaceSession>,
): WorkspaceSummary[] {
  return catalog.map((workspace) =>
    workspace.id !== workspaceId
      ? workspace
      : {
          ...workspace,
          sessions: workspace.sessions.map((session) =>
            session.id === sessionId ? { ...session, ...patch } : session,
          ),
        },
  );
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
        {
          id: sessionId,
          name: null,
          updated_at_ms: Date.now(),
          first_user_message: title,
          has_pending_approval: false,
        },
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
          // A tombstoned (deleting) session must not be re-added as an extra:
          // that is exactly the resurrection the delete-tombstone guards against.
          !session.deleting &&
          session.workspaceId === workspace.id &&
          Boolean(session.firstUserMessage) &&
          !present.has(session.sessionId),
      )
      .map((session) => ({
        id: session.sessionId,
        name: null,
        updated_at_ms: Date.now(),
        first_user_message: session.firstUserMessage ?? null,
        has_pending_approval: session.approvals.length > 0,
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

function setSocket(store: CodaStore, server: string, socket: WebSocket, rpc: CodaRpcClient) {
  updateState(store, (state) => {
    state.wsMap[server] = socket;
    state.rpcMap[server] = rpc;
  });
}

function closeSocket(store: CodaStore, server: string) {
  currentSocket(store, server)?.close();
}

function removeSocket(store: CodaStore, server: string) {
  updateState(store, (state) => {
    delete state.wsMap[server];
    delete state.rpcMap[server];
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

type SessionRestore = {
  session: OpenedSession;
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
    if (!current) {
      return;
    }
    current.catalog = mergeLocal ? mergeCatalog(workspaces, current.sessions) : workspaces;
    // Self-heals a dropped `session_status` push: every fetched row
    // reconciles a stale `running` in either direction.
    for (const workspace of workspaces) {
      for (const session of workspace.sessions) {
        reconcileOpenedSessionRunning(state, server, workspace.id, session.id, session.status);
      }
    }
  });
}

function applySessionName(
  store: CodaStore,
  server: string,
  workspaceId: string,
  sessionId: string,
  name: string | null,
) {
  updateState(store, (state) => {
    const current = state.servers[server];
    if (!current) {
      return;
    }
    const workspace = current.catalog.find((item) => item.id === workspaceId);
    const session = workspace?.sessions.find((item) => item.id === sessionId);
    if (session) {
      session.name = name;
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
          const seed = initialModelSelection(current, session.workspaceId);
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
    const seed = initialModelSelection(current, workspaceId);
    current.sessions[key] = {
      ...blankSession(workspaceId, sessionId),
      draft: true,
      providerId: seed.providerId,
      reasoningEffort: seed.reasoningEffort,
    };
  });
}

/** Mark (or unmark) a session as delete-in-flight. The flag both gates local
 * actions and survives a disconnect as a tombstone until the reconnect re-delete
 * settles it (Decision 9). */
function setSessionDeleting(store: CodaStore, server: string, key: SessionKey, deleting: boolean) {
  updateState(store, (state) => {
    const session = state.servers[server]?.sessions[key];
    if (session) {
      session.deleting = deleting;
    }
  });
}

function setSessionStarting(store: CodaStore, server: string, key: SessionKey, starting: boolean) {
  updateState(store, (state) => {
    const session = state.servers[server]?.sessions[key];
    if (session) {
      session.starting = starting;
    }
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
      const seed = initialModelSelection(current, workspaceId);
      session.providerId = seed.providerId;
      session.reasoningEffort = seed.reasoningEffort;
      // Same story for the posture, except it is remembered per session rather
      // than per workspace: switching in one conversation must not change what
      // the next one starts on. A session still live on the server overrides
      // this the moment its snapshot lands.
      //
      // What the session already carries is the fallback, not the hard default:
      // a fork was handed its parent's mode a moment ago, and storage being
      // unavailable must not silently downgrade that to the default.
      session.permissionMode = initialSessionMode(
        server,
        workspaceId,
        sessionId,
        session.permissionMode,
      );
    }
  });
}

/** A user message this client is showing ahead of the server's acknowledgement.
 *
 * Every other route to a user entry stamps the server's id on it — history
 * replay, a rewind's own append — and events never carry user messages at all,
 * so the missing id is exactly the "the server has not confirmed this yet"
 * marker, with no extra state to keep in step. A compaction's optimistic copy
 * carries the same missing id but is not a pending *task*: it must not make the
 * session look running, so it is excluded here and tracked separately. */
function isPendingUserEntry(entry: TranscriptEntry): boolean {
  return entry.kind === "user" && !entry.messageId && !entry.pendingCompact;
}

/** The optimistic copy of a `/compact` command. `compact` deliberately answers
 * without a message id, so this entry cannot be adopted like a task's; the
 * end-of-compaction snapshot records the very line it mirrors, and content
 * equality retires it there (`applySnapshotToSession`). */
function isPendingCompactionEntry(entry: TranscriptEntry): boolean {
  return entry.kind === "user" && !entry.messageId && entry.pendingCompact === true;
}

/** The text a user message carries in a snapshot, shared by history replay and
 * the content match that retires a pending compaction copy. */
function userMessageText(message: HistoryMessage): string {
  if (!("User" in message)) {
    return "";
  }
  return message.User.parts
    .filter((p) => p.type === "text")
    .map((p) => (p as { type: "text"; text: string }).text)
    .join("");
}

/** Everything a snapshot decides about one session.
 *
 * A snapshot is authoritative even when it is empty. That is only sound because
 * the server never serves a history that is missing a message the client
 * legitimately holds: a live entry appends its in-flight user message to the
 * snapshot it composes, and an entry that was released first waits out a
 * graceful shutdown, whose persisted history already contains that message —
 * the driver writes it on accepting the task, before generation. So an empty
 * snapshot means the session really is empty, and treating it as "no news"
 * would leave a rewound-away transcript on screen as if it still existed.
 *
 * Both halves of that premise are load-bearing and pinned server-side, in
 * `midturn_attach_replays_chunks_and_evicts_previous` and
 * `user_task_is_checkpointed_before_turn_completes`.
 *
 * What the server cannot know about is a task this client has sent but not yet
 * heard back on. That message exists only here until the reply lands, and the
 * event stream never carries user messages, so it is carried across explicitly
 * — `adoptServerMessageId` drops it again if the snapshot turns out to have had
 * it all along. */
export function applySnapshotToSession(
  session: OpenedSession,
  snapshot: {
    messages: HistoryMessage[];
    approvals: PendingApproval[];
    providerId: string;
    reasoningEffort: ReasoningEffort | null;
    permissionMode: PermissionMode;
    turnRunning: boolean;
    compacting?: boolean;
    backgroundTasks?: TaskSummary[];
  },
): OpenedSession {
  const argsById = collectToolArgs(snapshot.messages);
  const spansById = collectGenerationSpans(snapshot.messages);
  const pending = session.entries.filter(isPendingUserEntry);
  // A compaction's optimistic copy is reconciled by content: `compact` answers
  // without a message id, so the only thing that retires the copy is the
  // end-of-compaction snapshot carrying the recorded `/compact` line. The start
  // snapshot (compacting, nothing new yet) keeps it, the ending one replaces it
  // with the persisted entry.
  const recordedTexts = new Set(snapshot.messages.map(userMessageText));
  const pendingCompaction = session.entries.filter(
    (entry) =>
      isPendingCompactionEntry(entry) &&
      (snapshot.compacting === true || !recordedTexts.has(entry.content)),
  );
  // The composer no longer shows its own "compacting" indicator; this
  // transcript entry carries that state instead, then gives way to the real
  // "Context compacted" entry `historyToEntries` produces once it lands.
  const pendingCompactionStatus: TranscriptEntry[] = snapshot.compacting
    ? [
        {
          id: "compaction-pending",
          kind: "compaction",
          status: "compacting",
          content: "",
          startedAt: new Date().toISOString(),
        },
      ]
    : [];
  return {
    ...session,
    // Rebuilt, not merged: the snapshot is the whole history, so a span it
    // doesn't account for belongs to a call that no longer exists.
    generationSpans: spansById,
    draft: false,
    providerId: snapshot.providerId,
    reasoningEffort: snapshot.reasoningEffort,
    // The server's posture wins: attaching to a session that is still running
    // must show what it is *actually* executing under, not what this browser
    // remembered for it.
    permissionMode: snapshot.permissionMode,
    usage: historyUsage(snapshot.messages),
    // A snapshot means this client holds the session now (clearing any
    // eviction), and `turnRunning` says whether replayed events of an in-flight
    // turn follow.
    approvals: snapshot.approvals,
    // A snapshot older than the pending task says `turnRunning: false` about a
    // turn that is about to start. Taking it at face value reopens the composer
    // and lets a second task go out under the first one — so the pending
    // message speaks for its own turn until the reply that created it lands.
    running: snapshot.turnRunning || pending.length > 0,
    compacting: snapshot.compacting ?? false,
    backgroundTasks: snapshot.backgroundTasks ?? session.backgroundTasks,
    evicted: false,
    editing: reconcileEditing(session.editing, snapshot.messages),
    entries: [
      ...snapshot.messages.flatMap((message) => historyToEntries(message, argsById, spansById)),
      ...pending,
      ...pendingCompaction,
      ...pendingCompactionStatus,
    ],
    drafts: {},
    allowDrafts: {},
    // Same reasoning for the title: on a session whose very first task is the
    // pending one, the optimistic title is the only one there is, and nothing
    // downstream would restore it — the reply only carries an id.
    firstUserMessage:
      snapshot.messages.length === 0 && pending.length === 0 ? undefined : session.firstUserMessage,
  };
}

/** Streamed chunk events can arrive many times a second; coalesce them into
 * one store update per animation frame instead of one per event. rAF pauses
 * in a hidden/minimized tab, so a self-rescheduling timeout drains the queue
 * there instead — re-arming only when a new event needs it, not a standing
 * poll. Snapshot/eviction handlers (and `visibilitychange`, on refocus) flush
 * the FIFO queue first to stay ordered relative to a resync. */
const HIDDEN_FLUSH_INTERVAL_MS = 500;

type PendingEvent = {
  server: string;
  workspaceId: string;
  sessionId: string;
  event: WireEvent;
};

let pendingEvents: PendingEvent[] = [];
let rafHandle: number | undefined;
let hiddenTimeoutHandle: number | undefined;

function flushPendingEvents() {
  if (rafHandle !== undefined) {
    cancelAnimationFrame(rafHandle);
    rafHandle = undefined;
  }
  if (hiddenTimeoutHandle !== undefined) {
    window.clearTimeout(hiddenTimeoutHandle);
    hiddenTimeoutHandle = undefined;
  }
  if (pendingEvents.length === 0) {
    return;
  }
  const batch = pendingEvents;
  pendingEvents = [];
  updateState(codaStore, (state) => {
    for (const { server, workspaceId, sessionId, event } of batch) {
      const key = sessionKey(workspaceId, sessionId);
      const session = draftSession(state, server, key);
      if (session) {
        state.servers[server].sessions[key] = reduceEvent(session, event);
      }
    }
  });
}

function scheduleFlush() {
  if (typeof document !== "undefined" && document.hidden) {
    hiddenTimeoutHandle ??= window.setTimeout(flushPendingEvents, HIDDEN_FLUSH_INTERVAL_MS);
    return;
  }
  rafHandle ??= requestAnimationFrame(flushPendingEvents);
}

if (typeof document !== "undefined") {
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) {
      flushPendingEvents();
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
  permissionMode: PermissionMode,
  turnRunning: boolean,
  compacting: boolean,
  backgroundTasks: TaskSummary[],
) {
  flushPendingEvents();
  const key = sessionKey(workspaceId, sessionId);
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
    if (messages.length === 0 && !session.entries.some(isPendingUserEntry)) {
      // The session has no opening message left for a title to come from, so
      // the one in the list is describing something that no longer exists —
      // and `upsertCatalogTitled` would never overwrite it. Mirrors the
      // reducer's own condition: a pending first task still owns the title.
      // The timestamp is deliberately left alone — an empty snapshot is not by
      // itself evidence of a write, and plain opens produce them too.
      current.catalog = patchCatalogSession(current.catalog, workspaceId, sessionId, {
        first_user_message: null,
      });
    }
    current.sessions[key] = applySnapshotToSession(session, {
      messages,
      approvals,
      providerId,
      reasoningEffort,
      permissionMode,
      turnRunning,
      compacting,
      backgroundTasks,
    });
  });
  // Self-heal the memory: whatever the session is running under is what this
  // browser should reopen it on if the hub later closes it underneath us.
  rememberSessionMode(server, workspaceId, sessionId, permissionMode);
}

/** The session's task list changed. Its own push: tasks outlive turns, so
 * their comings and goings are not part of a turn's event stream. */
function applyBackgroundTasks(
  store: CodaStore,
  server: string,
  workspaceId: string,
  sessionId: string,
  tasks: TaskSummary[],
) {
  const key = sessionKey(workspaceId, sessionId);
  updateState(store, (state) => {
    const session = state.servers[server]?.sessions[key];
    if (session) {
      session.backgroundTasks = tasks;
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

/** Mark a session as held by another client — either we were evicted or an
 * open was refused as busy. Both land in the same read-only takeover state. */
function applyHeldElsewhere(
  store: CodaStore,
  server: string,
  workspaceId: string,
  sessionId: string,
  reason: "evicted" | "busy",
) {
  flushPendingEvents();
  const key = sessionKey(workspaceId, sessionId);
  updateState(store, (state) => {
    const session = draftSession(state, server, key);
    if (!session) {
      return;
    }
    session.evicted = true;
    session.running = false;
    state.servers[server].sessions[key] = addActivity(session, {
      tone: "warning",
      label: reason === "evicted" ? "session taken over" : "session in use",
      detail:
        reason === "evicted"
          ? "Another window took this session over."
          : "Another window is driving this session.",
    });
  });
}

/** Reconciles `running` to match a catalog row's `status` in *either*
 * direction — a one-directional version left a session that starts running
 * again from another tab stuck showing idle. `undefined` (no real catalog
 * data yet, e.g. a locally-synthesized `mergeCatalog` entry) is the only
 * value that leaves `running` untouched; an explicit `null` is treated as
 * confirmed-idle, same as a settled outcome. */
export function reconcileRunningWithStatus(
  session: OpenedSession,
  status: "running" | "completed" | "failed" | null | undefined,
): OpenedSession {
  if (status === undefined) {
    return session;
  }
  const running = status === "running";
  return session.running === running ? session : { ...session, running };
}

function reconcileOpenedSessionRunning(
  state: CodaDraft,
  server: string,
  workspaceId: string,
  sessionId: string,
  status: "running" | "completed" | "failed" | null | undefined,
) {
  const current = state.servers[server];
  const key = sessionKey(workspaceId, sessionId);
  const session = current?.sessions[key];
  if (current && session) {
    current.sessions[key] = reconcileRunningWithStatus(session, status);
  }
}

/** A session's turn just settled with nobody attached (`session_status` push).
 * A freshness optimization, not the only path to correctness — `setCatalog`
 * applies the same correction from the next `list_workspaces` fetch. */
function applySessionStatus(
  store: CodaStore,
  server: string,
  workspaceId: string,
  sessionId: string,
  outcome: "completed" | "failed",
) {
  flushPendingEvents();
  updateState(store, (state) => {
    const current = state.servers[server];
    if (!current) {
      return;
    }
    current.catalog = patchCatalogSession(current.catalog, workspaceId, sessionId, {
      status: outcome,
    });
    reconcileOpenedSessionRunning(state, server, workspaceId, sessionId, outcome);
  });
}

function applyEvent(server: string, workspaceId: string, sessionId: string, event: WireEvent) {
  pendingEvents.push({ server, workspaceId, sessionId, event });
  scheduleFlush();
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

/** The transcript key for a user message, from the id the server minted for it.
 * Shared by the optimistic path and the replayed-history path so one message
 * keeps one key. */
function userEntryId(messageId: string) {
  return `user:${messageId}`;
}

/** Where a rewind would cut: the index of the entry for `messageId`, or
 * `undefined` when the transcript no longer holds it. Entries are appended in
 * order, so everything from that index on is what the rewind discards. */
export function discardedFrom(entries: TranscriptEntry[], messageId: string): number | undefined {
  const index = entries.findIndex((entry) => entry.messageId === messageId);
  return index === -1 ? undefined : index;
}

/** The whole state transition a successful rewind performs.
 *
 * The edited message is appended here rather than waited for: the event stream
 * never carries user messages, so a client that applied only the surviving
 * history would show the assistant's reply hanging off the old conversation
 * with nothing to explain it. Usage is recomputed for the same reason a
 * snapshot recomputes it — the figure shown is the last assistant message's
 * running total, and the discarded ones have to stop counting. */
export function applyRewound(
  session: OpenedSession,
  payload: { messages: HistoryMessage[]; messageId: string; text: string; images: string[] },
): OpenedSession {
  const argsById = collectToolArgs(payload.messages);
  const spansById = collectGenerationSpans(payload.messages);
  const entries = payload.messages.flatMap((message) =>
    historyToEntries(message, argsById, spansById),
  );
  entries.push({
    id: userEntryId(payload.messageId),
    messageId: payload.messageId,
    kind: "user",
    content: payload.text,
    images: payload.images.length > 0 ? payload.images : undefined,
    startedAt: new Date().toISOString(),
  });
  return {
    ...session,
    entries,
    usage: historyUsage(payload.messages),
    approvals: [],
    drafts: {},
    allowDrafts: {},
    pendingCallInfo: {},
    generationSpans: spansById,
    running: true,
    // Cleared only here and in `rewindTurn`'s orphan branch — that is what takes
    // the composer out of edit mode, and emptying it is a side effect of the
    // remount that follows.
    editing: undefined,
    // Rewinding past the opening message makes the edited text the session's
    // first, which is the title the session list shows.
    firstUserMessage:
      payload.messages.length === 0 ? payload.text || IMAGE_ONLY_TITLE : session.firstUserMessage,
  };
}

/** Reconcile an edit in progress against a fresh snapshot.
 *
 * A rewind's reply can be lost — the connection drops between the server
 * committing and the client hearing about it — and this decides, from the
 * snapshot alone, which of the three states the session ended up in:
 *
 * - **Target survived.** Nothing was discarded; the edit stands untouched.
 * - **Target gone, no message took its place.** The truncation committed but
 *   the replacement turn did not start. The draft stays but stops naming a
 *   message, so the next submit is an ordinary task — which against this
 *   history is exactly the result the user asked for.
 * - **Target gone and one more user message than preceded it.** The whole
 *   rewind went through. The edit is done; anything left in the composer would
 *   be a second copy of a message that has already been sent and answered.
 *
 * A count settles the last two because the truncation leaves precisely the
 * messages before the target, and the only thing that can add one afterwards is
 * the replacement turn itself — no other turn can run while a rewind holds the
 * session. The id of that new message came back on the very reply that was
 * lost, so counting is the only handle the client has on it.
 *
 * A draft that is already an orphan runs the same count, and has to: it is
 * still one submit away from the same lost-reply window, and its baseline is
 * still the history the truncation left behind. `target === null` simply never
 * matches a message id, so it falls through to the count on its own. */
export function reconcileEditing(
  editing: OpenedSession["editing"],
  messages: HistoryMessage[],
): OpenedSession["editing"] {
  if (!editing) {
    return undefined;
  }
  // Whatever request this belonged to went down with the connection.
  const settled = { ...editing, submitting: false };
  let userMessages = 0;
  let survived = false;
  for (const message of messages) {
    if (!("User" in message)) {
      continue;
    }
    userMessages += 1;
    survived ||= message.User.message_id === settled.target;
  }
  if (survived) {
    return settled;
  }
  return userMessages > editing.precedingUserMessages ? undefined : { ...settled, target: null };
}

/** Render the user's message immediately, returning the id of the entry created
 * so the caller can reconcile it once the server answers with the real one. */
function appendUserMessage(
  store: CodaStore,
  server: string,
  key: SessionKey,
  content: string,
  images?: string[],
): string {
  const entryId = newId("user");
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
    delete session.forkDraft;
    session.entries.push({
      id: entryId,
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
  return entryId;
}

/** Re-key an optimistic user entry onto the id derived from the server's
 * `message_id`, so it matches the key the same message gets when replayed from
 * history.
 *
 * A snapshot that landed while the task was in flight may already have carried
 * this message — it is only kept across a snapshot because the client cannot
 * tell yet, and the id being adopted here is what finally settles it. Two
 * entries for one message is the worse failure, so the confirmed copy wins and
 * the optimistic one goes. */
export function adoptMessageId(
  entries: TranscriptEntry[],
  entryId: string,
  messageId: string,
): TranscriptEntry[] {
  const adoptedId = userEntryId(messageId);
  if (entries.some((entry) => entry.id === adoptedId)) {
    return entries.filter((entry) => entry.id !== entryId);
  }
  return entries.map((entry) =>
    entry.id === entryId ? { ...entry, id: adoptedId, messageId } : entry,
  );
}

function adoptServerMessageId(server: string, key: SessionKey, entryId: string, messageId: string) {
  updateState(codaStore, (state) => {
    const session = draftSession(state, server, key);
    if (session) {
      session.entries = adoptMessageId(session.entries, entryId, messageId);
    }
  });
}

/** Undo an optimistic user entry whose task never started. The session's title
 * and non-draft flag are left as they are — a later catalog refresh corrects
 * them, and unwinding them here could clobber newer server state. */
function discardOptimisticTask(
  server: string,
  key: SessionKey,
  entryId: string,
  previousRunning: boolean,
) {
  updateState(codaStore, (state) => {
    const session = draftSession(state, server, key);
    if (!session) {
      return;
    }
    session.entries = session.entries.filter((e) => e.id !== entryId);
    session.running = previousRunning;
  });
}

/** Show the `/compact` line immediately, like a task's user message, instead of
 * leaving the transcript silent across the summary round-trip. The copy is
 * marked `pendingCompact` so it neither sets `running` (a compaction is not a
 * turn — that would offer the abort button) nor claims a server id; the
 * end-of-compaction snapshot retires it by content, and a failed request drops
 * it outright. */
function appendCompactionMessage(
  store: CodaStore,
  server: string,
  key: SessionKey,
  text: string,
): string {
  const entryId = newId("user");
  updateState(store, (state) => {
    const session = draftSession(state, server, key);
    if (!session) {
      return;
    }
    session.entries.push({
      id: entryId,
      kind: "user",
      pendingCompact: true,
      content: text,
      startedAt: new Date().toISOString(),
    });
  });
  return entryId;
}

function discardPendingCompaction(server: string, key: SessionKey, entryId: string) {
  updateState(codaStore, (state) => {
    const session = draftSession(state, server, key);
    if (session) {
      session.entries = session.entries.filter((e) => e.id !== entryId);
    }
  });
}

/** Start a turn: show the user's message right away, then reconcile it with the
 * id the server minted. Rendering first costs a round trip of staleness on the
 * entry's key but avoids stalling the user's own message behind the ack.
 * Returns whether the turn started. */
async function startTurn(
  server: string,
  workspaceId: string,
  sessionId: string,
  text: string,
  images: string[],
): Promise<boolean> {
  const key = sessionKey(workspaceId, sessionId);
  const previousRunning = codaStore.getState().servers[server]?.sessions[key]?.running ?? false;
  const entryId = appendUserMessage(codaStore, server, key, text, images);
  const rpc = rpcFor(server);
  if (!rpc) {
    setServerStatus(codaStore, server, "error", "Connection closed");
    discardOptimisticTask(server, key, entryId, previousRunning);
    return false;
  }
  try {
    const { message_id } = await rpc.request("task", {
      workspace_id: workspaceId,
      session_id: sessionId,
      task: text,
      images: images.length > 0 ? images : undefined,
    });
    adoptServerMessageId(server, key, entryId, message_id);
    return true;
  } catch (err) {
    discardOptimisticTask(server, key, entryId, previousRunning);
    addSessionActivity(server, workspaceId, sessionId, {
      tone: "danger",
      label: "task rejected",
      detail: isServerError(err) ? err.message : "Connection lost before the task started",
    });
    return false;
  }
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

/**
 * Build `open_session` params, carrying the session's chosen model when it has
 * one (a draft seeds it locally) so the server opens on that provider rather
 * than the default.
 */
function openParams(session: OpenedSession, takeover = false) {
  return {
    workspace_id: session.workspaceId,
    session_id: session.sessionId,
    permission_mode: session.permissionMode,
    ...(takeover ? { takeover } : {}),
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

/** The JSON-RPC adapter for `server`'s current connection, if any. */
function rpcFor(server: string): CodaRpcClient | undefined {
  return codaStore.getState().rpcMap[server];
}

/** Fire a notification, surfacing a dropped connection as an error status.
 * Returns whether the frame was handed to the socket (Decision 10/11 gate). */
function notify<Method extends keyof RpcNotifications & string>(
  server: string,
  method: Method,
  params: RpcNotifications[Method],
): boolean {
  if (rpcFor(server)?.notify(method, params)) {
    return true;
  }
  setServerStatus(codaStore, server, "error", "Connection closed");
  return false;
}

/** Add an activity entry to a specific session (by key). */
function addSessionActivity(
  server: string,
  workspaceId: string,
  sessionId: string,
  entry: Omit<ActivityEntry, "id">,
) {
  const key = sessionKey(workspaceId, sessionId);
  updateState(codaStore, (state) => {
    const session = draftSession(state, server, key);
    if (session) {
      state.servers[server].sessions[key] = addActivity(session, entry);
    }
  });
}

/** Branch an `open_session` failure: a busy session drives the takeover UI; any
 * other server error shows an error activity. A dropped connection is ignored —
 * the reconnect restore handles it. */
function handleOpenError(server: string, workspaceId: string, sessionId: string, err: unknown) {
  if (!isServerError(err)) {
    return;
  }
  if (err.code === RpcCode.SESSION_BUSY) {
    applyHeldElsewhere(codaStore, server, workspaceId, sessionId, "busy");
    return;
  }
  addSessionActivity(server, workspaceId, sessionId, {
    tone: "danger",
    label: "open failed",
    detail: err.message,
  });
}

/** Surface a genuine server error from a connect-time catalog fetch, so an
 * empty sidebar / model selector is diagnosable instead of a silent degrade. A
 * dropped connection (code 0) is left to `onclose` → "closed"; only a real
 * fault flips the server to an error state. */
function reportCatalogFetchError(server: string, err: unknown, what: string) {
  if (isServerError(err)) {
    setServerStatus(codaStore, server, "error", `Failed to load ${what}: ${err.message}`);
  }
}

/** Send `open_session` and apply its snapshot result at the call site. The
 * unsolicited re-attach path applies the same reducer via the `snapshot` push.
 * Returns whether the session opened (so a caller can withhold a follow-up
 * task). */
async function requestOpenAndApply(
  server: string,
  session: OpenedSession,
  options: { takeover?: boolean } = {},
): Promise<boolean> {
  const rpc = rpcFor(server);
  if (!rpc) {
    setServerStatus(codaStore, server, "error", "Connection closed");
    return false;
  }
  try {
    const snap = await rpc.request("open_session", openParams(session, options.takeover));
    applySnapshot(
      codaStore,
      server,
      snap.workspace_id,
      snap.session_id,
      snap.messages,
      snap.pending_approvals ?? [],
      snap.provider_id,
      snap.reasoning_effort ?? null,
      snap.permission_mode ?? DEFAULT_PERMISSION_MODE,
      snap.turn_running ?? false,
      snap.compacting ?? false,
      snap.background_tasks ?? [],
    );
    return true;
  } catch (err) {
    handleOpenError(server, session.workspaceId, session.sessionId, err);
    return false;
  }
}

/** Open a draft session ahead of its first task, holding `starting` for the
 * round trip. That flag is the only thing standing between a second submit and
 * a duplicate turn: the composer's send gate keys on `running`, which nothing
 * sets until the task itself goes out. */
async function openBeforeFirstTask(server: string, session: OpenedSession): Promise<boolean> {
  setSessionStarting(codaStore, server, session.key, true);
  try {
    return await requestOpenAndApply(server, session);
  } finally {
    setSessionStarting(codaStore, server, session.key, false);
  }
}

/** Send a `delete_session` request and settle the tombstone on its outcome:
 * success removes the session (and reconciles the returned catalog); an explicit
 * server error clears the flag; a dropped connection keeps the tombstone for the
 * reconnect re-delete (Decision 9). */
function requestDelete(server: string, workspaceId: string, sessionId: string) {
  const key = sessionKey(workspaceId, sessionId);
  setSessionDeleting(codaStore, server, key, true);
  const rpc = rpcFor(server);
  if (!rpc) {
    return; // no connection: stays tombstoned until reconnect
  }
  rpc
    .request("delete_session", {
      workspace_id: workspaceId,
      session_id: sessionId,
    })
    .then((catalog) => {
      // Durable delete: merge the returned catalog (the tombstoned session is
      // skipped by `mergeCatalog`, so it isn't re-added as a local extra), then
      // drop the now-gone session.
      setCatalog(codaStore, server, catalog.workspaces, true);
      deleteSessionState(codaStore, server, key);
      forgetSessionMode(server, workspaceId, sessionId);
    })
    .catch((err) => {
      if (isServerError(err)) {
        // The delete definitively did not commit; return to normal.
        setSessionDeleting(codaStore, server, key, false);
        addSessionActivity(server, workspaceId, sessionId, {
          tone: "danger",
          label: "delete failed",
          detail: err.message,
        });
      }
      // else: dropped connection — keep the tombstone; reconnect re-deletes.
    });
}

/** On (re)connect, re-send `delete_session` for every tombstoned session so a
 * delete whose response was lost to a disconnect settles definitively (delete is
 * idempotent server-side). */
function resendPendingDeletes(server: string) {
  const current = codaStore.getState().servers[server];
  if (!current) {
    return;
  }
  for (const session of Object.values(current.sessions)) {
    if (session.deleting) {
      requestDelete(server, session.workspaceId, session.sessionId);
    }
  }
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

function activeSessionToRestore(server: string): SessionRestore | undefined {
  const snapshot = codaStore.getState();
  if (snapshot.activeServer !== server || !snapshot.activeKey) {
    return undefined;
  }
  const session = snapshot.servers[server]?.sessions[snapshot.activeKey];
  // An evicted session belongs to another client now; restoring it on a
  // transient reconnect would silently take it back — that must stay an explicit
  // user action ("Take over"). A tombstoned (deleting) session must not be
  // reopened either — that is the resurrection the delete-tombstone guards.
  return !session || session.draft || session.evicted || session.deleting
    ? undefined
    : { session, key: snapshot.activeKey };
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
  const rpc = createRpcClient<RpcRequests, RpcNotifications, RpcPushes>(socket);
  setSocket(codaStore, server, socket, rpc);

  // Server pushes (notifications) feed the existing reducers — the same reducer
  // whether a snapshot is solicited (an `open_session` result) or pushed here
  // (a hub re-attach).
  rpc.addMethod("event", (params) => {
    applyEvent(server, params.workspace_id, params.session_id, params.event);
  });
  rpc.addMethod("snapshot", (params) => {
    applySnapshot(
      codaStore,
      server,
      params.workspace_id,
      params.session_id,
      params.messages,
      params.pending_approvals ?? [],
      params.provider_id,
      params.reasoning_effort ?? null,
      params.permission_mode ?? DEFAULT_PERMISSION_MODE,
      params.turn_running ?? false,
      params.compacting ?? false,
      params.background_tasks ?? [],
    );
  });
  rpc.addMethod("background_tasks", (params) => {
    applyBackgroundTasks(codaStore, server, params.workspace_id, params.session_id, params.tasks);
  });
  rpc.addMethod("session_evicted", (params) => {
    applyHeldElsewhere(codaStore, server, params.workspace_id, params.session_id, "evicted");
  });
  rpc.addMethod("session_status", (params) => {
    applySessionStatus(codaStore, server, params.workspace_id, params.session_id, params.outcome);
  });

  socket.onopen = () => {
    setServerStatus(codaStore, server, "connected");
    // The client's own requests are the sole source of both catalogs.
    rpc
      .request("list_workspaces")
      .then((catalog) => setCatalog(codaStore, server, catalog.workspaces, false))
      .catch((err) => reportCatalogFetchError(server, err, "workspaces"));
    rpc
      .request("list_providers")
      .then((catalog) =>
        setProviderCatalog(codaStore, server, catalog.providers, catalog.default_provider),
      )
      .catch((err) => reportCatalogFetchError(server, err, "models"));
    if (sessionToRestore) {
      void requestOpenAndApply(server, sessionToRestore.session);
    }
    resendPendingDeletes(server);
  };
  socket.onclose = () => {
    // Reject any awaiting request so no caller hangs; a delete in flight keeps
    // its tombstone (the rejection carries the dropped-connection code).
    rpc.rejectAll("connection closed");
    if (currentSocket(codaStore, server) === socket) {
      setServerStatus(codaStore, server, "closed");
    }
  };
  socket.onerror = () => setServerStatus(codaStore, server, "error", "WebSocket connection failed");
  socket.onmessage = (event: MessageEvent<string>) => {
    try {
      rpc.receive(JSON.parse(event.data));
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

export async function renameSession(
  server: string,
  workspaceId: string,
  sessionId: string,
  rawName: string,
): Promise<void> {
  const workspace = workspaceId.trim();
  const session = sessionId.trim();
  if (!server || !workspace || !session) {
    throw new Error("Invalid session");
  }
  const name = rawName.trim();
  const invalid =
    [...name].length > 120 ||
    [...name].some((character) => {
      const codePoint = character.codePointAt(0) ?? 0;
      return (
        codePoint <= 0x1f ||
        (codePoint >= 0x7f && codePoint <= 0x9f) ||
        codePoint === 0x2028 ||
        codePoint === 0x2029
      );
    });
  if (invalid) {
    throw new Error("Session name must be a single line and at most 120 characters");
  }

  const rpc = rpcFor(server);
  if (!rpc) {
    throw new Error("Connection closed");
  }

  try {
    const result = await rpc.request("rename_session", {
      workspace_id: workspace,
      session_id: session,
      name: name || null,
    });
    applySessionName(codaStore, server, workspace, session, result.name);
  } catch (err) {
    const serverError = isServerError(err);
    if (
      serverError &&
      (err.code === RpcCode.SESSION_NOT_FOUND || err.code === RpcCode.UNKNOWN_WORKSPACE)
    ) {
      void rpc
        .request("list_workspaces")
        .then((catalog) => setCatalog(codaStore, server, catalog.workspaces, false))
        .catch((refreshError) => reportCatalogFetchError(server, refreshError, "workspaces"));
    }
    throw new Error(
      serverError ? err.message : "Connection lost before the session name was saved",
    );
  }
}

export function deleteSession(server: string, workspaceId: string, sessionId: string) {
  const workspace = workspaceId.trim();
  const session = sessionId.trim();
  if (!server || !workspace || !session) {
    return;
  }
  const key = sessionKey(workspace, session);
  const local = codaStore.getState().servers[server]?.sessions[key];
  // A draft was never opened on the server; drop it locally and be done.
  if (local?.draft) {
    deleteSessionState(codaStore, server, key);
    return;
  }
  // Already in flight / tombstoned: don't send a duplicate.
  if (local?.deleting) {
    return;
  }
  if (local?.compacting) {
    return;
  }
  // Mark deleting and remove only once the server confirms a durable delete;
  // no optimistic removal (no phantom deletion, and no resurrection if the
  // response is lost to a disconnect — Decision 9).
  requestDelete(server, workspace, session);
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
  notify(server, "close_session", {
    workspace_id: session.workspaceId,
    session_id: session.sessionId,
  });
}

/** Identifies the *source* session of a fork, which is what the in-flight flag
 * is scoped to — forking two different sessions at once is fine. */
export function forkKey(server: string, workspaceId: string, sessionId: string) {
  return `${server}|${sessionKey(workspaceId, sessionId)}`;
}

function setForking(key: string, inFlight: boolean) {
  updateState(codaStore, (state) => {
    if (inFlight) {
      state.forking[key] = true;
    } else {
      delete state.forking[key];
    }
  });
}

/**
 * Copy a session into a new one and switch to it. `cutMessageId` names the user
 * message to branch away from — the copy keeps the turns before it — and
 * `forkDraft` is that message, handed to the copy's composer. Omitting both
 * copies everything stored.
 *
 * Throws on failure, with the reason as its message — a fork that mints nothing
 * has to say so where the user clicked, since nothing renders the activity log.
 * A second call while one is in flight for the same source is dropped rather
 * than queued: the server mints an id per request, so it would leave a spare
 * copy behind.
 */
export async function forkSession(
  server: string,
  workspaceId: string,
  sessionId: string,
  cutMessageId?: string,
  forkDraft?: { text: string; images: string[] },
): Promise<void> {
  const key = forkKey(server, workspaceId, sessionId);
  const source = codaStore.getState().servers[server]?.sessions[sessionKey(workspaceId, sessionId)];
  if (codaStore.getState().forking[key] || source?.compacting) {
    return;
  }
  const params = {
    workspace_id: workspaceId,
    session_id: sessionId,
    ...(cutMessageId ? { cut_message_id: cutMessageId } : {}),
  };
  setForking(key, true);
  try {
    const rpc = rpcFor(server);
    if (!rpc) {
      throw new Error("Connection closed");
    }
    const forked = await rpc.request("fork_session", params);
    setCatalog(codaStore, server, forked.workspaces, true);
    // The copy inherits the source's posture the same way it inherits its
    // history — the fork is a continuation, and starting it on a different
    // footing than the conversation it branched from would be a surprise.
    //
    // Read it from the live session rather than from storage: the store holds
    // the value the server last confirmed, and a blocked or full localStorage
    // fails silently, which would quietly open the fork on the default. Storage
    // is only the fallback for a source this client has not opened.
    const source =
      codaStore.getState().servers[server]?.sessions[sessionKey(workspaceId, sessionId)];
    const inheritedMode =
      source?.permissionMode ?? initialSessionMode(server, workspaceId, sessionId);
    const copyKey = sessionKey(workspaceId, forked.session_id);
    // Both channels, because either one alone can drop it: the store carries it
    // into the open that follows even with storage unavailable, and storage
    // carries it across a reload.
    updateState(codaStore, (state) => {
      const copy = draftSession(state, server, copyKey);
      if (copy) {
        copy.permissionMode = inheritedMode;
      }
    });
    rememberSessionMode(server, workspaceId, forked.session_id, inheritedMode);
    openSession(server, workspaceId, forked.session_id);
    if (forkDraft) {
      updateState(codaStore, (state) => {
        const copy = draftSession(state, server, copyKey);
        if (copy) {
          copy.forkDraft = forkDraft;
        }
      });
    }
  } catch (err) {
    const detail = isServerError(err) ? err.message : "Connection lost before the fork started";
    addSessionActivity(server, workspaceId, sessionId, {
      tone: "danger",
      label: "fork failed",
      detail,
    });
    throw new Error(detail);
  } finally {
    setForking(key, false);
  }
}

/** Whether a fork is in flight for `key`, so every entry into it can go quiet
 * together rather than each tracking its own click. */
export const selectForking = (key: string) => (state: CodaStoreState) =>
  Boolean(state.forking[key]);

/** The active session's fork key, for the transcript's entries. */
export const selectActiveForkKey = (state: CodaStoreState) => {
  const session = activeSessionOf(state);
  return session && state.activeServer && !session.draft
    ? forkKey(state.activeServer, session.workspaceId, session.sessionId)
    : undefined;
};

/** Fork the active session — the transcript's entry point. */
export async function forkActiveSession(
  cutMessageId?: string,
  forkDraft?: { text: string; images: string[] },
): Promise<void> {
  const active = currentActive();
  // A draft was never opened on the server, so there is nothing to copy.
  if (!active || active.session.draft || active.session.compacting) {
    return;
  }
  const { workspaceId, sessionId } = active.session;
  await forkSession(active.server, workspaceId, sessionId, cutMessageId, forkDraft);
}

/** Persist edits to a fork's prefilled prompt so switching sessions does not
 * restore the original seed over the user's draft. */
export function updateForkDraft(server: string, key: SessionKey, text: string, images: string[]) {
  updateState(codaStore, (state) => {
    const draft = draftSession(state, server, key);
    if (draft?.forkDraft) {
      draft.forkDraft = { text, images };
    }
  });
}

export function openSession(server: string, workspaceId: string, sessionId: string) {
  const workspace = workspaceId.trim();
  const session = sessionId.trim();
  if (!server || !workspace || !session) {
    return;
  }
  const key = sessionKey(workspace, session);
  const local = codaStore.getState().servers[server]?.sessions[key];
  // A session mid-delete must not be re-opened (resurrection vector).
  if (local?.deleting) {
    return;
  }
  closeActiveSession(server, key);
  selectSession(codaStore, server, workspace, session);
  // Optimistic mirror of the server's clear-on-attach; skip when the catalog
  // already says "running", since attach doesn't change that.
  updateState(codaStore, (state) => {
    const current = state.servers[server];
    if (!current) {
      return;
    }
    const workspaceSummary = current.catalog.find((item) => item.id === workspace);
    const sessionSummary = workspaceSummary?.sessions.find((item) => item.id === session);
    if (sessionSummary?.status === "running") {
      return;
    }
    current.catalog = patchCatalogSession(current.catalog, workspace, session, {
      status: null,
    });
  });
  if (!local?.draft) {
    const opened = codaStore.getState().servers[server]?.sessions[key];
    if (opened) {
      void requestOpenAndApply(server, opened);
    }
  }
}

/** Re-open the active session with an explicit takeover, taking it from
 * whichever client currently drives it (the server evicts them and replays
 * the in-flight turn to us). */
export function takeOverActiveSession() {
  const active = currentActive();
  if (!active || active.session.draft) {
    return;
  }
  void requestOpenAndApply(active.server, active.session, { takeover: true });
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

export async function sendTask(task: string, images: string[] = []) {
  const text = task.trim();
  const active = currentActive();
  if (!text && images.length === 0) {
    return;
  }
  if (!active || active.session.deleting || active.session.starting) {
    return;
  }
  if (active.session.running || active.session.compacting || active.session.approvals.length > 0) {
    return;
  }
  // A draft/new session must be live before its first task, or the task would
  // come back `SESSION_NOT_LIVE` while the UI already showed it running
  // (Decision 10).
  if (active.session.draft && !(await openBeforeFirstTask(active.server, active.session))) {
    return;
  }
  await startTurn(
    active.server,
    active.session.workspaceId,
    active.session.sessionId,
    text,
    images,
  );
}

/** Ask the server to compact the active persisted session. The user's `/compact`
 * line is shown immediately, like a task's message; the compaction itself stays
 * snapshot-driven — the response only reports the outcome, and the recorded
 * line arrives with the end-of-compaction snapshot, which retires the
 * optimistic copy by content. */
export async function compactActiveSession(instructions: string): Promise<void> {
  const active = currentActive();
  if (
    !active ||
    active.session.draft ||
    active.session.deleting ||
    active.session.starting ||
    active.session.evicted ||
    active.session.running ||
    active.session.compacting ||
    active.session.approvals.length > 0 ||
    active.session.editing
  ) {
    return;
  }
  const { server, session } = active;
  if (session.entries.length === 0) {
    addSessionActivity(server, session.workspaceId, session.sessionId, {
      tone: "warning",
      label: "nothing to compact",
      detail: "The conversation is empty.",
    });
    return;
  }
  const rpc = rpcFor(server);
  if (!rpc) {
    setServerStatus(codaStore, server, "error", "Connection closed");
    return;
  }
  const key = sessionKey(session.workspaceId, session.sessionId);
  const text = instructions ? `/compact ${instructions}` : "/compact";
  const entryId = appendCompactionMessage(codaStore, server, key, text);
  try {
    const result = await rpc.request("compact", {
      workspace_id: session.workspaceId,
      session_id: session.sessionId,
      ...(instructions ? { instructions } : {}),
    });
    if (result.outcome === "applied") {
      addSessionActivity(server, session.workspaceId, session.sessionId, {
        tone: "success",
        label: "context compacted",
        detail: "Future turns will continue from the new summary.",
      });
    } else if (result.outcome === "recorded") {
      addSessionActivity(server, session.workspaceId, session.sessionId, {
        tone: "warning",
        label: "compaction failed",
        detail: "The conversation remains unchanged; the reason is in the transcript.",
      });
    } else if (result.outcome === "empty") {
      // Nothing was written, so the optimistic line has nothing to correspond
      // to — drop it rather than leave a phantom message.
      discardPendingCompaction(server, key, entryId);
      addSessionActivity(server, session.workspaceId, session.sessionId, {
        tone: "warning",
        label: "nothing to compact",
        detail: "The conversation is empty.",
      });
    } else {
      discardPendingCompaction(server, key, entryId);
      addSessionActivity(server, session.workspaceId, session.sessionId, {
        tone: result.stale ? "warning" : "danger",
        label: "compaction not applied",
        detail: result.reason,
      });
    }
  } catch (err) {
    discardPendingCompaction(server, key, entryId);
    addSessionActivity(server, session.workspaceId, session.sessionId, {
      tone: "danger",
      label: "compaction rejected",
      detail: isServerError(err)
        ? err.message
        : "Connection lost before compaction completed; reconnect to check its state.",
    });
  }
}

export async function sendTaskToNewSession(
  server: string,
  workspaceId: string,
  task: string,
  providerId?: string,
  reasoningEffort: ReasoningEffort | null = null,
  images: string[] = [],
  permissionMode: PermissionMode = DEFAULT_PERMISSION_MODE,
) {
  const workspace = workspaceId.trim();
  const text = task.trim();
  if (!server || !workspace || (!text && images.length === 0)) {
    return;
  }
  if (images.length === 0 && parseCompactCommand(text) !== null) {
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
  setSessionMode(codaStore, server, key, permissionMode);
  rememberSessionMode(server, workspace, sessionId, permissionMode);
  const session = codaStore.getState().servers[server]?.sessions[key];
  if (!session) {
    return;
  }
  // Open the new session first, then send the task only if it opened.
  if (!(await openBeforeFirstTask(server, session))) {
    return;
  }
  await startTurn(server, workspace, sessionId, text, images);
}

/** Pull a historical message back into the composer to be rewritten. Only from
 * a session at rest: a rewind discards messages, and everything the session has
 * in flight is downstream of what would go. */
export function beginEdit(messageId: string) {
  const active = currentActive();
  if (!active) {
    return;
  }
  const { server, session } = active;
  if (
    session.running ||
    session.compacting ||
    session.starting ||
    session.evicted ||
    session.deleting
  ) {
    return;
  }
  if (session.approvals.length > 0 || session.editing?.submitting) {
    return;
  }
  const index = session.entries.findIndex((item) => item.messageId === messageId);
  if (index === -1) {
    return;
  }
  const entry = session.entries[index];
  if (parseCompactCommand(entry.content) !== null) {
    return;
  }
  const key = sessionKey(session.workspaceId, session.sessionId);
  updateState(codaStore, (state) => {
    const draft = draftSession(state, server, key);
    if (draft) {
      draft.editing = {
        target: messageId,
        text: entry.content,
        images: entry.images ?? [],
        submitting: false,
        // What the history is about to be truncated to. The session is at rest
        // here, so every user entry already carries its server id and this
        // matches the user-message count the server would report.
        precedingUserMessages: session.entries
          .slice(0, index)
          .filter((item) => item.kind === "user").length,
      };
    }
  });
}

export function cancelEdit() {
  const active = currentActive();
  if (!active || active.session.editing?.submitting) {
    return;
  }
  const { server, session } = active;
  const key = sessionKey(session.workspaceId, session.sessionId);
  updateState(codaStore, (state) => {
    const draft = draftSession(state, server, key);
    if (draft) {
      draft.editing = undefined;
    }
  });
}

/** Submit whatever is in the composer while an edit is open.
 *
 * Owns the whole lifecycle for both branches, because clearing `editing` is the
 * only thing that takes the composer out of edit mode and nothing else knows to
 * do it: `applyRewound` covers the rewind branch, and the orphan branch has to
 * clear it here — `startTurn` has never heard of edit mode. Miss either and the
 * message goes out while its text stays in the box, ready to be sent again. */
export async function rewindTurn(task: string, images: string[] = []) {
  const active = currentActive();
  if (!active) {
    return;
  }
  const { server, session } = active;
  const editing = session.editing;
  if (!editing || editing.submitting) {
    return;
  }
  const text = task.trim();
  if (!text && images.length === 0) {
    return;
  }
  const { workspaceId, sessionId } = session;
  const key = sessionKey(workspaceId, sessionId);
  // Write the input back before sending. From here `editing` holds the draft,
  // not the seed it started as, so the composer can be remounted — by the
  // orphan downgrade below, or by anything else — and come back with what the
  // user actually typed.
  updateState(codaStore, (state) => {
    const draft = draftSession(state, server, key);
    if (draft?.editing) {
      draft.editing.text = text;
      draft.editing.images = images;
      draft.editing.submitting = true;
    }
  });

  if (editing.target === null) {
    // The history already stops at the rewind point, so this is a plain turn.
    const started = await startTurn(server, workspaceId, sessionId, text, images);
    updateState(codaStore, (state) => {
      const draft = draftSession(state, server, key);
      if (!draft?.editing) {
        return;
      }
      if (started) {
        draft.editing = undefined;
      } else {
        draft.editing.submitting = false;
      }
    });
    return;
  }

  const rpc = rpcFor(server);
  if (!rpc) {
    setServerStatus(codaStore, server, "error", "Connection closed");
    updateState(codaStore, (state) => {
      const draft = draftSession(state, server, key);
      if (draft?.editing) {
        draft.editing.submitting = false;
      }
    });
    return;
  }
  try {
    const { message_id, messages } = await rpc.request("rewind", {
      workspace_id: workspaceId,
      session_id: sessionId,
      message_id: editing.target,
      task: text,
      images: images.length > 0 ? images : undefined,
    });
    updateState(codaStore, (state) => {
      const current = state.servers[server];
      const draft = draftSession(state, server, key);
      if (!current || !draft) {
        return;
      }
      current.sessions[key] = applyRewound(draft, {
        messages,
        messageId: message_id,
        text,
        images,
      });
      // A rewind is a write like any other, so the list has to reorder for it
      // the way it does for a turn — which `appendUserMessage` gets for free
      // and a rewind, appending its message itself, does not. And when nothing
      // survived, the edited message is the session's first, so the title the
      // list shows has to follow it.
      current.catalog = patchCatalogSession(current.catalog, workspaceId, sessionId, {
        updated_at_ms: Date.now(),
        ...(messages.length === 0 ? { first_user_message: text || IMAGE_ONLY_TITLE } : {}),
      });
    });
  } catch (err) {
    // Only `submitting` moves. When the truncation had already committed the
    // server also pushes `Closed`, and the re-attach that follows runs
    // `reconcileEditing`; the two updates arrive in either order and commute
    // precisely because this one leaves `target` alone.
    updateState(codaStore, (state) => {
      const draft = draftSession(state, server, key);
      if (draft?.editing) {
        draft.editing.submitting = false;
      }
    });
    addSessionActivity(server, workspaceId, sessionId, {
      tone: "danger",
      label: "rewind rejected",
      detail: isServerError(err) ? err.message : "Connection lost before the session was rewound",
    });
  }
}

export function abort() {
  const active = currentActive();
  if (active) {
    notify(active.server, "abort", {
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

/** Dismiss the "the last turn was not saved" notice. The turn stays missing —
 * this only stops saying so. */
export function dismissPersistError() {
  const active = currentActive();
  if (!active) {
    return;
  }
  updateState(codaStore, (state) => {
    const session = draftSession(state, active.server, active.session.key);
    if (session) {
      session.persistError = undefined;
    }
  });
}

export function setModel(providerId: string, reasoningEffort: ReasoningEffort | null) {
  const active = currentActive();
  if (!active) {
    return;
  }
  if (active.session.draft) {
    setSessionModel(codaStore, active.server, active.session.key, providerId, reasoningEffort);
    rememberModelSelection(active.server, active.session.workspaceId, providerId, reasoningEffort);
    return;
  }
  if (active.session.deleting || active.session.compacting) {
    return;
  }
  const rpc = rpcFor(active.server);
  if (!rpc) {
    setServerStatus(codaStore, active.server, "error", "Connection closed");
    return;
  }
  const { server, session } = active;
  // Apply the switch on the server's confirmation (the selector reads the
  // session's stored model, which stays put until then — so an error needs no
  // explicit revert).
  rpc
    .request("set_model", {
      workspace_id: session.workspaceId,
      session_id: session.sessionId,
      provider_id: providerId,
      reasoning_effort: reasoningEffort,
    })
    .then((result) => {
      setSessionModel(
        codaStore,
        server,
        session.key,
        result.provider_id,
        result.reasoning_effort ?? null,
      );
      rememberModelSelection(
        server,
        session.workspaceId,
        result.provider_id,
        result.reasoning_effort ?? null,
      );
    })
    .catch((err) => {
      if (isServerError(err)) {
        addSessionActivity(server, session.workspaceId, session.sessionId, {
          tone: "warning",
          label: "model change failed",
          detail: err.message,
        });
      }
      // else: dropped connection — the reconnect restore re-syncs the model.
    });
}

function setSessionMode(store: CodaStore, server: string, key: SessionKey, mode: PermissionMode) {
  updateState(store, (state) => {
    const session = draftSession(state, server, key);
    if (session) {
      session.permissionMode = mode;
    }
  });
}

/**
 * Change the active session's posture.
 *
 * Applied locally first so drafts carry the choice into `open_session` and a
 * dropped connection can retry it while reconnecting. A live server may still
 * reject a stale session, in which case the optimistic choice is rolled back.
 */
export function setPermissionMode(mode: PermissionMode) {
  const active = currentActive();
  if (!active || active.session.deleting || active.session.evicted) {
    return;
  }
  const { server, session } = active;
  const previousMode = session.permissionMode;
  setSessionMode(codaStore, server, session.key, mode);
  rememberSessionMode(server, session.workspaceId, session.sessionId, mode);
  if (session.draft) {
    return;
  }
  const rpc = rpcFor(server);
  if (!rpc) {
    setServerStatus(codaStore, server, "error", "Connection closed");
    return;
  }
  rpc
    .request("set_permission_mode", {
      workspace_id: session.workspaceId,
      session_id: session.sessionId,
      mode,
    })
    .catch((err) => {
      if (isServerError(err)) {
        // Do not let an older failed request overwrite a choice made while it
        // was in flight.
        const current = codaStore.getState().servers[server]?.sessions[session.key];
        if (current?.permissionMode === mode) {
          setSessionMode(codaStore, server, session.key, previousMode);
          rememberSessionMode(server, session.workspaceId, session.sessionId, previousMode);
        }
        addSessionActivity(server, session.workspaceId, session.sessionId, {
          tone: "warning",
          label: "permission change failed",
          detail: err.message,
        });
      }
      // else: dropped connection — the reconnect re-opens with the remembered
      // mode, and the snapshot it answers with is what the UI then shows.
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

/**
 * Ranked workspace files for the composer's `@` picker.
 *
 * Nothing is cached here: the server ranks against a walk it reuses for a few
 * seconds, so a keystroke costs a round trip rather than a re-walk, and a file
 * created mid-conversation still turns up. Failures throw so the menu can say
 * why it is empty instead of implying the workspace is.
 */
export async function fetchWorkspaceFiles(
  server: string,
  workspaceId: string,
  query: string,
): Promise<{ files: WorkspaceFile[]; truncated: boolean }> {
  const rpc = rpcFor(server);
  if (!rpc) {
    throw new Error("Connection closed");
  }
  try {
    return await rpc.request("list_files", { workspace_id: workspaceId, query });
  } catch (err) {
    throw new Error(isServerError(err) ? err.message : "Connection closed");
  }
}

/** The workspace's skills for the composer's `/` picker, in the order the server
 * lists them (by name). */
export async function fetchWorkspaceSkills(
  server: string,
  workspaceId: string,
): Promise<SkillInfo[]> {
  const rpc = rpcFor(server);
  if (!rpc) {
    throw new Error("Connection closed");
  }
  try {
    const catalog = await rpc.request("list_skills", { workspace_id: workspaceId });
    return catalog.skills;
  } catch (err) {
    throw new Error(isServerError(err) ? err.message : "Connection closed");
  }
}

/** Approvals whose submit is in flight, by `${server}|${sessionKey}|${approvalKey}`.
 *
 * A submit awaits its staged allow-pattern writes before the resume goes out,
 * and the approval only leaves the store once it has — so a second click in
 * that window sends the same decision twice. The duplicate lands on whatever
 * the thread suspended on *next*, naming none of the calls parked there. Module
 * state rather than store state: it guards one in-flight send, and the panel
 * already goes away with the approval it submitted. */
const submittingApprovals = new Set<string>();

export async function submitApprovals() {
  const active = currentActive();
  if (!active) {
    return;
  }
  // Defense in depth behind the takeover mask: an evicted tab's approvals are
  // a stale snapshot — resumes would be rejected server-side, but the staged
  // allow-pattern writes would not, so nothing may be sent from here.
  if (active.session.evicted) {
    return;
  }
  const { server, session } = active;
  const rpc = rpcFor(server);
  for (const approval of session.approvals) {
    const approvalId = approvalKey(approval);
    const draft = session.drafts[approvalId] ?? {};
    const complete = approval.calls.every((item) => draft[item.id]);
    if (!complete) {
      continue;
    }
    const inFlight = `${server}|${session.key}|${approvalId}`;
    if (submittingApprovals.has(inFlight)) {
      continue;
    }
    submittingApprovals.add(inFlight);
    try {
      // Persist staged "always allow" patterns for approved calls only. This is
      // best-effort and must never block the resume: gather the writes with
      // `allSettled` (a rejection only logs a non-fatal activity) (Decision 11).
      const allow = session.allowDrafts[approvalId] ?? {};
      const allowWrites: Promise<unknown>[] = [];
      if (rpc) {
        for (const item of approval.calls) {
          const pattern = allow[item.id];
          if (pattern && draft[item.id] === "Execute") {
            allowWrites.push(
              rpc
                .request("add_allow_pattern", { workspace_id: session.workspaceId, pattern })
                .then(() =>
                  addAllowResultActivity(codaStore, server, session.workspaceId, pattern, null),
                )
                .catch((err) =>
                  addAllowResultActivity(
                    codaStore,
                    server,
                    session.workspaceId,
                    pattern,
                    isServerError(err) ? err.message : "connection closed",
                  ),
                ),
            );
          }
        }
      }
      await Promise.allSettled(allowWrites);
      // Clear the approval + allow drafts only when the resume actually left the
      // client, so a disconnect leaves them intact for retry (Decision 11).
      if (
        notify(server, "resume", {
          workspace_id: session.workspaceId,
          session_id: session.sessionId,
          agent_name: approval.agent_name,
          thread_id: approval.thread_id,
          decision: {
            parent_message_id: approval.parent_message_id,
            resolutions: approval.calls.map((item) => [item.id, draft[item.id]]),
          },
        })
      ) {
        clearApprovalState(codaStore, server, session.key, approval);
      }
    } finally {
      submittingApprovals.delete(inFlight);
    }
  }
}

// --- Selectors ---------------------------------------------------------------
// Stable empties so default-valued selectors keep referential identity and
// don't force re-renders under `useSyncExternalStore`.

const EMPTY_BACKGROUND_TASKS: TaskSummary[] = [];
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

export type SessionListSessionState = {
  running: boolean;
  approvalCount: number;
};

export type SessionListServer = Pick<ServerState, "url" | "alias" | "status" | "catalog"> & {
  sessions: Record<SessionKey, SessionListSessionState>;
};

let cachedSessionListServers: SessionListServer[] = [];

function sessionListServerMatches(cached: SessionListServer, server: ServerState): boolean {
  if (
    cached.url !== server.url ||
    cached.alias !== server.alias ||
    cached.status !== server.status ||
    cached.catalog !== server.catalog
  ) {
    return false;
  }

  const sessionKeys = Object.keys(server.sessions) as SessionKey[];
  if (sessionKeys.length !== Object.keys(cached.sessions).length) {
    return false;
  }
  return sessionKeys.every((key) => {
    const current = server.sessions[key];
    const previous = cached.sessions[key];
    return (
      previous?.running === current.running && previous.approvalCount === current.approvals.length
    );
  });
}

/**
 * The session list deliberately excludes transcripts and other live turn data.
 * Streaming chunks can then update the active session without rebuilding the
 * expanded sidebar tree on every token.
 */
export const selectSessionListServers = (state: CodaStoreState): SessionListServer[] => {
  if (
    state.order.length === cachedSessionListServers.length &&
    state.order.every((url, index) => {
      const server = state.servers[url];
      return Boolean(server) && sessionListServerMatches(cachedSessionListServers[index], server);
    })
  ) {
    return cachedSessionListServers;
  }

  const previousByUrl = new Map(cachedSessionListServers.map((server) => [server.url, server]));
  cachedSessionListServers = state.order.flatMap((url) => {
    const server = state.servers[url];
    if (!server) {
      return [];
    }
    const previous = previousByUrl.get(url);
    if (previous && sessionListServerMatches(previous, server)) {
      return [previous];
    }
    return [
      {
        url: server.url,
        alias: server.alias,
        status: server.status,
        catalog: server.catalog,
        sessions: Object.fromEntries(
          Object.entries(server.sessions).map(([key, session]) => [
            key,
            { running: session.running, approvalCount: session.approvals.length },
          ]),
        ) as Record<SessionKey, SessionListSessionState>,
      },
    ];
  });
  return cachedSessionListServers;
};

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
export const selectActiveBackgroundTasks = (state: CodaStoreState) =>
  activeSessionOf(state)?.backgroundTasks ?? EMPTY_BACKGROUND_TASKS;
/** How many of the active session's tasks are still running — the badge on the
 * panel trigger, and the reason to show it at all. */
export const selectActiveRunningTaskCount = (state: CodaStoreState) =>
  (activeSessionOf(state)?.backgroundTasks ?? EMPTY_BACKGROUND_TASKS).filter((task) => task.running)
    .length;
export const selectActiveCompacting = (state: CodaStoreState) =>
  activeSessionOf(state)?.compacting ?? false;
export const selectActiveStarting = (state: CodaStoreState) =>
  activeSessionOf(state)?.starting ?? false;
export const selectActiveEvicted = (state: CodaStoreState) =>
  activeSessionOf(state)?.evicted ?? false;
export const selectActivePersistError = (state: CodaStoreState) =>
  activeSessionOf(state)?.persistError;
export const selectActiveApprovals = (state: CodaStoreState) =>
  activeSessionOf(state)?.approvals ?? EMPTY_APPROVALS;
export const selectActiveDrafts = (state: CodaStoreState) =>
  activeSessionOf(state)?.drafts ?? EMPTY_DRAFTS;
export const selectActiveAllowDrafts = (state: CodaStoreState) =>
  activeSessionOf(state)?.allowDrafts ?? EMPTY_ALLOW_DRAFTS;
export const selectActiveApprovalCount = (state: CodaStoreState) =>
  activeSessionOf(state)?.approvals.length ?? 0;
export const selectActiveWorkspace = (state: CodaStoreState) => activeSessionOf(state)?.workspaceId;
/** Derived title of the active persisted session for the header breadcrumb. */
export const selectActiveSessionTitle = (state: CodaStoreState): string | undefined => {
  const session = activeSessionOf(state);
  if (!session || session.draft) {
    return undefined;
  }
  const server = activeServerOf(state);
  const workspace = server?.catalog.find((ws) => ws.id === session.workspaceId);
  const summary = workspace?.sessions.find((item) => item.id === session.sessionId);
  return sessionTitle({
    id: session.sessionId,
    name: summary?.name,
    first_user_message: summary?.first_user_message ?? session.firstUserMessage,
  });
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
export const selectActivePermissionMode = (state: CodaStoreState) =>
  activeSessionOf(state)?.permissionMode ?? DEFAULT_PERMISSION_MODE;
const EMPTY_USAGE: UsageRecord[] = [];
export const selectActiveEditing = (state: CodaStoreState) => activeSessionOf(state)?.editing;
export const selectActiveForkDraft = (state: CodaStoreState) => activeSessionOf(state)?.forkDraft;
/** Whether a message can be pulled back in to be rewritten. Mirrors the
 * server's own precondition, so the entry point is only offered when the
 * request would actually be accepted. */
export const selectCanRewind = (state: CodaStoreState): boolean => {
  const session = activeSessionOf(state);
  return (
    !!session &&
    selectActiveStatus(state) === "connected" &&
    !session.running &&
    !session.compacting &&
    !session.starting &&
    !session.evicted &&
    !session.deleting &&
    session.approvals.length === 0 &&
    !session.editing?.submitting
  );
};
export const selectActiveUsage = (state: CodaStoreState) =>
  activeSessionOf(state)?.usage ?? EMPTY_USAGE;

/** Subscribe to a slice of the store; re-renders only when that slice changes. */
export function useCodaStore<T>(selector: (state: CodaStoreState) => T): T {
  return useStore(codaStore, selector);
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
