import {
  Bot,
  Check,
  ChevronLeft,
  ChevronRight,
  FilePen,
  FilePlus2,
  ShieldAlert,
  FileSearch,
  FileText,
  FolderTree,
  ListChecks,
  ListTodo,
  type LucideIcon,
  Plug,
  Search,
  ShieldCheck,
  SquareTerminal,
  Wrench,
} from "lucide-react";
import { memo, useEffect, useRef, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Textarea } from "@/components/ui/textarea";
import {
  approvalKey,
  callArguments,
  describeTool,
  parseAskUserParams,
  SUBAGENT_TOOL_PREFIX,
  toolDisplayName,
  type AskUserParams,
  type PendingApproval,
  type ToolCall,
  type ToolCallResolution,
} from "@/lib/protocol";
import {
  clearDraftCall,
  draftCall,
  selectActiveAllowDrafts,
  selectActiveApprovals,
  selectActiveDrafts,
  setAllowDraft,
  submitApprovals,
  useCodaStore,
} from "@/store/session";
import { cn } from "@/lib/utils";

function formatArguments(value: string) {
  try {
    return JSON.stringify(JSON.parse(value), null, 2);
  } catch {
    return value;
  }
}

type ApprovalItem = { approval: PendingApproval; call: ToolCall };

const TOOL_ICONS: Record<string, LucideIcon> = {
  ask_user: ShieldAlert,
  read_file: FileText,
  write_file: FilePlus2,
  edit_file: FilePen,
  ls: FolderTree,
  glob: FileSearch,
  grep: Search,
  shell: SquareTerminal,
  read_todos: ListTodo,
  write_todos: ListChecks,
};

type AskUserAnswer =
  | { custom: false; answer: string | string[] }
  | { custom: true; answer: string };

function askUserResolvedText(decision: ToolCallResolution | undefined): string | null {
  return decision && decision !== "Execute" && "Resolved" in decision && "Ok" in decision.Resolved
    ? decision.Resolved.Ok
    : null;
}

function approvalHeader(call: ToolCall) {
  if (call.name === "ask_user") {
    return { detail: parseAskUserParams(call).question, Icon: ShieldAlert, label: "Ask" };
  }
  const Icon = call.name.startsWith(SUBAGENT_TOOL_PREFIX)
    ? Bot
    : call.name in TOOL_ICONS
      ? TOOL_ICONS[call.name]
      : call.name.startsWith("mcp__")
        ? Plug
        : Wrench;
  return {
    detail: describeTool(call.name, callArguments(call)),
    Icon,
    label: toolDisplayName(call.name),
  };
}

function parseAskUserAnswer(decision: ToolCallResolution | undefined): AskUserAnswer | null {
  const text = askUserResolvedText(decision);
  if (!text) {
    return null;
  }
  try {
    const parsed = JSON.parse(text);
    if (parsed && typeof parsed === "object") {
      const custom = (parsed as { custom?: unknown }).custom;
      if (custom === true) {
        const answer = (parsed as { answer?: unknown }).answer;
        return typeof answer === "string" ? { custom, answer } : null;
      }
      if (custom === false) {
        const answer = (parsed as { answer?: unknown }).answer;
        if (typeof answer === "string") {
          return { custom, answer };
        }
        return Array.isArray(answer)
          ? { custom, answer: answer.filter((item): item is string => typeof item === "string") }
          : null;
      }
    }
  } catch {
    return null;
  }
  return null;
}

function encodeAskUserAnswer(answer: AskUserAnswer) {
  return JSON.stringify(answer);
}

function askUserSelectedOptions(decision: ToolCallResolution | undefined, multiple: boolean) {
  const answer = parseAskUserAnswer(decision);
  if (answer && !answer.custom) {
    return Array.isArray(answer.answer) ? answer.answer : [answer.answer];
  }
  const text = askUserResolvedText(decision);
  if (!text) {
    return [];
  }
  if (!multiple) {
    return [text];
  }
  try {
    const parsed = JSON.parse(text);
    return Array.isArray(parsed)
      ? parsed.filter((item): item is string => typeof item === "string")
      : [];
  } catch {
    return [];
  }
}

function askUserCustomAnswer(decision: ToolCallResolution | undefined, askUser: AskUserParams) {
  const answer = parseAskUserAnswer(decision);
  if (answer?.custom) {
    return answer.answer;
  }
  if (answer) {
    return "";
  }
  const text = askUserResolvedText(decision);
  if (!text) {
    return "";
  }
  if (askUser.multiple) {
    try {
      const parsed = JSON.parse(text);
      return Array.isArray(parsed) ? "" : text;
    } catch {
      return text;
    }
  }
  return askUser.options.includes(text) ? "" : text;
}

function DecisionRadio({
  value,
  selected,
  label,
}: {
  value: string;
  selected: boolean;
  label: string;
}) {
  return (
    <Label
      className={cn(
        "cursor-pointer rounded-md border px-3 py-2 transition-colors",
        selected ? "border-primary bg-accent" : "border-input hover:bg-accent",
      )}
    >
      <RadioGroupItem value={value} />
      {label}
    </Label>
  );
}

function AskUserApprovalCall({
  askUser,
  decision,
  onDraft,
  onClearDraft,
}: {
  askUser: AskUserParams;
  decision: ToolCallResolution | undefined;
  onDraft: (resolution: ToolCallResolution) => void;
  onClearDraft: () => void;
}) {
  const [answer, setAnswer] = useState(() => askUserCustomAnswer(decision, askUser));
  const [selectedOptions, setSelectedOptions] = useState(() =>
    askUserSelectedOptions(decision, askUser.multiple),
  );
  const resolvedAnswer = parseAskUserAnswer(decision);
  const chosen =
    resolvedAnswer && !resolvedAnswer.custom && "answer" in resolvedAnswer
      ? resolvedAnswer.answer
      : askUserResolvedText(decision);
  const chosenIndex =
    chosen === null ? -1 : askUser.options.findIndex((option) => option === chosen);
  const selectedIndex = answer ? -1 : chosenIndex;
  const toggleOption = (option: string) => {
    const next = selectedOptions.includes(option)
      ? selectedOptions.filter((selected) => selected !== option)
      : [...selectedOptions, option];
    setAnswer("");
    setSelectedOptions(next);
    if (next.length > 0) {
      onDraft({ Resolved: { Ok: encodeAskUserAnswer({ custom: false, answer: next }) } });
    } else {
      onClearDraft();
    }
  };
  const handleAnswerChange = (value: string) => {
    setAnswer(value);
    setSelectedOptions([]);
    const trimmed = value.trim();
    if (trimmed) {
      onDraft({ Resolved: { Ok: encodeAskUserAnswer({ custom: true, answer: trimmed }) } });
    } else {
      onClearDraft();
    }
  };

  return (
    <div className="space-y-3">
      {askUser.options.length ? (
        <>
          {askUser.multiple ? (
            <div className="scrollbar-fine grid max-h-52 gap-2 overflow-y-auto">
              {askUser.options.map((option, optionIndex) => {
                const selected = selectedOptions.includes(option);
                return (
                  <Label
                    key={`${option}-${optionIndex}`}
                    className={cn(
                      "flex cursor-pointer items-start gap-3 rounded-md border px-3 py-2 text-sm leading-5 transition-colors",
                      selected ? "border-primary bg-accent" : "border-input hover:bg-accent",
                    )}
                  >
                    <Checkbox
                      checked={selected}
                      onCheckedChange={() => toggleOption(option)}
                      className="mt-0.5"
                    />
                    <span className="min-w-0 whitespace-normal text-left">{option}</span>
                  </Label>
                );
              })}
            </div>
          ) : (
            <RadioGroup
              value={selectedIndex >= 0 ? String(selectedIndex) : ""}
              onValueChange={(value) => {
                const option = askUser.options[Number(value)];
                if (option !== undefined) {
                  setAnswer("");
                  onDraft({
                    Resolved: {
                      Ok: encodeAskUserAnswer({ custom: false, answer: option }),
                    },
                  });
                }
              }}
              className="scrollbar-fine grid max-h-52 gap-2 overflow-y-auto"
            >
              {askUser.options.map((option, optionIndex) => {
                const selected = selectedIndex === optionIndex;
                return (
                  <Label
                    key={`${option}-${optionIndex}`}
                    className={cn(
                      "flex cursor-pointer items-start gap-3 rounded-md border px-3 py-2 text-sm leading-5 transition-colors",
                      selected ? "border-primary bg-accent" : "border-input hover:bg-accent",
                    )}
                  >
                    <RadioGroupItem value={String(optionIndex)} className="mt-0.5" />
                    <span className="min-w-0 whitespace-normal text-left">{option}</span>
                  </Label>
                );
              })}
            </RadioGroup>
          )}
        </>
      ) : null}
      <div className="space-y-2 border-t pt-3">
        <Textarea
          value={answer}
          onChange={(event) => handleAnswerChange(event.target.value)}
          placeholder="Custom response"
        />
      </div>
    </div>
  );
}

export const ApprovalPanel = memo(function ApprovalPanel() {
  const approvals = useCodaStore(selectActiveApprovals);
  const drafts = useCodaStore(selectActiveDrafts);
  const allowDrafts = useCodaStore(selectActiveAllowDrafts);
  const items: ApprovalItem[] = approvals.flatMap((approval) =>
    approval.calls.map((call) => ({ approval, call })),
  );
  const [index, setIndex] = useState(0);

  // Reset to the first item whenever the pending set itself changes (a new
  // batch arrives) — but keep position while the user works through a batch.
  const itemsKey = items.map((item) => `${item.approval.thread_id}:${item.call.id}`).join("|");
  const prevKey = useRef(itemsKey);
  useEffect(() => {
    if (prevKey.current !== itemsKey) {
      prevKey.current = itemsKey;
      setIndex(0);
    }
  }, [itemsKey]);

  if (items.length === 0) {
    return null;
  }

  const decisionOf = (item: ApprovalItem) => drafts[approvalKey(item.approval)]?.[item.call.id];
  const allowOf = (item: ApprovalItem) => allowDrafts[approvalKey(item.approval)]?.[item.call.id];
  const current = items[Math.min(index, items.length - 1)] ?? items[0];
  const currentIndex = items.indexOf(current);
  const currentHeader = approvalHeader(current.call);
  const CurrentHeaderIcon = "Icon" in currentHeader ? currentHeader.Icon : undefined;
  const decidedCount = items.filter((item) => decisionOf(item)).length;
  const allDecided = decidedCount === items.length;

  const handleDraft = (resolution: ToolCallResolution) => {
    draftCall(current.approval, current.call, resolution);
    // Approving needs no follow-up, so jump ahead; rejecting stays put so the
    // user can fill in a reason.
    if (resolution === "Execute" && currentIndex < items.length - 1) {
      setIndex(currentIndex + 1);
    }
  };
  const handleClearDraft = () => {
    clearDraftCall(current.approval, current.call);
  };

  return (
    <div className="px-2 pt-2 sm:px-4">
      <div className="mx-auto w-full max-w-4xl overflow-hidden rounded-lg border">
        <div className="flex max-h-[55vh] flex-col">
          <div className="flex items-start justify-between gap-3 px-3 pt-2.5 sm:px-4">
            <div className="flex min-w-0 items-start gap-2 text-sm font-medium leading-6">
              {CurrentHeaderIcon ? (
                <CurrentHeaderIcon className="mt-1 size-4 shrink-0 text-muted-foreground" />
              ) : null}
              {"label" in currentHeader ? (
                <span className="shrink-0 leading-6">{currentHeader.label}</span>
              ) : null}
              <span className="min-w-0 whitespace-normal break-words">{currentHeader.detail}</span>
            </div>
            <Badge variant="warning" className="shrink-0">
              {decidedCount}/{items.length} reviewed
            </Badge>
          </div>
          <div className="scrollbar-fine overflow-y-auto px-3 py-2.5 sm:px-4">
            <ApprovalCall
              key={`${current.approval.thread_id}:${current.call.id}`}
              call={current.call}
              decision={decisionOf(current)}
              allowPattern={allowOf(current)}
              suggestedAllowPattern={
                current.approval.suggested_shell_allow_patterns[current.call.id]
              }
              onDraft={handleDraft}
              onClearDraft={handleClearDraft}
              onSetAllow={(pattern) => setAllowDraft(current.approval, current.call, pattern)}
            />
          </div>
          <div className="flex items-center justify-between gap-2 px-3 py-2 sm:px-4">
            <div className="flex items-center gap-1">
              <Button
                variant="ghost"
                size="icon"
                disabled={currentIndex === 0}
                onClick={() => setIndex(currentIndex - 1)}
              >
                <ChevronLeft />
              </Button>
              <span className="min-w-12 text-center text-xs tabular-nums text-muted-foreground">
                {currentIndex + 1} / {items.length}
              </span>
              <Button
                variant="ghost"
                size="icon"
                disabled={currentIndex >= items.length - 1}
                onClick={() => setIndex(currentIndex + 1)}
              >
                <ChevronRight />
              </Button>
            </div>
            <Button disabled={!allDecided} onClick={submitApprovals}>
              <Check />
              Submit {decidedCount}/{items.length}
            </Button>
          </div>
        </div>
      </div>
    </div>
  );
});

function ApprovalCall({
  call,
  decision,
  allowPattern: stagedAllow,
  suggestedAllowPattern,
  onDraft,
  onClearDraft,
  onSetAllow,
}: {
  call: ToolCall;
  decision: ToolCallResolution | undefined;
  /** The "always allow" pattern staged for this call, if any. */
  allowPattern: string | undefined;
  suggestedAllowPattern: string | undefined;
  onDraft: (resolution: ToolCallResolution) => void;
  onClearDraft: () => void;
  onSetAllow: (pattern: string | null) => void;
}) {
  const [reason, setReason] = useState(() =>
    decision && decision !== "Execute" && "Rejected" in decision
      ? (decision.Rejected.reason ?? "")
      : "",
  );
  const [allowPattern, setAllowPattern] = useState(
    () => stagedAllow ?? suggestedAllowPattern ?? "",
  );
  const askUser = call.name === "ask_user" ? parseAskUserParams(call) : null;
  const approved = decision === "Execute";
  const rejected = Boolean(decision && decision !== "Execute" && "Rejected" in decision);
  const remembering = Boolean(stagedAllow);
  const showAllowPattern = call.name === "shell" && Boolean(stagedAllow ?? suggestedAllowPattern);

  if (askUser) {
    return (
      <AskUserApprovalCall
        askUser={askUser}
        decision={decision}
        onDraft={onDraft}
        onClearDraft={onClearDraft}
      />
    );
  }

  return (
    <div className="space-y-3">
      <pre className="max-h-44 overflow-auto rounded-md bg-muted p-3 text-xs leading-5 text-muted-foreground">
        {formatArguments(callArguments(call))}
      </pre>
      {showAllowPattern ? (
        <div className="grid grid-cols-[minmax(0,1fr)_auto] gap-2">
          <Input
            value={allowPattern}
            className="min-w-0"
            onChange={(event) => {
              const value = event.target.value;
              setAllowPattern(value);
              if (remembering) {
                onSetAllow(value);
              }
            }}
          />
          <Button
            type="button"
            variant={remembering ? "secondary" : "outline"}
            aria-pressed={remembering}
            className={cn(
              "shrink-0",
              remembering && "border border-warning/70 text-warning-foreground",
            )}
            onClick={() => {
              if (remembering) {
                onSetAllow(null);
              } else {
                onDraft("Execute");
                onSetAllow(allowPattern);
              }
            }}
          >
            <ShieldCheck />
            Always
          </Button>
        </div>
      ) : null}
      <RadioGroup
        value={approved ? "run" : rejected ? "reject" : ""}
        onValueChange={(value) => {
          if (value === "run") {
            onDraft("Execute");
          } else if (value === "reject") {
            onSetAllow(null);
            onDraft({
              Rejected: { reason: reason.trim() ? reason.trim() : null },
            });
          }
        }}
        className="grid grid-cols-2 gap-2"
      >
        <DecisionRadio value="run" selected={approved} label="Approve" />
        <DecisionRadio value="reject" selected={rejected} label="Reject" />
      </RadioGroup>
      <Input
        value={reason}
        disabled={!rejected}
        onChange={(event) => {
          const value = event.target.value;
          setReason(value);
          onDraft({ Rejected: { reason: value.trim() ? value.trim() : null } });
        }}
        placeholder="Rejection reason (optional)"
      />
    </div>
  );
}
