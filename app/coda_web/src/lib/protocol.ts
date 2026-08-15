export type ToolCall = {
  id: string;
  name: string;
  arguments?: string | null;
};

export type CompletionUsage = {
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
  prompt_tokens_details?: {
    audio_tokens?: number | null;
    cached_tokens?: number | null;
    cache_hit_tokens?: number | null;
    cache_miss_tokens?: number | null;
  } | null;
  completion_tokens_details?: {
    accepted_prediction_tokens?: number | null;
    audio_tokens?: number | null;
    reasoning_tokens?: number | null;
    rejected_prediction_tokens?: number | null;
  } | null;
};

export type AssistantMessage = {
  /** Server-minted identity, stable across reconnects. */
  message_id: string;
  content: string;
  tool_calls: ToolCall[];
  usage?: CompletionUsage | null;
  reasoning_content?: string | null;
  aborted?: boolean;
  /** RFC 3339 timestamps; the gap between them is the generation duration. */
  started_at: string;
  ended_at: string;
  /** RFC 3339 timestamp for the end of streamed reasoning, before answer content. */
  reasoning_ended_at?: string | null;
};

export type ToolOutput = { Ok: string } | { Err: string };

export type ToolArtifact = {
  type: "file_diff";
  path: string;
  operation: "create" | "modify" | "delete";
  patch: string;
};

export type ToolCallOutcome =
  | "Auto"
  | "Approved"
  | "Resolved"
  | "Aborted"
  | { Rejected: { reason?: string | null } };

export type ToolMessage = {
  /** Server-minted identity of this message. Distinct from `id`, which is the
   * id of the tool call it answers. */
  message_id: string;
  id: string;
  name: string;
  output: ToolOutput;
  outcome: ToolCallOutcome;
  artifacts?: ToolArtifact[];
  /** RFC 3339 timestamps; the gap between them is the execution duration.
   * `started_at` is absent for instantly-resolved calls (rejections, dispatch errors). */
  started_at?: string | null;
  ended_at: string;
};

export type ContentPart = { type: "text"; text: string } | { type: "image"; url: string };

export type UserMessage = {
  /** Server-minted identity, stable across reconnects. */
  message_id: string;
  parts: ContentPart[];
  created_at: string;
};

export type HistoryMessage =
  | { System: string }
  | { User: UserMessage }
  | { Assistant: AssistantMessage }
  | { Tool: ToolMessage };

export type PendingApproval = {
  thread_id: string;
  agent_name: string;
  /** The assistant message that asked for these calls — the batch's identity.
   * Echoed back in `resume` so the server can tell a stale decision (a second
   * submit landing after the batch already ran) from a live one. A call id
   * cannot serve here: ids are only unique within one assistant message. */
  parent_message_id: string;
  calls: ToolCall[];
  suspended_at: string;
  suggested_shell_allow_patterns: Record<string, string>;
};

export type ToolCallResolution =
  | "Execute"
  | { Resolved: ToolOutput }
  | { Rejected: { reason?: string | null } };

export type ResumeDecision = {
  /** The batch being answered, from `PendingApproval.parent_message_id`. */
  parent_message_id: string;
  resolutions: Array<[string, ToolCallResolution]>;
};

export type WorkspaceSession = {
  id: string;
  name: string | null;
  updated_at_ms?: number | null;
  first_user_message?: string | null;
  has_pending_approval: boolean;
};

export type WorkspaceSummary = {
  id: string;
  path: string;
  sessions: WorkspaceSession[];
};

/** Reasoning effort level — an arbitrary string configured per model. */
export type ReasoningEffort = string;

/**
 * How much a session may do without stopping to ask. An allow-list: the tools a
 * mode covers run unattended, everything else suspends for approval.
 *
 * - `explore` — inspection tools, plus shell commands pre-approved by the workspace.
 * - `accept_edits` — plus writing files. The default for a new session.
 * - `yolo` — everything, `shell` included. Only what outranks the mode still
 *   stops a call: `ask_user`, the workspace's `approval_required` list, and its
 *   shell deny-list.
 */
export type PermissionMode = "explore" | "accept_edits" | "yolo";

export const PERMISSION_MODES: PermissionMode[] = ["explore", "accept_edits", "yolo"];

/** What a session starts on, and what anything unremembered falls back to. */
export const DEFAULT_PERMISSION_MODE: PermissionMode = "accept_edits";

export function isPermissionMode(value: unknown): value is PermissionMode {
  return PERMISSION_MODES.includes(value as PermissionMode);
}

export type Modality = "text" | "image";

/**
 * A model the dashboard can select, grouped under a provider.
 * Empty `reasoning_efforts` means the model has no reasoning controls.
 */
export type ProviderInfo = {
  id: string;
  provider: string;
  model: string;
  context_window: number;
  reasoning_efforts: ReasoningEffort[];
  default_reasoning_effort?: ReasoningEffort | null;
  input_modalities: Modality[];
};

/**
 * Frozen JSON-RPC error codes. The wire carries only the number; this table
 * mirrors the server's `rpc.rs` constants. Standard codes sit in the JSON-RPC
 * range; app codes in the reserved server-error block (`-32000..-32099`).
 */
export const RpcCode = {
  PARSE_ERROR: -32700,
  INVALID_REQUEST: -32600,
  METHOD_NOT_FOUND: -32601,
  INVALID_PARAMS: -32602,
  INTERNAL_ERROR: -32603,
  /** `open_session`: another client holds it → drives the takeover UI. */
  SESSION_BUSY: -32001,
  /** `delete_session`: another connection is driving it. */
  NOT_OWNER: -32002,
  /** `set_model`: stale / not attached / not live. */
  SESSION_NOT_LIVE: -32003,
  /** `set_model`: a turn is in flight. */
  MODEL_SWITCH_WHILE_RUNNING: -32004,
  /** `set_model`: an opened session cannot change provider/model. */
  MODEL_LOCKED: -32005,
  /** A command requiring an idle session found a turn in flight or awaiting approval. */
  SESSION_NOT_IDLE: -32006,
  UNKNOWN_WORKSPACE: -32010,
  INVALID_SESSION_ID: -32011,
  INVALID_MODEL_SELECTION: -32012,
  SESSION_NOT_FOUND: -32013,
  /** `rewind`: the target is not a user message of this session's root thread —
   * including one an earlier rewind already discarded. */
  REWIND_TARGET_NOT_FOUND: -32014,
  OPEN_FAILED: -32020,
  DELETE_FAILED: -32021,
  RENAME_FAILED: -32022,
  /** `rewind`: it did not complete. This does not say whether the truncation
   * committed — when it did, a `Closed` push follows and the re-attach carries
   * the authoritative history. */
  REWIND_FAILED: -32023,
  FORK_FAILED: -32024,
  ALLOW_PATTERN_FAILED: -32030,
} as const;

// --- Request results / server-push payloads ----------------------------------
// These mirror the server's `wire.rs` structs. A `Snapshot` backs both the
// `open_session` result and the unsolicited `snapshot` push; the catalogs back
// both a request result and (historically) a push.

type Snapshot = {
  workspace_id: string;
  session_id: string;
  messages: HistoryMessage[];
  pending_approvals?: PendingApproval[];
  provider_id: string;
  reasoning_effort?: ReasoningEffort | null;
  /** The mode the session is *actually* running under. Authoritative: a
   * client attaching to a session that is already live adopts this instead of
   * imposing the one it remembered. */
  permission_mode?: PermissionMode;
  /** A turn is still in flight; its events are replayed after the snapshot. */
  turn_running?: boolean;
};

type WorkspaceCatalog = { workspaces: WorkspaceSummary[] };

type ProviderCatalog = { providers: ProviderInfo[]; default_provider: string };

type ModelSelectionResult = {
  provider_id: string;
  reasoning_effort?: ReasoningEffort | null;
};

/** Params of an `event` push: one live runtime event, nested under `event`. */
type EventPush = { workspace_id: string; session_id: string; event: WireEvent };

/** A bare (workspace, session) reference — the params of `abort` / `close_session`
 * notifications and of the `session_evicted` push. */
type SessionRef = { workspace_id: string; session_id: string };

// --- Request / notification params (client → server) -------------------------
// Mirror the server's `wire.rs` param structs. Together with the result types
// above they form the `RpcRequests` / `RpcNotifications` schema maps that type
// the RPC client.

type RpcRequest<Params, Result> = { params: Params; result: Result };

/**
 * Client → server **request** methods: each maps to its params and result type.
 * A `params` of `undefined` marks a no-argument request (`list_*`). This is the
 * `Req` schema the typed `RpcClient` keys `request(...)` on.
 */
export type RpcRequests = {
  list_workspaces: RpcRequest<undefined, WorkspaceCatalog>;
  list_providers: RpcRequest<undefined, ProviderCatalog>;
  open_session: RpcRequest<
    {
      workspace_id: string;
      session_id: string;
      provider_id?: string;
      reasoning_effort?: ReasoningEffort | null;
      /** The posture this client remembers for the session. Seeds a session the
       * server is not already running; ignored (and answered with the live
       * value) for one that is. */
      permission_mode?: PermissionMode;
      /** Evict whoever currently holds the session; without it the server
       * rejects with `SESSION_BUSY`. */
      takeover?: boolean;
    },
    Snapshot
  >;
  /** Change how much the session may do unattended. Unlike `set_model` this
   * rebuilds nothing, so it is accepted mid-turn and while approvals are
   * pending; it applies to the next tool call, not to calls already suspended. */
  set_permission_mode: RpcRequest<
    { workspace_id: string; session_id: string; mode: PermissionMode },
    { mode: PermissionMode }
  >;
  set_model: RpcRequest<
    {
      workspace_id: string;
      session_id: string;
      provider_id: string;
      reasoning_effort: ReasoningEffort | null;
    },
    ModelSelectionResult
  >;
  add_allow_pattern: RpcRequest<{ workspace_id: string; pattern: string }, Record<string, never>>;
  delete_session: RpcRequest<SessionRef, WorkspaceCatalog>;
  rename_session: RpcRequest<SessionRef & { name: string | null }, { name: string | null }>;
  /** Copy the session at `cut_message_id` — a root-thread user message — keeping
   * the turns before the one it opened. Omitting the cut copies everything
   * stored. The source is untouched; the server mints the new id and answers
   * with a refreshed catalog. */
  fork_session: RpcRequest<
    SessionRef & { cut_message_id?: string },
    { session_id: string; name: string | null } & WorkspaceCatalog
  >;
  /** Start a turn. A request rather than a notification so the server can
   * answer with the id it minted for the user message, letting the client key
   * that message the same way the server does. Rejects with SESSION_NOT_IDLE
   * while another turn is active, including one awaiting approval. */
  task: RpcRequest<
    {
      workspace_id: string;
      session_id: string;
      task: string;
      images?: string[];
    },
    { message_id: string }
  >;
  /** Discard `message_id` and everything the session produced from it onward,
   * then start a turn from the edited text. Answers with the id minted for that
   * text and the history that survived — *without* it, since the event stream
   * never carries user messages and the client appends that one itself. */
  rewind: RpcRequest<
    {
      workspace_id: string;
      session_id: string;
      message_id: string;
      task: string;
      images?: string[];
    },
    { message_id: string; messages: HistoryMessage[] }
  >;
};

/**
 * Client → server **notification** methods: each maps to its params type. This
 * is the `Notif` schema the typed `RpcClient` keys `notify(...)` on.
 */
export type RpcNotifications = {
  resume: {
    workspace_id: string;
    session_id: string;
    agent_name: string;
    thread_id: string;
    decision: ResumeDecision;
  };
  abort: SessionRef;
  close_session: SessionRef;
};

/** Server → client notifications handled through `RpcClient.addMethod(...)`. */
export type RpcPushes = {
  event: EventPush;
  snapshot: Snapshot;
  session_evicted: SessionRef;
};

export type WireEvent =
  | {
      type: "llm_start";
      agent_name: string;
      thread_id: string;
      model: string;
    }
  | {
      type: "llm_chunk";
      agent_name: string;
      thread_id: string;
      content: string;
    }
  | {
      type: "llm_reasoning_chunk";
      agent_name: string;
      thread_id: string;
      content: string;
    }
  | {
      type: "llm_end";
      agent_name: string;
      thread_id: string;
      message: AssistantMessage;
    }
  | {
      type: "tool_start";
      agent_name: string;
      thread_id: string;
      call: ToolCall;
    }
  | {
      type: "tool_end";
      agent_name: string;
      thread_id: string;
      message: ToolMessage;
    }
  | {
      type: "suspended";
      agent_name: string;
      thread_id: string;
      approval: PendingApproval;
    }
  | {
      type: "aborted";
      agent_name: string;
      thread_id: string;
      target: { reason: "generation" } | { reason: "tool_calls"; call_ids: string[] };
    }
  | {
      type: "error";
      agent_name: string;
      thread_id: string;
      message: string;
    }
  /** This turn's content never reached the database. Deliberately not a
   * turn-ending event — the turn is not finished, whatever is already on
   * screen. The server drops the session right after, so the next snapshot
   * shows what is actually stored. */
  | {
      type: "persist_failed";
      agent_name: string;
      thread_id: string;
      message: string;
    };

export function isOkOutput(output: ToolOutput): output is { Ok: string } {
  return "Ok" in output;
}

export function outputText(output: ToolOutput): string {
  return isOkOutput(output) ? output.Ok : output.Err;
}

export function outcomeText(outcome: ToolCallOutcome): string {
  if (typeof outcome === "string") {
    return outcome.toLowerCase();
  }
  return "rejected";
}

export function approvalKey(approval: PendingApproval): string {
  return `${approval.agent_name}:${approval.thread_id}`;
}

/**
 * Prefix the runtime applies to sub-agent names when exposing them to the LLM as
 * tools (mirrors MCP's `mcp__`). Keep in sync with `SUBAGENT_TOOL_PREFIX` in
 * `crates/coda_agent/src/agent.rs`. The prefix self-identifies a sub-agent
 * invocation wherever its tool name surfaces — live events and history alike.
 */
export const SUBAGENT_TOOL_PREFIX = "agent__";

export function isSubAgentToolName(name: string | undefined | null): name is string {
  return Boolean(name && name.startsWith(SUBAGENT_TOOL_PREFIX));
}

export function subAgentDisplayName(name: string): string {
  return name.startsWith(SUBAGENT_TOOL_PREFIX) ? name.slice(SUBAGENT_TOOL_PREFIX.length) : name;
}

/** Friendly action verbs for the built-in tools, e.g. `read_file` → `Read`. */
const TOOL_DISPLAY_NAMES: Record<string, string> = {
  ask_user: "Ask",
  read_file: "Read",
  write_file: "Write",
  edit_file: "Edit",
  ls: "List",
  glob: "Find",
  grep: "Search",
  shell: "Run",
  read_todos: "Read todos",
  write_todos: "Update todos",
};

/**
 * A human-readable label for a tool invocation. Built-ins map to a verb,
 * sub-agents drop the `agent__` prefix, and MCP tools keep their final segment.
 */
export function toolDisplayName(name: string): string {
  if (name.startsWith(SUBAGENT_TOOL_PREFIX)) {
    return subAgentDisplayName(name);
  }
  if (name in TOOL_DISPLAY_NAMES) {
    return TOOL_DISPLAY_NAMES[name];
  }
  if (name.startsWith("mcp__")) {
    const rest = name.slice("mcp__".length);
    const sep = rest.indexOf("__");
    if (sep === -1) {
      return rest || name;
    }
    const server = rest.slice(0, sep);
    const tool = rest.slice(sep + 2);
    return tool ? `MCP(${server}): ${tool}` : server;
  }
  return name;
}

/** Format a `read_file` offset/limit pair as a `(from-to)` line range. */
function formatLineRange(offset: unknown, limit: unknown): string | undefined {
  const start = typeof offset === "number" && offset >= 1 ? Math.floor(offset) : undefined;
  const count = typeof limit === "number" && limit >= 1 ? Math.floor(limit) : undefined;
  if (start === undefined && count === undefined) {
    return undefined;
  }
  const from = start ?? 1;
  return count === undefined ? `(${from}-)` : `(${from}-${from + count - 1})`;
}

export function callArguments(call: ToolCall): string {
  return call.arguments?.trim() || "{}";
}

export function parseCallArguments(call: ToolCall): unknown {
  try {
    return JSON.parse(callArguments(call));
  } catch {
    return {};
  }
}

export function extractShellCommand(call: ToolCall): string {
  const args = parseCallArguments(call);
  if (args && typeof args === "object" && "command" in args) {
    const command = (args as { command?: unknown }).command;
    return typeof command === "string" ? command : "";
  }
  return "";
}

function basename(p: string): string {
  const trimmed = p.replace(/[/\\]+$/, "");
  const segment = trimmed.split(/[/\\]/).pop() ?? "";
  return segment || trimmed;
}

/**
 * A short, human-readable summary of what a tool is acting on: the sub-agent
 * task, file basename, shell command, or search pattern.
 */
export function describeTool(
  name: string,
  argumentsJson: string | null | undefined,
): string | undefined {
  let args: Record<string, unknown> = {};
  try {
    const parsed = JSON.parse(argumentsJson?.trim() || "{}");
    if (parsed && typeof parsed === "object") {
      args = parsed as Record<string, unknown>;
    }
  } catch {
    return undefined;
  }
  const str = (value: unknown) =>
    typeof value === "string" && value.trim() ? value.trim() : undefined;

  if (isSubAgentToolName(name)) {
    return str(args.task);
  }

  switch (name) {
    case "ask_user":
      return str(args.question);
    case "shell":
      return str(args.description) ?? str(args.command);
    case "read_file": {
      const path = str(args.file_path);
      if (!path) {
        return undefined;
      }
      const range = formatLineRange(args.offset, args.limit);
      return range ? `${basename(path)} ${range}` : basename(path);
    }
    case "write_file":
    case "edit_file": {
      const path = str(args.file_path);
      return path ? basename(path) : undefined;
    }
    case "ls": {
      const path = str(args.path);
      return path ? basename(path) : undefined;
    }
    case "glob": {
      const pattern = str(args.pattern);
      if (!pattern) {
        return undefined;
      }
      const dir = str(args.path);
      return dir ? `${pattern} in ${basename(dir)}` : pattern;
    }
    case "grep": {
      const pattern = str(args.pattern);
      if (!pattern) {
        return undefined;
      }
      const scope = str(args.glob) ?? (str(args.path) ? basename(str(args.path)!) : undefined);
      return scope ? `${pattern} in ${scope}` : pattern;
    }
    case "write_todos": {
      if (!Array.isArray(args.todos)) {
        return undefined;
      }
      const todos = args.todos;
      const done = todos.filter(
        (item) => item && typeof item === "object" && (item as { done?: unknown }).done,
      ).length;
      return `${done}/${todos.length} done`;
    }
    default:
      return undefined;
  }
}

export type AskUserParams = {
  question: string;
  options: string[];
  multiple: boolean;
};

export function parseAskUserParams(call: ToolCall): AskUserParams {
  const args = parseCallArguments(call);
  if (args && typeof args === "object") {
    const question = (args as { question?: unknown }).question;
    const options = (args as { options?: unknown }).options;
    const multiple = (args as { multiple?: unknown }).multiple;
    return {
      question: typeof question === "string" ? question : "Input required",
      options: Array.isArray(options)
        ? options.filter((item): item is string => typeof item === "string")
        : [],
      multiple: multiple === true,
    };
  }
  return { question: "Input required", options: [], multiple: false };
}
