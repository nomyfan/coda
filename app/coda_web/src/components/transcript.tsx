import {
  Bot,
  Brain,
  Check,
  ChevronDown,
  ChevronRight,
  CircleAlert,
  Copy,
  FilePen,
  FilePlus2,
  ShieldAlert,
  FileSearch,
  FileText,
  FolderTree,
  ListChecks,
  GitBranch,
  ListTodo,
  type LucideIcon,
  MessageSquare,
  Pencil,
  Plug,
  Search,
  SquareTerminal,
  Wrench,
} from "lucide-react";
import { LayoutGroup, motion } from "motion/react";
import {
  lazy,
  memo,
  Suspense,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";
import {
  ImageLightbox,
  IMAGE_LIGHTBOX_TRANSITION,
  imageLightboxLayoutId,
} from "@/components/image-lightbox";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { CodaMark } from "@/components/logo";
import { Markdown } from "@/components/markdown";
import {
  beginEdit,
  discardedFrom,
  forkActiveSession,
  selectActiveApprovalCount,
  selectActiveEditing,
  selectActiveEntries,
  selectActiveForkKey,
  selectActiveKey,
  selectActiveRunning,
  selectCanRewind,
  selectForking,
  type TranscriptEntry,
  useCodaStore,
} from "@/store/session";
import { parseCompactCommand } from "@/lib/compact-command";
import {
  isSubAgentToolName,
  subAgentDisplayName,
  SUBAGENT_TOOL_PREFIX,
  toolDisplayName,
} from "@/lib/protocol";
import { getStoredThemePreference, resolveTheme, subscribeThemeChange } from "@/lib/theme";
import { cn, formatClockTime, formatDuration, formatElapsed } from "@/lib/utils";

const NO_ENTRIES: TranscriptEntry[] = [];

const VellumPatchDiff = lazy(() =>
  import("@/components/vellum-patch-diff").then(({ VellumPatchDiff: Component }) => ({
    default: Component,
  })),
);

const ROOT_AGENT = "coda";

/** Reveal-on-hover, matching the message action buttons' fade-in behavior. */
const HOVER_REVEAL =
  "opacity-0 transition-opacity group-hover/message:opacity-100 group-focus-within/message:opacity-100";

/** A sub-agent's own events (its inner LLM turns, reasoning, tool calls). */
function isSubAgentEntry(entry: TranscriptEntry) {
  return Boolean(entry.agentName && entry.agentName !== ROOT_AGENT);
}

/** An entry backed by a real tool invocation — `tool_call` while it's running,
 * `tool_result` once it resolves (same entry, updated in place). Checking
 * both catches a call left stranded mid-flight by an abort, which never gets
 * the `tool_result` update but was still a real operation. Excludes a
 * rejected call: it never runs, but still gets a result-only entry (no
 * `tool_start` preceded it) carrying `status: "rejected"`, since the server
 * answers a denied approval with a `tool_end` of its own. */
function isToolEntry(entry: TranscriptEntry) {
  if (entry.kind === "tool_call") {
    return true;
  }
  return entry.kind === "tool_result" && entry.status !== "rejected";
}

type TranscriptRenderItem =
  | { type: "entry"; entry: TranscriptEntry }
  | { type: "turn"; id: string; entries: TranscriptEntry[] };

function findFinalAssistantIndex(entries: TranscriptEntry[]) {
  for (let index = entries.length - 1; index >= 0; index -= 1) {
    const entry = entries[index];
    if (
      entry.kind === "assistant" &&
      (entry.isFinalResponse || (entry.agentName === ROOT_AGENT && entry.liveKey))
    ) {
      return index;
    }
  }
  return -1;
}

/** How a turn ended, rather than more work inside it. An abort emits both: the
 * marker, and then the error the interrupted run returns. */
function isTurnEndNotice(entry: TranscriptEntry) {
  return entry.kind === "error" || (entry.kind === "system" && entry.status === "aborted");
}

/**
 * Where a turn's answer sits, or -1 when the turn has none to lift out of the
 * process list.
 *
 * Anything after the reply means the turn went on past it — except these
 * notices, which land *after* the partial reply an interrupted model did
 * produce. Letting them hide it would bury the reply in the process list along
 * with its copy and fork actions, and only until a reload: they are events
 * rather than messages, so they never come back with the history.
 */
export function finalAssistantIndexOf(entries: TranscriptEntry[]) {
  const index = findFinalAssistantIndex(entries);
  if (index < 0) {
    return -1;
  }
  return entries.slice(index + 1).every(isTurnEndNotice) ? index : -1;
}

/** Entries rendered as collapsed disclosure rows inside an assistant turn. */
function hasDisclosureWork(entries: TranscriptEntry[]) {
  return entries.some(
    (entry) =>
      entry.kind === "tool_call" ||
      entry.kind === "tool_result" ||
      entry.kind === "reasoning" ||
      (entry.kind === "assistant" && entry.isFinalResponse === false),
  );
}

function turnGroup(entries: TranscriptEntry[]): TranscriptRenderItem {
  return {
    type: "turn",
    id: `turn:${entries[0]?.id ?? "empty"}`,
    entries,
  };
}

export function transcriptRenderItems(entries: TranscriptEntry[]): TranscriptRenderItem[] {
  const items: TranscriptRenderItem[] = [];
  let index = 0;

  while (index < entries.length) {
    const entry = entries[index];
    if (entry.kind === "user") {
      items.push({ type: "entry", entry });
      index += 1;
      continue;
    }

    const start = index;
    while (index < entries.length && entries[index].kind !== "user") {
      index += 1;
    }

    const segment = entries.slice(start, index);
    if (!hasDisclosureWork(segment)) {
      items.push(...segment.map((entry) => ({ type: "entry" as const, entry })));
      continue;
    }

    items.push(turnGroup(segment));
  }

  return items;
}

/**
 * `suppressed` blanks the transcript while the new-session composer is open
 * (no session is active yet), without unsubscribing from the store.
 */
export const Transcript = memo(function Transcript({
  workspace,
  suppressed,
}: {
  workspace?: string;
  suppressed: boolean;
}) {
  const liveEntries = useCodaStore(selectActiveEntries);
  const liveRunning = useCodaStore(selectActiveRunning);
  const liveApprovalCount = useCodaStore(selectActiveApprovalCount);
  const activeKey = useCodaStore(selectActiveKey);
  const editing = useCodaStore(selectActiveEditing);
  const entries = suppressed ? NO_ENTRIES : liveEntries;
  const running = suppressed ? false : liveRunning;
  const approvalPending = !suppressed && liveApprovalCount > 0;
  const scrollRef = useRef<HTMLElement | null>(null);
  const contentRef = useRef<HTMLDivElement | null>(null);
  const bottomRef = useRef<HTMLDivElement | null>(null);
  const contentHeightRef = useRef<number | undefined>(undefined);
  const runningRef = useRef(running);
  // While true the view follows new content; it flips off once the user scrolls
  // up to read, so streaming output no longer yanks them back to the bottom.
  const stickToBottomRef = useRef(true);
  const renderItems = transcriptRenderItems(entries);
  // Everything from the edit target on is what submitting would discard. Entries
  // are appended in order, so one index marks the whole region — and an orphan
  // draft (`target === null`) marks nothing, because there is nothing left to
  // discard.
  const discardFrom =
    !suppressed && editing?.target ? discardedFrom(entries, editing.target) : undefined;
  const discardedIds =
    discardFrom === undefined
      ? undefined
      : new Set(entries.slice(discardFrom).map((entry) => entry.id));
  const lastEntry = entries.at(-1);
  // A fork keeps the turns *before* the message it cuts at, so the opening one
  // has nothing to keep — that copy would just be an empty session.
  const firstUserId = entries.find((entry) => entry.kind === "user")?.id;

  function handleScroll() {
    const el = scrollRef.current;
    if (!el) {
      return;
    }
    const distanceFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight;
    stickToBottomRef.current = distanceFromBottom < 20;
  }

  // Switching sessions resumes following from the bottom, so a session opened
  // after the user scrolled up in another still lands on its latest content.
  // Runs before the scroll effect below so that effect sees a reset ref.
  useEffect(() => {
    stickToBottomRef.current = true;
  }, [activeKey]);

  useLayoutEffect(() => {
    runningRef.current = running;
  }, [running]);

  useEffect(() => {
    // Always snap to the bottom for the user's own just-sent message; otherwise
    // only follow if they hadn't scrolled away.
    if (!stickToBottomRef.current && lastEntry?.kind !== "user") {
      return;
    }
    stickToBottomRef.current = true;
    bottomRef.current?.scrollIntoView({ block: "end" });
  }, [activeKey, entries.length, running, lastEntry?.kind]);

  useEffect(() => {
    const content = contentRef.current;
    if (!content) {
      return;
    }
    // Follow actual layout growth: collapsed reasoning stays free, while an
    // expanded reasoning stream still keeps a bottom-pinned viewport moving.
    const observer = new ResizeObserver(([entry]) => {
      const previousHeight = contentHeightRef.current;
      const nextHeight = entry.contentRect.height;
      contentHeightRef.current = nextHeight;
      if (
        previousHeight !== undefined &&
        previousHeight !== nextHeight &&
        runningRef.current &&
        stickToBottomRef.current
      ) {
        bottomRef.current?.scrollIntoView({ block: "end" });
      }
    });
    observer.observe(content);
    return () => observer.disconnect();
  }, []);

  return (
    // `layoutScroll` lets motion account for this container's scroll offset when
    // measuring layout, so scrolling to the bottom on a new message doesn't make
    // the layoutId image thumbnails spuriously animate from their old position.
    <motion.section
      ref={scrollRef}
      onScroll={handleScroll}
      layoutScroll
      className="scrollbar-fine fade-edge-bottom min-h-0 flex-1 overflow-y-auto bg-background px-4 py-3"
    >
      <div ref={contentRef} className="mx-auto flex w-full max-w-4xl flex-col gap-2">
        {entries.length === 0 ? (
          <div className="flex min-h-[48vh] flex-col items-center justify-center text-center">
            <CodaMark className="mb-4 size-12" />
            <div className="text-base font-semibold">
              {workspace ? "What should we do?" : "No session selected"}
            </div>
            <p className="mt-1.5 max-w-md text-sm leading-6 text-muted-foreground">
              {workspace
                ? "Send a message to start the session."
                : "Pick a server and open or create a session to begin."}
            </p>
          </div>
        ) : (
          <>
            {renderItems.map((item, index) => {
              const anchor = item.type === "entry" ? item.entry.id : item.entries[0]?.id;
              const discarded = Boolean(anchor && discardedIds?.has(anchor));
              return (
                <div
                  key={item.type === "entry" ? item.entry.id : item.id}
                  className={cn(
                    "contents",
                    discarded && "[&>*]:opacity-40 [&>*]:transition-opacity",
                  )}
                >
                  {item.type === "entry" ? (
                    <TranscriptItem entry={item.entry} forkable={item.entry.id !== firstUserId} />
                  ) : (
                    <AssistantTurnBubble
                      entries={item.entries}
                      approvalPending={approvalPending}
                      // Only the last turn can still be going; whatever is
                      // behind it is over however it ended.
                      settled={index < renderItems.length - 1 || !(running || approvalPending)}
                    />
                  )}
                </div>
              );
            })}
            {/* Optimistic loading bubble: the turn is running but the backend
             * hasn't streamed its first event yet, so no assistant turn exists
             * to carry the shimmer. Shown from send until the first chunk/tool. */}
            {running && lastEntry?.kind === "user" ? <PendingTurnBubble /> : null}
          </>
        )}
        <div ref={bottomRef} />
      </div>
    </motion.section>
  );
});

function entryTitle(entry: TranscriptEntry) {
  if (entry.title) {
    return toolDisplayName(entry.title);
  }
  return entry.agentName ? `${entry.agentName}` : entry.kind;
}

/** Icons standing in for each built-in tool name (see `TOOL_DISPLAY_NAMES`). */
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

/** Pick the glyph that stands in for an entry's tool/kind label. */
function entryIcon(entry: TranscriptEntry): LucideIcon {
  switch (entry.kind) {
    case "reasoning":
      return Brain;
    case "assistant":
      return MessageSquare;
    case "error":
      return CircleAlert;
    default:
      break;
  }
  const name = entry.title;
  if (name) {
    if (name.startsWith(SUBAGENT_TOOL_PREFIX)) {
      return Bot;
    }
    if (name in TOOL_ICONS) {
      return TOOL_ICONS[name];
    }
    if (name.startsWith("mcp__")) {
      return Plug;
    }
  }
  return Wrench;
}

/** A step is mid-flight while the runtime reports it as running or thinking. */
function isEntryActive(entry: TranscriptEntry) {
  return entry.status === "running" || entry.status === "thinking";
}

/** The icon that replaces an entry's text label; carries the name as a tooltip. */
function EntryIcon({ entry, active }: { entry: TranscriptEntry; active?: boolean }) {
  const Icon = entryIcon(entry);
  return (
    <span title={entryTitle(entry)} className="flex shrink-0 items-center">
      <Icon
        aria-hidden
        className={cn("size-4", active ? "text-foreground" : "text-muted-foreground")}
      />
    </span>
  );
}

function EntryDetail({ entry }: { entry: TranscriptEntry }) {
  if (!entry.detail) {
    return null;
  }
  return <span className="truncate font-mono text-xs text-muted-foreground">{entry.detail}</span>;
}

type EntryTimingMode = "start-and-duration" | "duration" | "end";

/** Wall-clock time and/or elapsed duration for a message, e.g. `14:03 · 3.2s`. */
function EntryTiming({
  entry,
  mode = "start-and-duration",
  className,
}: {
  entry: TranscriptEntry;
  mode?: EntryTimingMode;
  className?: string;
}) {
  const time =
    mode === "start-and-duration"
      ? formatClockTime(entry.startedAt)
      : mode === "end"
        ? formatClockTime(entry.endedAt)
        : undefined;
  const duration =
    mode === "start-and-duration" || mode === "duration"
      ? formatDuration(entry.startedAt, entry.endedAt)
      : undefined;
  if (!time && !duration) {
    return null;
  }
  return (
    <span className={cn("shrink-0 text-xs text-muted-foreground tabular-nums", className)}>
      {time}
      {time && duration ? " · " : null}
      {duration}
    </span>
  );
}

function EntryStatus({ entry }: { entry: TranscriptEntry }) {
  if (!entry.status) {
    return null;
  }
  return (
    <Badge variant={entry.status === "running" ? "warning" : "secondary"}>{entry.status}</Badge>
  );
}

function hasFileDiffArtifacts(entry: TranscriptEntry) {
  return entry.artifacts?.some((artifact) => artifact.type === "file_diff") ?? false;
}

function ToolEntryContent({ entry }: { entry: TranscriptEntry }) {
  const fileDiffs = entry.artifacts?.filter((artifact) => artifact.type === "file_diff") ?? [];
  const theme = useSyncExternalStore(subscribeThemeChange, () =>
    resolveTheme(getStoredThemePreference()),
  );
  return (
    <div className="space-y-3">
      {entry.command ? (
        <div className="space-y-1">
          <div className="text-xs font-medium text-muted-foreground">Command</div>
          <pre className="whitespace-pre-wrap break-words rounded-md border border-border/70 bg-background/70 p-2 pr-10 font-mono text-xs leading-5">
            {entry.command}
          </pre>
        </div>
      ) : null}
      {fileDiffs.length === 0 ? (
        <pre className="whitespace-pre-wrap break-words pr-10 font-sans text-sm leading-6">
          {entry.content}
        </pre>
      ) : null}
      {fileDiffs.map((artifact) => (
        <div
          key={`${artifact.operation}:${artifact.path}`}
          className="overflow-hidden rounded-md border"
        >
          <Suspense fallback={<div className="p-3 text-sm text-muted-foreground">Loading…</div>}>
            <VellumPatchDiff patch={artifact.patch} theme={theme} />
          </Suspense>
        </div>
      ))}
    </div>
  );
}

function CopyContentButton({ content, label = "content" }: { content: string; label?: string }) {
  const [copied, setCopied] = useState(false);

  if (content.trim().length === 0) {
    return null;
  }

  async function copyContent() {
    await navigator.clipboard.writeText(content);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1200);
  }

  return (
    <Button
      type="button"
      variant="quiet"
      size="icon"
      className="size-7"
      title={copied ? "Copied" : `Copy ${label}`}
      onClick={copyContent}
    >
      {copied ? <Check className="size-4" /> : <Copy className="size-4" />}
    </Button>
  );
}

function MessageActions({
  content,
  label,
  align,
  onEdit,
  forkFrom,
}: {
  content: string;
  label: string;
  align: "start" | "end";
  /** Present only on a user message the session is currently able to rewind to. */
  onEdit?: () => void;
  /** The message to branch from, on a user message that has turns before it and
   * an id the server has acknowledged. */
  forkFrom?: TranscriptEntry;
}) {
  return (
    <div
      className={cn(
        "flex h-8 items-center gap-1 px-1 opacity-0 transition-opacity group-hover/message:opacity-100 group-focus-within/message:opacity-100",
        align === "end" ? "justify-end" : "justify-start",
      )}
    >
      {onEdit ? (
        <Button
          size="icon"
          variant="ghost"
          className="size-7 text-muted-foreground hover:text-foreground"
          title="Edit and rewind to here"
          aria-label="Edit and rewind to here"
          onClick={onEdit}
        >
          <Pencil className="size-4" />
        </Button>
      ) : null}
      {forkFrom ? <ForkButton entry={forkFrom} /> : null}
      <CopyContentButton content={content} label={label} />
    </div>
  );
}

/** Disabled while a fork of this session is in flight — from *any* entry, since
 * the session has one per eligible user message plus the sidebar's and the
 * server mints a new id per request. A success navigates to the copy, so only
 * the failure has anything left to say here. */
function ForkButton({ entry }: { entry: TranscriptEntry }) {
  const key = useCodaStore(selectActiveForkKey);
  const forking = useCodaStore(selectForking(key ?? ""));
  const [error, setError] = useState<string>();
  return (
    <>
      <Button
        size="icon"
        variant="ghost"
        disabled={forking}
        className="size-7 text-muted-foreground hover:text-foreground"
        title="Branch a copy from before this message"
        aria-label="Branch a copy from before this message"
        onClick={() => {
          setError(undefined);
          forkActiveSession(entry.messageId!, {
            text: entry.content,
            images: entry.images ?? [],
          }).catch((err: unknown) => setError(err instanceof Error ? err.message : "Fork failed"));
        }}
      >
        <GitBranch className="size-4" />
      </Button>
      {error ? <span className="px-1 text-xs text-destructive">{error}</span> : null}
    </>
  );
}

/** A process step: either a plain entry or a grouped sub-agent invocation. */
type ProcessItem =
  | { type: "entry"; entry: TranscriptEntry }
  | {
      type: "subagent";
      key: string;
      agentName: string;
      /**
       * The parent `coda` entry for this invocation. It starts as a `tool_call`
       * while the sub-agent runs and is converted in place to a `tool_result`
       * once the reply lands — so its kind tells us whether the run is done.
       */
      callEntry?: TranscriptEntry;
      entries: TranscriptEntry[];
    };

type SubAgentItem = Extract<ProcessItem, { type: "subagent" }>;

/**
 * Fold a flat process timeline into items, collapsing each sub-agent run into a
 * single group. The anchor is the `coda` tool entry whose name carries the
 * sub-agent prefix — a `tool_call` while it runs, an in-place `tool_result` once
 * it replies. A sub-agent's own events attach to the open group with the
 * matching agent name, *not* merely the ones that happen to follow — so several
 * sub-agents invoked in one batch (whose events interleave) each land under
 * their own invocation. Sequential runs of the same agent split correctly too;
 * only truly concurrent same-name runs degrade (events fold into the latest).
 *
 * Works identically for live turns and resumed history (where only the anchor
 * survives, with no inner process), since the prefix self-identifies it.
 */
export function groupProcessItems(entries: TranscriptEntry[]): ProcessItem[] {
  const items: ProcessItem[] = [];
  // Open sub-agent groups keyed by agent display name.
  const openByName = new Map<string, SubAgentItem>();

  for (const entry of entries) {
    const isAnchor =
      (entry.kind === "tool_call" || entry.kind === "tool_result") &&
      isSubAgentToolName(entry.title);

    // A `coda`-issued prefixed tool entry opens a top-level sub-agent group.
    if (isAnchor && !isSubAgentEntry(entry)) {
      const agentName = subAgentDisplayName(entry.title as string);
      const group: SubAgentItem = {
        type: "subagent",
        key: entry.id,
        agentName,
        callEntry: entry,
        entries: [],
      };
      items.push(group);
      openByName.set(agentName, group);
      continue;
    }

    // A sub-agent's own event attaches to its matching open group (opening a
    // fallback group if no anchor survived, e.g. an orphaned resumed run).
    if (isSubAgentEntry(entry)) {
      const name = entry.agentName ?? "sub-agent";
      let group = openByName.get(name);
      if (!group) {
        group = { type: "subagent", key: entry.id, agentName: name, entries: [] };
        items.push(group);
        openByName.set(name, group);
      }
      group.entries.push(entry);
      // A nested sub-agent invocation: route the nested agent's own events into
      // this same group rather than letting them surface at the top level.
      if (isAnchor) {
        openByName.set(subAgentDisplayName(entry.title as string), group);
      }
      continue;
    }

    items.push({ type: "entry", entry });
  }

  return items;
}

/** One step inside a process disclosure (assistant prose inline, rest collapsed). */
function ProcessEntry({ entry }: { entry: TranscriptEntry }) {
  if (entry.kind === "assistant") {
    return <Markdown>{entry.content}</Markdown>;
  }
  return <TranscriptDisclosure entry={entry} />;
}

function latestActiveProcessEntry(entries: TranscriptEntry[]) {
  for (let index = entries.length - 1; index >= 0; index -= 1) {
    const entry = entries[index];
    if (entry.status === "thinking" || entry.status === "running") {
      return entry;
    }
  }
  return entries.at(-1);
}

function processStepText(stepCount: number) {
  return `${stepCount} ${stepCount === 1 ? "step" : "steps"}`;
}

/**
 * Count actual tool invocations behind a turn's process items, rather than
 * items themselves — reasoning and intermediate assistant prose aren't
 * operations. A sub-agent group counts its own invocation (the `agent__*`
 * call) plus every tool entry nested inside it; a nested sub-agent's own
 * invocation is itself a tool entry already flattened into that same
 * `entries` list by `groupProcessItems`, so no recursion is needed.
 */
export function processStepCount(items: ProcessItem[]): number {
  return items.reduce((total, item) => {
    if (item.type === "entry") {
      return total + (isToolEntry(item.entry) ? 1 : 0);
    }
    const ownCall = item.callEntry && isToolEntry(item.callEntry) ? 1 : 0;
    const nested = item.entries.filter(isToolEntry).length;
    return total + ownCall + nested;
  }, 0);
}

type Span = { start: number; end: number };

function parseSpan(from: string | null | undefined, to: string | null | undefined) {
  const start = from ? Date.parse(from) : NaN;
  const end = to ? Date.parse(to) : NaN;
  if (Number.isNaN(start) || Number.isNaN(end)) {
    return undefined;
  }
  return { start, end: Math.max(start, end) };
}

/** Every stretch of time an entry accounts for: its own, and the generation
 * behind it that may have left no entry of its own. */
function entrySpans(entry: TranscriptEntry): Span[] {
  const spans = [
    parseSpan(entry.startedAt, entry.endedAt),
    parseSpan(entry.generation?.startedAt, entry.generation?.endedAt),
  ];
  return spans.filter((span) => span !== undefined);
}

/**
 * How long the turn was actually working: the *union* of its entries' spans.
 *
 * Not a sum, because tool calls run concurrently and would count one stretch of
 * time many times over. Not the span from first start to last end, because the
 * turn also sits idle — waiting for a human to approve a call is not work, and
 * no entry covers that gap.
 *
 * Approval waits *inside* a sub-agent do still count: the parent's `agent__*`
 * entry spans the whole invocation, suspension included, and there is nothing
 * finer to fall back on once history drops the sub-agent's own steps.
 */
export function processWorkMs(entries: TranscriptEntry[]): number | undefined {
  const spans = entries.flatMap(entrySpans).sort((a, b) => a.start - b.start);
  if (spans.length === 0) {
    return undefined;
  }

  let total = 0;
  let { start: mergedStart, end: mergedEnd } = spans[0];
  for (const span of spans.slice(1)) {
    if (span.start <= mergedEnd) {
      mergedEnd = Math.max(mergedEnd, span.end);
      continue;
    }
    total += mergedEnd - mergedStart;
    mergedStart = span.start;
    mergedEnd = span.end;
  }
  return total + (mergedEnd - mergedStart);
}

function processEntrySummary(entry: TranscriptEntry | undefined) {
  if (!entry) {
    return { title: "Working" };
  }
  if (entry.kind === "reasoning") {
    return { title: "Thinking" };
  }
  if (entry.kind === "tool_call" || entry.kind === "tool_result") {
    return { title: entryTitle(entry), detail: entry.detail };
  }
  if (entry.kind === "assistant") {
    return { title: "Responding" };
  }
  return { title: entryTitle(entry), detail: entry.detail };
}

/** A collapsed disclosure gathering an entire sub-agent run under its name. */
function SubAgentGroup({ item }: { item: Extract<ProcessItem, { type: "subagent" }> }) {
  // The invocation entry flips from `tool_call` to `tool_result` when the
  // sub-agent replies; until then it's still working, so open it while it runs.
  // Orphaned runs (no anchor) fall back to whether any inner step is still live.
  const complete = item.callEntry
    ? item.callEntry.kind === "tool_result"
    : !item.entries.some((entry) => entry.status === "running" || entry.status === "thinking");
  const [open, setOpen] = useState(!complete);
  const previousComplete = useRef(complete);

  useEffect(() => {
    if (previousComplete.current === complete) {
      return;
    }
    previousComplete.current = complete;
    setOpen(!complete);
  }, [complete]);

  const task = item.callEntry?.detail;
  const stepCount = item.entries.filter(isToolEntry).length;
  // Resumed history keeps no inner process — surface the reply that survived so
  // the group isn't empty when expanded. Checked against the raw entry count,
  // not the tool-only `stepCount`, since a resumed run with no tool calls but
  // some other leftover entry still has an inner process to show.
  const showResultOnly = item.entries.length === 0 && item.callEntry?.kind === "tool_result";

  return (
    <Collapsible open={open} onOpenChange={setOpen}>
      <article className="rounded-md border border-border bg-card">
        <CollapsibleTrigger asChild>
          <button
            type="button"
            className="flex w-full items-center justify-between gap-3 rounded-md px-2 py-1.5 text-left text-muted-foreground hover:text-foreground"
            title={open ? "Collapse sub-agent" : "Expand sub-agent"}
          >
            <div className="flex min-w-0 items-center gap-2">
              <Bot
                aria-hidden
                className={cn(
                  "size-4 shrink-0",
                  complete ? "text-muted-foreground" : "text-foreground",
                )}
              />
              <span
                className={cn("shrink-0 truncate text-sm font-medium", !complete && "text-shimmer")}
              >
                {item.agentName}
              </span>
              <Badge variant="cyan" className="shrink-0 whitespace-nowrap">
                agent
              </Badge>
              {task ? <span className="truncate text-xs text-muted-foreground">{task}</span> : null}
            </div>
            <div className="flex shrink-0 items-center gap-2">
              <Badge variant={complete ? "secondary" : "warning"}>
                {!complete
                  ? "running"
                  : stepCount > 0
                    ? `${stepCount} ${stepCount === 1 ? "step" : "steps"}`
                    : "done"}
              </Badge>
              {open ? <ChevronDown className="size-4" /> : <ChevronRight className="size-4" />}
            </div>
          </button>
        </CollapsibleTrigger>
        <CollapsibleContent>
          <div className="space-y-2 px-2 pb-2">
            {showResultOnly && item.callEntry ? (
              // Resumed history keeps no inner process, only the reply the
              // sub-agent returned — render it as prose, not a nested tool row.
              <Markdown>{item.callEntry.content}</Markdown>
            ) : (
              item.entries.map((entry) => <ProcessEntry key={entry.id} entry={entry} />)
            )}
          </div>
        </CollapsibleContent>
      </article>
    </Collapsible>
  );
}

/** Placeholder shown between sending a task and the first streamed event, so the
 * turn reads as in-flight immediately instead of after the backend responds. */
function PendingTurnBubble() {
  return (
    <div className="group/message">
      <article className="rounded-md bg-background p-3 shadow-sm">
        <div className="flex items-center gap-3 py-1">
          <span className="truncate text-sm font-medium text-shimmer">Thinking…</span>
        </div>
      </article>
    </div>
  );
}

function sameEntries(previous: TranscriptEntry[], next: TranscriptEntry[]) {
  return previous.length === next.length && previous.every((entry, index) => entry === next[index]);
}

const AssistantTurnBubble = memo(
  function AssistantTurnBubble({
    entries,
    approvalPending,
    settled,
  }: {
    entries: TranscriptEntry[];
    approvalPending: boolean;
    /** Whether this turn is over, which nothing in the turn itself can say: an
     * abort leaves a marker, but only as a live event, and if every tool happened
     * to finish cleanly before the cancel landed the stored turn is
     * indistinguishable from one still in flight. So it comes from the caller,
     * which knows whether this is the last turn and whether the session is going. */
    settled: boolean;
  }) {
    const finalAssistantIndex = finalAssistantIndexOf(entries);
    const finalAssistant = finalAssistantIndex >= 0 ? entries[finalAssistantIndex] : undefined;
    const completedFinalAssistant = finalAssistant?.isFinalResponse ? finalAssistant : undefined;
    const processComplete = completedFinalAssistant !== undefined || settled;
    const intermediateEntries =
      finalAssistantIndex >= 0
        ? entries.filter((_, index) => index !== finalAssistantIndex)
        : entries;
    const processItems = groupProcessItems(intermediateEntries);
    const [processOpen, setProcessOpen] = useState(false);
    const activeProcessEntry = latestActiveProcessEntry(intermediateEntries);
    const activeSummary = approvalPending
      ? { title: "Approval required" }
      : processEntrySummary(activeProcessEntry);
    const stepText = processStepText(processStepCount(processItems));
    // The process only: writing the answer is the turn's product, not work spent
    // getting there. Its reasoning entry stays in — only the prose drops out.
    const workMs = processWorkMs(intermediateEntries);
    const duration = workMs === undefined ? undefined : formatElapsed(workMs);
    const processTitle = processComplete
      ? duration
        ? `Worked for ${duration} with ${stepText}`
        : `Worked with ${stepText}`
      : activeSummary.title;

    return (
      <div className="group/message">
        <article className="rounded-md bg-background p-3 shadow-sm">
          <div className="space-y-3">
            <Collapsible open={processOpen} onOpenChange={setProcessOpen}>
              <CollapsibleTrigger asChild>
                <button
                  type="button"
                  className="flex w-full items-center justify-between gap-3 rounded-md py-1 text-left text-muted-foreground hover:text-foreground"
                  title={processOpen ? "Collapse process" : "Expand process"}
                >
                  <div className="flex min-w-0 items-center gap-2">
                    {processComplete ? (
                      <span className="truncate text-sm font-medium">{processTitle}</span>
                    ) : (
                      <span className="truncate text-sm font-medium text-shimmer">
                        {activeSummary.detail ?? processTitle}
                      </span>
                    )}
                  </div>
                  {processOpen ? (
                    <ChevronDown className="size-4 shrink-0" />
                  ) : (
                    <ChevronRight className="size-4 shrink-0" />
                  )}
                </button>
              </CollapsibleTrigger>
              <CollapsibleContent>
                <div className="mt-2 space-y-2 px-1">
                  {processItems.map((item) =>
                    item.type === "subagent" ? (
                      <SubAgentGroup key={item.key} item={item} />
                    ) : (
                      <ProcessEntry key={item.entry.id} entry={item.entry} />
                    ),
                  )}
                </div>
              </CollapsibleContent>
            </Collapsible>
            {finalAssistant ? <Markdown>{finalAssistant.content}</Markdown> : null}
          </div>
        </article>
        {finalAssistant ? (
          <div className="flex items-center gap-1">
            <MessageActions content={finalAssistant.content} label="response" align="start" />
            <EntryTiming entry={finalAssistant} mode="end" className={cn("px-1", HOVER_REVEAL)} />
          </div>
        ) : null}
      </div>
    );
  },
  (previous, next) =>
    previous.approvalPending === next.approvalPending &&
    previous.settled === next.settled &&
    sameEntries(previous.entries, next.entries),
);

function TranscriptDisclosure({ entry }: { entry: TranscriptEntry }) {
  const [open, setOpen] = useState(false);
  const title = disclosureTitle(entry);
  const active = isEntryActive(entry);
  const hasFileDiff = hasFileDiffArtifacts(entry);

  return (
    <Collapsible open={open} onOpenChange={setOpen}>
      <CollapsibleTrigger asChild>
        <button
          type="button"
          className="flex w-full items-center justify-between gap-3 rounded-md py-1 text-left text-muted-foreground hover:text-foreground"
          title={open ? "Collapse" : "Expand"}
        >
          <div className="flex min-w-0 flex-1 items-center gap-2">
            <EntryIcon entry={entry} active={active} />
            {entry.detail ? (
              <span
                className={cn(
                  "truncate font-mono text-xs",
                  active ? "text-shimmer" : "text-muted-foreground",
                )}
              >
                {entry.detail}
              </span>
            ) : (
              <span className={cn("shrink-0 truncate text-sm", active && "text-shimmer")}>
                {title}
              </span>
            )}
            <EntryTiming entry={entry} mode="duration" />
          </div>
          <div className="grid shrink-0 grid-cols-[6.5rem_1.75rem] items-center gap-2">
            <div className="flex justify-end">
              <EntryStatus entry={entry} />
            </div>
            <div className="flex size-7 items-center justify-center">
              {open ? <ChevronDown className="size-4" /> : <ChevronRight className="size-4" />}
            </div>
          </div>
        </button>
      </CollapsibleTrigger>
      <CollapsibleContent>
        <div
          className={cn(
            "relative mt-1 max-h-64 overflow-auto rounded-md",
            !hasFileDiff && "border border-border bg-muted/20 p-3",
          )}
        >
          {entry.kind === "tool_result" && !hasFileDiff ? (
            <div className="sticky top-0 z-10 h-0">
              <div className="flex justify-end">
                <CopyContentButton content={entry.content} label="result" />
              </div>
            </div>
          ) : null}
          {entry.kind === "assistant" ? (
            <Markdown>{entry.content}</Markdown>
          ) : entry.kind === "reasoning" ? (
            <div className="text-muted-foreground">
              <Markdown>{entry.content}</Markdown>
            </div>
          ) : (
            <ToolEntryContent entry={entry} />
          )}
        </div>
      </CollapsibleContent>
    </Collapsible>
  );
}

function disclosureTitle(entry: TranscriptEntry) {
  if (entry.kind === "assistant") {
    return `${entry.agentName ?? "coda"} message`;
  }
  return entryTitle(entry);
}

function UserMessageBubble({ entry, forkable }: { entry: TranscriptEntry; forkable: boolean }) {
  const [lightboxIndex, setLightboxIndex] = useState<number | null>(null);
  const canRewind = useCodaStore(selectCanRewind);
  const messageId = entry.messageId;
  const getImageLayoutId = (index: number) => imageLightboxLayoutId(index, entry.images?.[index]);

  return (
    <LayoutGroup id={`message-${entry.id}`}>
      <div className="group/message flex flex-col items-end">
        <div className="max-w-[82%] space-y-2">
          {entry.images && entry.images.length > 0 && (
            <div className="flex flex-wrap justify-end gap-2">
              {entry.images.map((src, index) => (
                <button
                  key={index}
                  type="button"
                  title="View full size"
                  aria-label={`View image ${index + 1} full size`}
                  onClick={() => setLightboxIndex(index)}
                  className="block"
                >
                  <motion.img
                    layoutId={getImageLayoutId(index)}
                    transition={IMAGE_LIGHTBOX_TRANSITION}
                    src={src}
                    alt={`Image ${index + 1}`}
                    className="h-20 w-20 rounded-md border border-border/40 object-cover shadow-sm"
                  />
                </button>
              ))}
            </div>
          )}
          {entry.content && (
            <div className="rounded-md bg-primary px-3.5 py-2 text-primary-foreground shadow-sm">
              <Markdown>{entry.content}</Markdown>
            </div>
          )}
        </div>
        <div className="flex items-center justify-end gap-1">
          <EntryTiming entry={entry} className={cn("px-1", HOVER_REVEAL)} />
          <MessageActions
            content={entry.content}
            label="message"
            align="end"
            onEdit={
              canRewind && messageId && parseCompactCommand(entry.content) === null
                ? () => beginEdit(messageId)
                : undefined
            }
            forkFrom={forkable && messageId ? entry : undefined}
          />
        </div>
        {lightboxIndex !== null && entry.images && (
          <ImageLightbox
            images={entry.images}
            initialIndex={lightboxIndex}
            getLayoutId={getImageLayoutId}
            onClose={() => setLightboxIndex(null)}
          />
        )}
      </div>
    </LayoutGroup>
  );
}

const TranscriptItem = memo(function TranscriptItem({
  entry,
  forkable,
}: {
  entry: TranscriptEntry;
  forkable?: boolean;
}) {
  const [toolResultOpen, setToolResultOpen] = useState(false);

  if (entry.kind === "user") {
    return <UserMessageBubble entry={entry} forkable={forkable === true} />;
  }

  if (entry.kind === "compaction") {
    return (
      <Collapsible open={toolResultOpen} onOpenChange={setToolResultOpen}>
        <div className="flex items-center gap-3 py-2 text-muted-foreground">
          <div className="h-px flex-1 bg-border" />
          <CollapsibleTrigger asChild>
            <Button
              variant="quiet"
              size="sm"
              className="h-7 gap-1.5 px-2 text-xs"
              title={toolResultOpen ? "Hide compaction summary" : "Show compaction summary"}
            >
              <span>Context compacted</span>
              {toolResultOpen ? (
                <ChevronDown className="size-3.5" />
              ) : (
                <ChevronRight className="size-3.5" />
              )}
            </Button>
          </CollapsibleTrigger>
          <div className="h-px flex-1 bg-border" />
        </div>
        <CollapsibleContent>
          <article className="mx-auto mb-2 max-w-3xl rounded-md border border-border/70 bg-muted/20 p-3 text-sm">
            <Markdown>{entry.content}</Markdown>
          </article>
        </CollapsibleContent>
      </Collapsible>
    );
  }

  const tone =
    entry.kind === "error" ? "border-rose-500/35 bg-rose-500/10" : "border-border bg-card";

  const title = entryTitle(entry);
  const header = (
    <div className="mb-2 flex items-center justify-between gap-3">
      <div className="flex min-w-0 items-center gap-2">
        <EntryIcon entry={entry} />
        <span className="shrink-0 truncate text-sm font-medium">{title}</span>
        <EntryDetail entry={entry} />
        {entry.agentName && entry.agentName !== "coda" ? <Badge variant="cyan">agent</Badge> : null}
      </div>
      <div className="flex shrink-0 items-center gap-2">
        <EntryStatus entry={entry} />
      </div>
    </div>
  );

  if (entry.kind === "tool_result") {
    const hasFileDiff = hasFileDiffArtifacts(entry);
    return (
      <article className={cn("rounded-md border p-3 shadow-sm", tone)}>
        <Collapsible open={toolResultOpen} onOpenChange={setToolResultOpen}>
          <div className="mb-2 flex items-center justify-between gap-3">
            <div className="flex min-w-0 items-center gap-2">
              <EntryIcon entry={entry} />
              <span className="shrink-0 truncate text-sm font-medium">{title}</span>
              <EntryDetail entry={entry} />
            </div>
            <div className="flex shrink-0 items-center gap-2">
              <EntryTiming entry={entry} mode="duration" />
              <EntryStatus entry={entry} />
              <CollapsibleTrigger asChild>
                <Button
                  variant="quiet"
                  size="sm"
                  className="h-7 px-2"
                  title={toolResultOpen ? "Collapse result" : "Expand result"}
                >
                  {toolResultOpen ? (
                    <ChevronDown className="size-4" />
                  ) : (
                    <ChevronRight className="size-4" />
                  )}
                  <span>{toolResultOpen ? "Collapse" : "Expand"}</span>
                </Button>
              </CollapsibleTrigger>
            </div>
          </div>
          <CollapsibleContent>
            <div
              className={cn(
                "relative max-h-80 overflow-auto rounded-md md:max-h-96",
                !hasFileDiff && "border border-border/70 bg-background/70 p-3",
              )}
            >
              {!hasFileDiff ? (
                <div className="sticky top-0 z-10 h-0">
                  <div className="flex justify-end">
                    <CopyContentButton content={entry.content} label="result" />
                  </div>
                </div>
              ) : null}
              <ToolEntryContent entry={entry} />
            </div>
          </CollapsibleContent>
        </Collapsible>
      </article>
    );
  }

  if (entry.kind === "assistant") {
    return (
      <div className="group/message">
        <article className="rounded-md bg-background p-3 shadow-sm">
          <Markdown>{entry.content}</Markdown>
        </article>
        <div className="flex items-center gap-1">
          <MessageActions content={entry.content} label="response" align="start" />
          <EntryTiming entry={entry} mode="end" className={cn("px-1", HOVER_REVEAL)} />
        </div>
      </div>
    );
  }

  return (
    <article className={cn("rounded-md border p-3 shadow-sm", tone)}>
      {header}
      <pre className="whitespace-pre-wrap break-words font-sans text-sm leading-6">
        {entry.content}
      </pre>
    </article>
  );
});
