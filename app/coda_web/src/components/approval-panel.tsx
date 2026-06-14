import {
  Check,
  ChevronLeft,
  ChevronRight,
  KeyRound,
  ShieldCheck,
  TerminalSquare,
} from "lucide-react";
import { memo, useEffect, useRef, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Textarea } from "@/components/ui/textarea";
import {
  approvalKey,
  callArguments,
  deriveAllowPattern,
  extractShellCommand,
  parseAskUserParams,
  subAgentDisplayName,
  type PendingApproval,
  type ToolCall,
  type ToolCallResolution,
} from "@/lib/protocol";
import {
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

  return (
    <div className="px-4 pt-2">
      <div className="mx-auto w-full max-w-4xl overflow-hidden rounded-lg border border-amber-500/40 bg-amber-500/5">
        <div className="flex max-h-[55vh] flex-col">
          <div className="flex items-center justify-between px-4 pt-2.5">
            <h2 className="flex items-center gap-2 text-sm font-medium">
              <ShieldCheck className="size-4 text-amber-600" />
              Approval required
            </h2>
            <Badge variant="warning">
              {decidedCount}/{items.length} reviewed
            </Badge>
          </div>
          <div className="scrollbar-fine overflow-y-auto px-4 py-2.5">
            <ApprovalCall
              key={`${current.approval.thread_id}:${current.call.id}`}
              call={current.call}
              decision={decisionOf(current)}
              allowPattern={allowOf(current)}
              onDraft={handleDraft}
              onSetAllow={(pattern) => setAllowDraft(current.approval, current.call, pattern)}
            />
          </div>
          <div className="flex items-center justify-between gap-2 px-4 py-2">
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
  onDraft,
  onSetAllow,
}: {
  call: ToolCall;
  decision: ToolCallResolution | undefined;
  /** The "always allow" pattern staged for this call, if any. */
  allowPattern: string | undefined;
  onDraft: (resolution: ToolCallResolution) => void;
  onSetAllow: (pattern: string | null) => void;
}) {
  const [reason, setReason] = useState(() =>
    decision && decision !== "Execute" && "Rejected" in decision
      ? (decision.Rejected.reason ?? "")
      : "",
  );
  const [answer, setAnswer] = useState("");
  const [allowPattern, setAllowPattern] = useState(
    () => stagedAllow ?? deriveAllowPattern(extractShellCommand(call)),
  );
  const askUser = call.name === "ask_user" ? parseAskUserParams(call) : null;
  const approved = decision === "Execute";
  const rejected = Boolean(decision && decision !== "Execute" && "Rejected" in decision);
  const remembering = Boolean(stagedAllow);

  if (askUser) {
    const chosen =
      decision && decision !== "Execute" && "Resolved" in decision
        ? "Ok" in decision.Resolved
          ? decision.Resolved.Ok
          : null
        : null;
    return (
      <div className="space-y-3">
        <div className="flex items-center gap-2 text-sm font-medium">
          <KeyRound className="size-4 text-muted-foreground" />
          ask_user
        </div>
        <p className="text-sm leading-6">{askUser.question}</p>
        {askUser.options.length ? (
          <div className="scrollbar-fine grid max-h-52 gap-2 overflow-y-auto">
            {askUser.options.map((option) => (
              <Button
                key={option}
                variant={chosen === option ? "secondary" : "outline"}
                className="h-auto justify-start whitespace-normal py-2 text-left"
                onClick={() => onDraft({ Resolved: { Ok: option } })}
              >
                {option}
              </Button>
            ))}
          </div>
        ) : null}
        <div className="space-y-2 border-t pt-3">
          <Textarea
            value={answer}
            onChange={(event) => setAnswer(event.target.value)}
            placeholder="Custom response"
          />
          <Button
            variant="secondary"
            className="w-full"
            disabled={!answer.trim()}
            onClick={() => onDraft({ Resolved: { Ok: answer.trim() } })}
          >
            <Check />
            Use this response
          </Button>
        </div>
      </div>
    );
  }

  return (
    <div className="space-y-3">
      <div className="flex min-w-0 items-center gap-2 text-sm font-medium">
        <TerminalSquare className="size-4 shrink-0 text-muted-foreground" />
        <span className="truncate">{subAgentDisplayName(call.name)}</span>
      </div>
      <pre className="max-h-44 overflow-auto rounded-md bg-muted p-3 text-xs leading-5 text-muted-foreground">
        {formatArguments(callArguments(call))}
      </pre>
      {call.name === "shell" ? (
        <div className="flex gap-2">
          <Input
            value={allowPattern}
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
            className={cn(remembering && "border-amber-500/70 text-amber-700")}
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
