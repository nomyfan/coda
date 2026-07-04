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

export type ToolCallOutcome =
  | "Auto"
  | "Approved"
  | "Resolved"
  | "Aborted"
  | { Rejected: { reason?: string | null } };

export type ToolMessage = {
  id: string;
  name: string;
  output: ToolOutput;
  outcome: ToolCallOutcome;
  /** RFC 3339 timestamps; the gap between them is the execution duration.
   * `started_at` is absent for instantly-resolved calls (rejections, dispatch errors). */
  started_at?: string | null;
  ended_at: string;
};

export type ContentPart = { type: "text"; text: string } | { type: "image"; url: string };

export type UserMessage = { parts: ContentPart[]; created_at: string };

export type HistoryMessage =
  | { System: string }
  | { User: UserMessage }
  | { Assistant: AssistantMessage }
  | { Tool: ToolMessage };

export type PendingApproval = {
  thread_id: string;
  agent_name: string;
  calls: ToolCall[];
  suspended_at: string;
  suggested_shell_allow_patterns: Record<string, string>;
};

export type ToolCallResolution =
  | "Execute"
  | { Resolved: ToolOutput }
  | { Rejected: { reason?: string | null } };

export type ResumeDecision = {
  resolutions: Array<[string, ToolCallResolution]>;
};

export type WorkspaceSession = {
  id: string;
  updated_at_ms?: number | null;
  first_user_message?: string | null;
  has_pending_approval: boolean;
};

export type WorkspaceSummary = {
  id: string;
  path: string;
  sessions: WorkspaceSession[];
};

/** Reasoning effort levels, mirroring the server enum. */
export type ReasoningEffort = "none" | "minimal" | "low" | "medium" | "high" | "xhigh";

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
  input_modalities: Modality[];
};

export type ClientMessage =
  | { type: "list_workspaces" }
  | { type: "list_providers" }
  | {
      type: "open_session";
      workspace_id: string;
      session_id: string;
      provider_id?: string;
      reasoning_effort?: ReasoningEffort | null;
      /** Evict whoever currently holds the session (explicit user decision);
       * without it the server answers `session_busy` instead. */
      takeover?: boolean;
    }
  | { type: "task"; workspace_id: string; session_id: string; task: string; images?: string[] }
  | {
      type: "resume";
      workspace_id: string;
      session_id: string;
      agent_name: string;
      thread_id: string;
      decision: ResumeDecision;
    }
  | { type: "abort"; workspace_id: string; session_id: string }
  | { type: "delete_session"; workspace_id: string; session_id: string }
  | { type: "close_session"; workspace_id: string; session_id: string }
  | { type: "add_allow_pattern"; workspace_id: string; pattern: string }
  | {
      type: "set_model";
      workspace_id: string;
      session_id: string;
      provider_id: string;
      reasoning_effort: ReasoningEffort | null;
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
    };

export type ServerMessage =
  | {
      type: "workspace_catalog";
      workspaces: WorkspaceSummary[];
    }
  | {
      type: "provider_catalog";
      providers: ProviderInfo[];
      default_provider: string;
    }
  | {
      type: "model_changed";
      workspace_id: string;
      session_id: string;
      provider_id: string;
      reasoning_effort?: ReasoningEffort | null;
    }
  | {
      type: "snapshot";
      workspace_id: string;
      session_id: string;
      messages: HistoryMessage[];
      pending_approvals?: PendingApproval[];
      provider_id: string;
      reasoning_effort?: ReasoningEffort | null;
      /** A turn is still in flight; its events are replayed after the snapshot. */
      turn_running?: boolean;
    }
  | { type: "event"; workspace_id: string; session_id: string; event: WireEvent }
  | { type: "allow_pattern_result"; workspace_id: string; pattern: string; error?: string | null }
  | { type: "session_evicted"; workspace_id: string; session_id: string }
  /** An open without `takeover` hit a session another client is driving. */
  | { type: "session_busy"; workspace_id: string; session_id: string };

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
