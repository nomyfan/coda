export type ToolCall = {
  id: string;
  name: string;
  arguments?: string | null;
};

export type CompletionUsage = {
  prompt_tokens: number;
  completion_tokens: number;
};

export type AssistantMessage = {
  content: string;
  tool_calls: ToolCall[];
  usage?: CompletionUsage | null;
  aborted?: boolean;
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
};

export type HistoryMessage =
  | { System: string }
  | { User: string }
  | { Assistant: AssistantMessage }
  | { Tool: ToolMessage };

export type PendingApproval = {
  thread_id: string;
  agent_name: string;
  calls: ToolCall[];
  suspended_at: string;
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
};

export type WorkspaceSummary = {
  id: string;
  path: string;
  sessions: WorkspaceSession[];
};

export type ClientMessage =
  | { type: "list_workspaces" }
  | { type: "open_session"; workspace_id: string; session_id: string }
  | { type: "task"; workspace_id: string; session_id: string; task: string }
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
  | { type: "add_allow_pattern"; workspace_id: string; pattern: string };

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
      type: "snapshot";
      workspace_id: string;
      session_id: string;
      messages: HistoryMessage[];
      pending_approvals?: PendingApproval[];
    }
  | { type: "event"; workspace_id: string; session_id: string; event: WireEvent }
  | { type: "allow_pattern_result"; workspace_id: string; pattern: string; error?: string | null };

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
 * A short, human-readable summary of what a built-in tool is acting on — the
 * file basename for filesystem tools, the command for `shell`, the pattern for
 * search tools. Returns undefined when there's nothing useful to surface.
 */
export function describeTool(
  name: string,
  argumentsJson: string | null | undefined
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

  switch (name) {
    case "shell":
      return str(args.command);
    case "read_file":
    case "write_file":
    case "edit_file": {
      const path = str(args.file_path);
      return path ? basename(path) : undefined;
    }
    case "ls": {
      const path = str(args.path);
      return path ? basename(path) : undefined;
    }
    case "glob":
    case "grep":
      return str(args.pattern);
    default:
      return undefined;
  }
}

export function deriveAllowPattern(command: string): string {
  const firstToken = command.trim().split(/\s+/)[0] ?? "";
  return /\s/.test(command.trim()) ? `${firstToken} *` : firstToken;
}

export type AskUserParams = {
  question: string;
  options: string[];
};

export function parseAskUserParams(call: ToolCall): AskUserParams {
  const args = parseCallArguments(call);
  if (args && typeof args === "object") {
    const question = (args as { question?: unknown }).question;
    const options = (args as { options?: unknown }).options;
    return {
      question: typeof question === "string" ? question : "Input required",
      options: Array.isArray(options) ? options.filter((item): item is string => typeof item === "string") : [],
    };
  }
  return { question: "Input required", options: [] };
}
