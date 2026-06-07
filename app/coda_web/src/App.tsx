import {
  Ban,
  Check,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  CircleStop,
  Command,
  Copy,
  Cpu,
  Folder,
  KeyRound,
  Loader2,
  MessageSquareText,
  PanelLeft,
  Pencil,
  Plus,
  PlugZap,
  RotateCcw,
  Send,
  ShieldCheck,
  Sparkles,
  TerminalSquare,
  Trash2,
  Unplug,
  X,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Textarea } from "@/components/ui/textarea";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  approvalKey,
  callArguments,
  deriveAllowPattern,
  extractShellCommand,
  parseAskUserParams,
  type PendingApproval,
  type ToolCall,
  type ToolCallResolution,
} from "@/lib/protocol";
import {
  type ConnectionStatus,
  type OpenedSession,
  type ServerState,
  type SessionKey,
  type TranscriptEntry,
  type WorkspaceSummary,
  useCodaSession,
} from "@/lib/session";
import { cn } from "@/lib/utils";
import { Markdown } from "@/components/markdown";

const statusCopy: Record<
  ConnectionStatus,
  { label: string; tone: "secondary" | "success" | "warning" | "danger" }
> = {
  idle: { label: "Ready", tone: "secondary" },
  connecting: { label: "Connecting", tone: "warning" },
  connected: { label: "Connected", tone: "success" },
  closed: { label: "Disconnected", tone: "secondary" },
  error: { label: "Error", tone: "danger" },
};

function sessionTitle(session: {
  id: string;
  first_user_message?: string | null;
}) {
  return session.first_user_message?.trim() || session.id;
}

function formatArguments(value: string) {
  try {
    return JSON.stringify(JSON.parse(value), null, 2);
  } catch {
    return value;
  }
}

function WorkspaceHeader({ approvals }: { approvals: PendingApproval[] }) {
  return (
    <header className="flex h-11 shrink-0 items-center justify-between border-b bg-background/90 px-4 backdrop-blur">
      <div className="flex min-w-0 items-center gap-2">
        <div className="flex size-6 items-center justify-center rounded-md bg-primary text-primary-foreground shadow-sm">
          <Command className="size-3.5" />
        </div>
        <h1 className="truncate text-sm font-semibold tracking-normal">Coda</h1>
        <span className="size-1 rounded-full bg-muted-foreground/45" />
        <span className="text-xs text-muted-foreground">
          {approvals.length} pending approval(s)
        </span>
      </div>
    </header>
  );
}

function StatusDot({ status }: { status: ConnectionStatus }) {
  const tone =
    status === "connected"
      ? "bg-emerald-500"
      : status === "connecting"
      ? "bg-amber-500"
      : "bg-rose-500";
  return (
    <span
      className={cn(
        "size-2.5 shrink-0 rounded-full",
        tone,
        status === "connecting" && "animate-pulse"
      )}
      title={statusCopy[status].label}
    />
  );
}

function AddServerDialog({
  open,
  onOpenChange,
  onConnect,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onConnect: (serverUrl: string) => void;
}) {
  const defaultUrl = "ws://127.0.0.1:3000";
  const [url, setUrl] = useState(defaultUrl);

  function commit() {
    const value = url.trim();
    if (!value) {
      return;
    }
    onConnect(value);
    setUrl(defaultUrl);
    onOpenChange(false);
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(nextOpen) => {
        onOpenChange(nextOpen);
        if (!nextOpen) {
          setUrl(defaultUrl);
        }
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Add server</DialogTitle>
          <DialogDescription>
            Connect to a running Coda server by URL.
          </DialogDescription>
        </DialogHeader>
        <form
          className="space-y-4"
          onSubmit={(event) => {
            event.preventDefault();
            commit();
          }}
        >
          <div className="space-y-2">
            <label htmlFor="server-url" className="text-sm font-medium">
              Server URL
            </label>
            <Input
              id="server-url"
              autoFocus
              value={url}
              onChange={(event) => setUrl(event.target.value)}
              placeholder={defaultUrl}
            />
          </div>
          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => onOpenChange(false)}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={!url.trim()}>
              <PlugZap className="size-4" />
              Connect
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function SessionRow({
  serverUrl,
  workspaceId,
  session,
  isActive,
  running,
  awaitingApproval,
  disabled,
  onOpen,
  onDelete,
}: {
  serverUrl: string;
  workspaceId: string;
  session: WorkspaceSummary["sessions"][number];
  isActive: boolean;
  running: boolean;
  awaitingApproval: boolean;
  disabled: boolean;
  onOpen: (serverUrl: string, workspaceId: string, sessionId: string) => void;
  onDelete: (serverUrl: string, workspaceId: string, sessionId: string) => void;
}) {
  const [confirming, setConfirming] = useState(false);

  return (
    <div className="group flex items-center gap-1 pr-1">
      <Button
        variant={isActive ? "secondary" : "ghost"}
        className="h-auto min-w-0 flex-1 justify-start gap-2 px-2 py-1 text-left"
        disabled={disabled}
        onClick={() => onOpen(serverUrl, workspaceId, session.id)}
      >
        {awaitingApproval ? (
          <span
            className="flex size-4 shrink-0 items-center justify-center"
            title="Awaiting approval"
          >
            <span className="size-2.5 animate-pulse rounded-full bg-amber-500" />
          </span>
        ) : running ? (
          <Loader2 className="size-4 shrink-0 animate-spin text-amber-500" />
        ) : (
          <MessageSquareText className="size-4 shrink-0 text-muted-foreground" />
        )}
        <span className="min-w-0 flex-1 truncate text-sm">
          {sessionTitle(session)}
        </span>
      </Button>
      {confirming ? (
        <>
          <Button
            variant="ghost"
            size="icon"
            className="size-6 shrink-0 text-destructive"
            onClick={() => {
              setConfirming(false);
              onDelete(serverUrl, workspaceId, session.id);
            }}
            title="Confirm delete"
          >
            <Check className="size-4" />
          </Button>
          <Button
            variant="ghost"
            size="icon"
            className="size-6 shrink-0"
            onClick={() => setConfirming(false)}
            title="Cancel"
          >
            <X className="size-4" />
          </Button>
        </>
      ) : (
        <Button
          variant="ghost"
          size="icon"
          className="size-6 shrink-0 text-muted-foreground opacity-0 hover:text-destructive group-hover:opacity-100"
          onClick={() => setConfirming(true)}
          title="Delete session"
        >
          <Trash2 className="size-4" />
        </Button>
      )}
    </div>
  );
}

function WorkspaceNode({
  serverUrl,
  workspace,
  status,
  activeServer,
  activeKey,
  sessions: openedSessions,
  onOpenSession,
  onNewSession,
  onDeleteSession,
}: {
  serverUrl: string;
  workspace: WorkspaceSummary;
  status: ConnectionStatus;
  activeServer?: string;
  activeKey?: SessionKey;
  sessions: Record<SessionKey, OpenedSession>;
  onOpenSession: (
    serverUrl: string,
    workspaceId: string,
    sessionId: string
  ) => void;
  onNewSession: (serverUrl: string, workspaceId: string) => void;
  onDeleteSession: (
    serverUrl: string,
    workspaceId: string,
    sessionId: string
  ) => void;
}) {
  const [collapsed, setCollapsed] = useState(false);
  const sessions = [...workspace.sessions].sort(
    (a, b) =>
      (b.updated_at_ms ?? Number.POSITIVE_INFINITY) -
      (a.updated_at_ms ?? Number.POSITIVE_INFINITY)
  );

  return (
    <div className="space-y-0.5">
      <div className="flex items-center gap-1 pr-1 text-sm">
        <button
          type="button"
          className="flex min-w-0 flex-1 items-center gap-1.5 rounded-md px-1 py-1 text-left hover:bg-accent"
          onClick={() => setCollapsed((value) => !value)}
        >
          {collapsed ? (
            <ChevronRight className="size-4 shrink-0 text-muted-foreground" />
          ) : (
            <ChevronDown className="size-4 shrink-0 text-muted-foreground" />
          )}
          <Folder className="size-4 shrink-0 text-muted-foreground" />
          <span className="min-w-0 flex-1 truncate font-medium">
            {workspace.id}
          </span>
          <Badge variant="outline">{sessions.length}</Badge>
        </button>
        <Button
          variant="ghost"
          size="icon"
          className="size-6 shrink-0"
          disabled={status !== "connected"}
          onClick={() => onNewSession(serverUrl, workspace.id)}
          title="New session"
        >
          <Plus className="size-4" />
        </Button>
      </div>
      {!collapsed ? (
        <div className="space-y-0.5 pl-5">
          {sessions.map((session) => {
            const key: SessionKey = `${workspace.id}/${session.id}`;
            const opened = openedSessions[key];
            return (
              <SessionRow
                key={session.id}
                serverUrl={serverUrl}
                workspaceId={workspace.id}
                session={session}
                isActive={activeServer === serverUrl && key === activeKey}
                running={opened?.running ?? false}
                awaitingApproval={(opened?.approvals.length ?? 0) > 0}
                disabled={status !== "connected"}
                onOpen={onOpenSession}
                onDelete={onDeleteSession}
              />
            );
          })}
          {sessions.length === 0 ? (
            <div className="px-2 py-1 text-xs text-muted-foreground">
              No sessions yet
            </div>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

function ServerNode({
  server,
  activeServer,
  activeKey,
  onReconnect,
  onDisconnect,
  onRemove,
  onRename,
  onOpenSession,
  onNewSession,
  onDeleteSession,
}: {
  server: ServerState;
  activeServer?: string;
  activeKey?: SessionKey;
  onReconnect: (serverUrl: string) => void;
  onDisconnect: (serverUrl: string) => void;
  onRemove: (serverUrl: string) => void;
  onRename: (serverUrl: string, alias: string) => void;
  onOpenSession: (
    serverUrl: string,
    workspaceId: string,
    sessionId: string
  ) => void;
  onNewSession: (serverUrl: string, workspaceId: string) => void;
  onDeleteSession: (
    serverUrl: string,
    workspaceId: string,
    sessionId: string
  ) => void;
}) {
  const [collapsed, setCollapsed] = useState(false);
  const [editing, setEditing] = useState(false);
  const [aliasDraft, setAliasDraft] = useState(server.alias ?? "");

  function startEditing() {
    setAliasDraft(server.alias ?? "");
    setEditing(true);
  }

  function commitAlias() {
    onRename(server.url, aliasDraft);
    setEditing(false);
  }

  return (
    <div className="space-y-0.5">
      {editing ? (
        <div className="flex items-center gap-1 pr-1">
          <Input
            autoFocus
            value={aliasDraft}
            onChange={(event) => setAliasDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                commitAlias();
              } else if (event.key === "Escape") {
                setEditing(false);
              }
            }}
            placeholder={server.url}
            className="h-7 flex-1"
          />
          <Button
            variant="ghost"
            size="icon"
            className="size-6 shrink-0 text-emerald-600"
            onClick={commitAlias}
            title="Save name"
          >
            <Check className="size-4" />
          </Button>
          <Button
            variant="ghost"
            size="icon"
            className="size-6 shrink-0"
            onClick={() => setEditing(false)}
            title="Cancel"
          >
            <X className="size-4" />
          </Button>
        </div>
      ) : (
        <div className="group flex items-center gap-1 pr-1">
          <button
            type="button"
            className="flex min-w-0 flex-1 items-center gap-1.5 rounded-md px-1 py-1 text-left hover:bg-accent"
            onClick={() => setCollapsed((value) => !value)}
          >
            {collapsed ? (
              <ChevronRight className="size-4 shrink-0 text-muted-foreground" />
            ) : (
              <ChevronDown className="size-4 shrink-0 text-muted-foreground" />
            )}
            <StatusDot status={server.status} />
            <span
              className="min-w-0 flex-1 truncate text-sm font-medium"
              title={server.url}
            >
              {server.alias || server.url}
            </span>
          </button>
          <Button
            variant="ghost"
            size="icon"
            className="size-6 shrink-0 text-muted-foreground opacity-0 group-hover:opacity-100"
            onClick={startEditing}
            title="Rename server"
          >
            <Pencil className="size-4" />
          </Button>
          {server.status !== "connected" && server.status !== "connecting" ? (
            <Button
              variant="ghost"
              size="icon"
              className="size-6 shrink-0"
              onClick={() => onReconnect(server.url)}
              title="Reconnect"
            >
              <RotateCcw className="size-4" />
            </Button>
          ) : (
            <Button
              variant="ghost"
              size="icon"
              className="size-6 shrink-0"
              onClick={() => onDisconnect(server.url)}
              title="Disconnect"
            >
              <Unplug className="size-4" />
            </Button>
          )}
          <Button
            variant="ghost"
            size="icon"
            className="size-6 shrink-0"
            onClick={() => onRemove(server.url)}
            title="Remove server"
          >
            <X className="size-4" />
          </Button>
        </div>
      )}
      {!collapsed ? (
        <div className="space-y-0.5 pl-4">
          {server.catalog.map((workspace) => (
            <WorkspaceNode
              key={workspace.id}
              serverUrl={server.url}
              workspace={workspace}
              status={server.status}
              activeServer={activeServer}
              activeKey={activeKey}
              sessions={server.sessions}
              onOpenSession={onOpenSession}
              onNewSession={onNewSession}
              onDeleteSession={onDeleteSession}
            />
          ))}
          {server.catalog.length === 0 ? (
            <div className="px-2 py-1 text-xs text-muted-foreground">
              {server.status === "connected"
                ? "No workspaces"
                : statusCopy[server.status].label}
            </div>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

function Sidebar({
  servers,
  activeServer,
  activeKey,
  onConnectServer,
  onDisconnectServer,
  onRemoveServer,
  onRenameServer,
  onOpenSession,
  onNewSession,
  onDeleteSession,
}: {
  servers: ServerState[];
  activeServer?: string;
  activeKey?: SessionKey;
  onConnectServer: (serverUrl: string) => void;
  onDisconnectServer: (serverUrl: string) => void;
  onRemoveServer: (serverUrl: string) => void;
  onRenameServer: (serverUrl: string, alias: string) => void;
  onOpenSession: (
    serverUrl: string,
    workspaceId: string,
    sessionId: string
  ) => void;
  onNewSession: (serverUrl: string, workspaceId: string) => void;
  onDeleteSession: (
    serverUrl: string,
    workspaceId: string,
    sessionId: string
  ) => void;
}) {
  const [adding, setAdding] = useState(false);
  const [collapsed, setCollapsed] = useState(false);

  return (
    <aside
      className={cn(
        "flex min-h-0 w-full flex-col gap-2 border-r bg-card/55 p-2.5 transition-[width] lg:w-[256px]",
        collapsed && "lg:w-12"
      )}
    >
      <div className="flex items-center justify-between pl-1">
        {collapsed ? (
          <Button
            variant="ghost"
            size="icon"
            className="size-7"
            onClick={() => setCollapsed(false)}
            title="Expand servers"
          >
            <PanelLeft className="size-4" />
          </Button>
        ) : (
          <>
            <h2 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
              Servers
            </h2>
            <div className="flex items-center gap-1">
              <Button
                variant="ghost"
                size="icon"
                className="size-6"
                onClick={() => setAdding(true)}
                title="Add server"
              >
                <Plus className="size-4" />
              </Button>
              <Button
                variant="ghost"
                size="icon"
                className="size-6"
                onClick={() => setCollapsed(true)}
                title="Collapse servers"
              >
                <PanelLeft className="size-4" />
              </Button>
            </div>
          </>
        )}
      </div>
      {collapsed ? (
        <div className="min-h-0 flex-1" />
      ) : (
        <div className="scrollbar-fine min-h-0 flex-1 space-y-0.5 overflow-y-auto rounded-md bg-background/70 p-1.5">
          {servers.length === 0 ? (
            <div className="flex min-h-32 flex-col items-center justify-center gap-3 px-3 py-6 text-center">
              <div className="text-sm font-medium">No servers</div>
              <p className="text-xs leading-5 text-muted-foreground">
                Connect to a local or remote Coda server.
              </p>
              <Button size="sm" onClick={() => setAdding(true)}>
                <Plus className="size-4" />
                Add server
              </Button>
            </div>
          ) : (
            <>
              {servers.map((server) => (
                <ServerNode
                  key={server.url}
                  server={server}
                  activeServer={activeServer}
                  activeKey={activeKey}
                  onReconnect={onConnectServer}
                  onDisconnect={onDisconnectServer}
                  onRemove={onRemoveServer}
                  onRename={onRenameServer}
                  onOpenSession={onOpenSession}
                  onNewSession={onNewSession}
                  onDeleteSession={onDeleteSession}
                />
              ))}
            </>
          )}
        </div>
      )}
      <AddServerDialog
        open={adding}
        onOpenChange={setAdding}
        onConnect={onConnectServer}
      />
    </aside>
  );
}

function WorkingIndicator() {
  return (
    <div className="flex items-center gap-2 px-1 py-1 text-sm text-muted-foreground">
      <span className="flex gap-1">
        <span className="size-1.5 animate-bounce rounded-full bg-muted-foreground/60 [animation-delay:-0.3s]" />
        <span className="size-1.5 animate-bounce rounded-full bg-muted-foreground/60 [animation-delay:-0.15s]" />
        <span className="size-1.5 animate-bounce rounded-full bg-muted-foreground/60" />
      </span>
    </div>
  );
}

type TranscriptRenderItem =
  | { type: "entry"; entry: TranscriptEntry }
  | { type: "turn"; id: string; entries: TranscriptEntry[] };

function lastAssistantIndex(entries: TranscriptEntry[]) {
  for (let index = entries.length - 1; index >= 0; index -= 1) {
    if (entries[index].kind === "assistant") {
      return index;
    }
  }
  return -1;
}

function hasToolWork(entries: TranscriptEntry[]) {
  return entries.some(
    (entry) => entry.kind === "tool_call" || entry.kind === "tool_result"
  );
}

function turnGroup(entries: TranscriptEntry[]): TranscriptRenderItem {
  return {
    type: "turn",
    id: entries.map((entry) => entry.id).join(":"),
    entries,
  };
}

function transcriptRenderItems(
  entries: TranscriptEntry[]
): TranscriptRenderItem[] {
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
    if (!hasToolWork(segment)) {
      items.push(
        ...segment.map((entry) => ({ type: "entry" as const, entry }))
      );
      continue;
    }

    items.push(turnGroup(segment));
  }

  return items;
}

function Transcript({
  entries,
  running,
  workspace,
}: {
  entries: TranscriptEntry[];
  running: boolean;
  workspace?: string;
}) {
  const bottomRef = useRef<HTMLDivElement | null>(null);
  const renderItems = transcriptRenderItems(entries);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ block: "end" });
  }, [entries.length, entries.at(-1)?.content, running]);

  return (
    <section className="scrollbar-fine min-h-0 flex-1 overflow-y-auto px-4 py-3">
      <div className="mx-auto flex w-full max-w-4xl flex-col gap-2">
        {entries.length === 0 ? (
          <div className="flex min-h-[48vh] flex-col items-center justify-center text-center">
            <div className="mb-3 flex size-10 items-center justify-center rounded-md bg-accent text-accent-foreground">
              <Sparkles className="size-5" />
            </div>
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
          renderItems.map((item) =>
            item.type === "entry" ? (
              <TranscriptItem key={item.entry.id} entry={item.entry} />
            ) : (
              <AssistantTurnBubble key={item.id} entries={item.entries} />
            )
          )
        )}
        {running ? <WorkingIndicator /> : null}
        <div ref={bottomRef} />
      </div>
    </section>
  );
}

function entryTitle(entry: TranscriptEntry) {
  return entry.title ?? (entry.agentName ? `${entry.agentName}` : entry.kind);
}

function EntryIcon({ entry }: { entry: TranscriptEntry }) {
  const Icon =
    entry.kind === "assistant"
      ? MessageSquareText
      : entry.kind === "tool_call"
      ? TerminalSquare
      : entry.kind === "tool_result"
      ? ShieldCheck
      : entry.kind === "error"
      ? Ban
      : Cpu;

  return <Icon className="size-4 shrink-0 text-muted-foreground" />;
}

function EntryStatus({ entry }: { entry: TranscriptEntry }) {
  return (
    <>
      {entry.status ? (
        <Badge variant={entry.status === "running" ? "warning" : "secondary"}>
          {entry.status}
        </Badge>
      ) : null}
      {entry.usage ? (
        <Badge variant="outline">
          {entry.usage.prompt_tokens + entry.usage.completion_tokens} tokens
        </Badge>
      ) : null}
    </>
  );
}

function CopyContentButton({
  content,
  label = "content",
}: {
  content: string;
  label?: string;
}) {
  const [copied, setCopied] = useState(false);

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

function AssistantTurnBubble({ entries }: { entries: TranscriptEntry[] }) {
  const lastIndex = lastAssistantIndex(entries);
  const finalAssistantIndex = lastIndex === entries.length - 1 ? lastIndex : -1;
  const finalAssistant =
    finalAssistantIndex >= 0 ? entries[finalAssistantIndex] : undefined;
  const intermediateEntries =
    finalAssistantIndex >= 0
      ? entries.filter((_, index) => index !== finalAssistantIndex)
      : entries;
  const usage = finalAssistant?.usage;

  return (
    <article className="rounded-md border border-border bg-card p-3 shadow-sm">
      <div className="mb-3 flex items-center justify-between gap-3">
        <div className="flex min-w-0 items-center gap-2">
          <MessageSquareText className="size-4 shrink-0 text-muted-foreground" />
          <span className="truncate text-sm font-medium">
            {finalAssistant?.agentName ?? "coda"}
          </span>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {usage ? (
            <Badge variant="outline">
              {usage.prompt_tokens + usage.completion_tokens} tokens
            </Badge>
          ) : null}
        </div>
      </div>
      <div className="space-y-3">
        {intermediateEntries.map((entry) =>
          entry.kind === "assistant" ? (
            <Markdown key={entry.id}>{entry.content}</Markdown>
          ) : (
            <TranscriptDisclosure key={entry.id} entry={entry} />
          )
        )}
        {finalAssistant ? <Markdown>{finalAssistant.content}</Markdown> : null}
        {finalAssistant ? (
          <div className="flex justify-start">
            <CopyContentButton
              content={finalAssistant.content}
              label="response"
            />
          </div>
        ) : null}
      </div>
    </article>
  );
}

function TranscriptDisclosure({ entry }: { entry: TranscriptEntry }) {
  const [open, setOpen] = useState(false);
  const title = disclosureTitle(entry);

  return (
    <Collapsible open={open} onOpenChange={setOpen}>
      <CollapsibleTrigger asChild>
        <button
          type="button"
          className="flex w-full items-center justify-between gap-3 rounded-md py-1 text-left text-muted-foreground hover:text-foreground"
          title={open ? "Collapse" : "Expand"}
        >
          <div className="flex min-w-0 flex-1 items-center gap-2">
            <EntryIcon entry={entry} />
            <span className="truncate text-sm">{title}</span>
          </div>
          <div className="grid shrink-0 grid-cols-[6.5rem_1.75rem] items-center gap-2">
            <div className="flex justify-end">
              <EntryStatus entry={entry} />
            </div>
            <div className="flex size-7 items-center justify-center">
              {open ? (
                <ChevronDown className="size-4" />
              ) : (
                <ChevronRight className="size-4" />
              )}
            </div>
          </div>
        </button>
      </CollapsibleTrigger>
      <CollapsibleContent>
        <div className="relative mt-1 max-h-64 overflow-auto rounded-md border border-border bg-muted/20 p-3">
          {entry.kind === "tool_result" ? (
            <div className="sticky top-0 z-10 h-0">
              <div className="flex justify-end">
                <CopyContentButton content={entry.content} label="result" />
              </div>
            </div>
          ) : null}
          {entry.kind === "assistant" ? (
            <Markdown>{entry.content}</Markdown>
          ) : (
            <pre className="whitespace-pre-wrap break-words pr-10 font-sans text-sm leading-6">
              {entry.content}
            </pre>
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

function TranscriptItem({ entry }: { entry: TranscriptEntry }) {
  const [toolResultOpen, setToolResultOpen] = useState(false);

  if (entry.kind === "user") {
    return (
      <div className="group flex justify-end gap-1">
        <div className="pt-1 opacity-0 transition-opacity group-hover:opacity-100">
          <CopyContentButton content={entry.content} label="message" />
        </div>
        <div className="max-w-[82%] rounded-md bg-primary px-3.5 py-2 text-primary-foreground shadow-sm">
          <Markdown>{entry.content}</Markdown>
        </div>
      </div>
    );
  }

  const tone =
    entry.kind === "error"
      ? "border-rose-500/35 bg-rose-500/10"
      : "border-border bg-card";

  const title = entryTitle(entry);
  const header = (
    <div className="mb-2 flex items-center justify-between gap-3">
      <div className="flex min-w-0 items-center gap-2">
        <EntryIcon entry={entry} />
        <span className="truncate text-sm font-medium">{title}</span>
        {entry.agentName && entry.agentName !== "coda" ? (
          <Badge variant="cyan">sub-agent</Badge>
        ) : null}
      </div>
      <div className="flex shrink-0 items-center gap-2">
        <EntryStatus entry={entry} />
      </div>
    </div>
  );

  if (entry.kind === "tool_result") {
    return (
      <article className={cn("rounded-md border p-3 shadow-sm", tone)}>
        <Collapsible open={toolResultOpen} onOpenChange={setToolResultOpen}>
          <div className="mb-2 flex items-center justify-between gap-3">
            <div className="flex min-w-0 items-center gap-2">
              <EntryIcon entry={entry} />
              <span className="truncate text-sm font-medium">{title}</span>
            </div>
            <div className="flex shrink-0 items-center gap-2">
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
            <div className="relative max-h-80 overflow-auto rounded-md border border-border/70 bg-background/70 p-3 md:max-h-96">
              <div className="sticky top-0 z-10 h-0">
                <div className="flex justify-end">
                  <CopyContentButton content={entry.content} label="result" />
                </div>
              </div>
              <pre className="whitespace-pre-wrap break-words pr-10 font-sans text-sm leading-6">
                {entry.content}
              </pre>
            </div>
          </CollapsibleContent>
        </Collapsible>
      </article>
    );
  }

  return (
    <article className={cn("rounded-md border p-3 shadow-sm", tone)}>
      {header}
      {entry.kind === "assistant" ? (
        <div className="space-y-3">
          <Markdown>{entry.content}</Markdown>
          <div className="flex justify-start">
            <CopyContentButton content={entry.content} label="response" />
          </div>
        </div>
      ) : (
        <pre className="whitespace-pre-wrap break-words font-sans text-sm leading-6">
          {entry.content}
        </pre>
      )}
    </article>
  );
}
function Composer({
  status,
  running,
  workspace,
  workspaces,
  onChangeWorkspace,
  onSend,
  onAbort,
}: {
  status: ConnectionStatus;
  running: boolean;
  workspace?: string;
  workspaces: string[];
  onChangeWorkspace: (workspaceId: string) => void;
  onSend: (task: string) => void;
  onAbort: () => void;
}) {
  const [task, setTask] = useState("");
  const connected = status === "connected";

  function submit() {
    const text = task.trim();
    if (!text || !connected || running) {
      return;
    }
    onSend(text);
    setTask("");
  }

  return (
    <form
      className="bg-background/95 p-3 backdrop-blur"
      onSubmit={(event) => {
        event.preventDefault();
        submit();
      }}
    >
      {workspace ? (
        <div className="mx-auto mb-2 flex max-w-4xl items-center gap-2">
          <Select value={workspace} onValueChange={onChangeWorkspace}>
            <SelectTrigger
              size="sm"
              className="w-auto gap-1.5 rounded-md text-xs"
              disabled={!connected || workspaces.length === 0}
            >
              <Folder className="size-3.5 text-muted-foreground" />
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {workspaces.map((id) => (
                <SelectItem key={id} value={id}>
                  {id}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      ) : null}
      <div className="relative mx-auto max-w-4xl">
        <Textarea
          value={task}
          onChange={(event) => setTask(event.target.value)}
          onKeyDown={(event) => {
            if (
              event.key === "Enter" &&
              !event.shiftKey &&
              !event.nativeEvent.isComposing
            ) {
              event.preventDefault();
              submit();
            }
          }}
          placeholder="Ask Coda to edit, inspect, test, or explain…  (Enter to send, Shift+Enter for newline)"
          className="min-h-[52px] pr-12"
        />
        {running ? (
          <Button
            size="icon"
            variant="secondary"
            className="absolute bottom-2 right-2 size-8"
            type="button"
            onClick={onAbort}
            disabled={!connected}
            title="Abort"
          >
            <CircleStop />
          </Button>
        ) : (
          <Button
            size="icon"
            className="absolute bottom-2 right-2 size-8"
            type="submit"
            disabled={!connected || !task.trim()}
            title="Send"
          >
            <Send />
          </Button>
        )}
      </div>
    </form>
  );
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
        selected ? "border-primary bg-accent" : "border-input hover:bg-accent"
      )}
    >
      <RadioGroupItem value={value} />
      {label}
    </Label>
  );
}

function ApprovalPanel({
  approvals,
  drafts,
  onDraft,
  onSubmit,
  onAllowPattern,
}: {
  approvals: PendingApproval[];
  drafts: Record<string, Record<string, ToolCallResolution>>;
  onDraft: (
    approval: PendingApproval,
    call: ToolCall,
    resolution: ToolCallResolution
  ) => void;
  onSubmit: () => void;
  onAllowPattern: (pattern: string) => void;
}) {
  const items: ApprovalItem[] = approvals.flatMap((approval) =>
    approval.calls.map((call) => ({ approval, call }))
  );
  const [index, setIndex] = useState(0);

  // Reset to the first item whenever the pending set itself changes (a new
  // batch arrives) — but keep position while the user works through a batch.
  const itemsKey = items
    .map((item) => `${item.approval.thread_id}:${item.call.id}`)
    .join("|");
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

  const decisionOf = (item: ApprovalItem) =>
    drafts[approvalKey(item.approval)]?.[item.call.id];
  const current = items[Math.min(index, items.length - 1)] ?? items[0];
  const currentIndex = items.indexOf(current);
  const decidedCount = items.filter((item) => decisionOf(item)).length;
  const allDecided = decidedCount === items.length;

  const handleDraft = (resolution: ToolCallResolution) => {
    onDraft(current.approval, current.call, resolution);
    // Approving needs no follow-up, so jump ahead; rejecting stays put so the
    // user can fill in a reason.
    if (resolution === "Execute" && currentIndex < items.length - 1) {
      setIndex(currentIndex + 1);
    }
  };

  return (
    <div className="pointer-events-none absolute inset-x-0 bottom-full px-4">
      <div className="pointer-events-auto mx-auto w-full max-w-4xl overflow-hidden rounded-t-lg border border-b-0 border-amber-500/50 bg-card shadow-lg ring-1 ring-amber-500/10">
        <div className="flex max-h-[60vh] flex-col bg-amber-500/5">
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
              onDraft={handleDraft}
              onAllowPattern={onAllowPattern}
            />
          </div>
          <div className="flex items-center justify-between gap-2 border-t border-amber-500/20 px-4 py-2">
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
            <Button disabled={!allDecided} onClick={onSubmit}>
              <Check />
              Submit {decidedCount}/{items.length}
            </Button>
          </div>
        </div>
      </div>
    </div>
  );
}

function ApprovalCall({
  call,
  decision,
  onDraft,
  onAllowPattern,
}: {
  call: ToolCall;
  decision: ToolCallResolution | undefined;
  onDraft: (resolution: ToolCallResolution) => void;
  onAllowPattern: (pattern: string) => void;
}) {
  const [reason, setReason] = useState(() =>
    decision && decision !== "Execute" && "Rejected" in decision
      ? decision.Rejected.reason ?? ""
      : ""
  );
  const [answer, setAnswer] = useState("");
  const [allowPattern, setAllowPattern] = useState(() =>
    deriveAllowPattern(extractShellCommand(call))
  );
  const askUser = call.name === "ask_user" ? parseAskUserParams(call) : null;
  const approved = decision === "Execute";
  const rejected = Boolean(decision && decision !== "Execute" && "Rejected" in decision);

  if (askUser) {
    const chosen =
      decision && decision !== "Execute" && "Resolved" in decision
        ? "Ok" in decision.Resolved
          ? decision.Resolved.Ok
          : null
        : null;
    return (
      <div className="space-y-3 rounded-md border bg-background p-3">
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
    <div className="space-y-3 rounded-md border bg-background p-3">
      <div className="flex min-w-0 items-center gap-2 text-sm font-medium">
        <TerminalSquare className="size-4 shrink-0 text-muted-foreground" />
        <span className="truncate">{call.name}</span>
      </div>
      <pre className="max-h-44 overflow-auto rounded-md bg-muted p-3 text-xs leading-5 text-muted-foreground">
        {formatArguments(callArguments(call))}
      </pre>
      {call.name === "shell" ? (
        <div className="flex gap-2">
          <Input
            value={allowPattern}
            onChange={(event) => setAllowPattern(event.target.value)}
          />
          <Button
            variant="outline"
            onClick={() => {
              onAllowPattern(allowPattern);
              onDraft("Execute");
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
            onDraft({ Rejected: { reason: reason.trim() ? reason.trim() : null } });
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

export default function App() {
  const session = useCodaSession();

  const activeServerState = session.servers.find(
    (server) => server.url === session.activeServer
  );
  const workspaceIds = activeServerState?.catalog.map((ws) => ws.id) ?? [];

  return (
    <div className="flex h-screen min-h-[600px] flex-col overflow-hidden bg-background">
      <WorkspaceHeader approvals={session.approvals} />
      <main className="grid min-h-0 flex-1 grid-cols-1 lg:grid-cols-[256px_minmax(0,1fr)]">
        <Sidebar
          servers={session.servers}
          activeServer={session.activeServer}
          activeKey={session.activeKey}
          onConnectServer={session.connectServer}
          onDisconnectServer={session.disconnectServer}
          onRemoveServer={session.removeServer}
          onRenameServer={session.renameServer}
          onOpenSession={session.openSession}
          onNewSession={session.newSession}
          onDeleteSession={session.deleteSession}
        />
        <section className="flex min-h-0 flex-col">
          <Transcript
            entries={session.entries}
            running={session.running}
            workspace={session.activeWorkspace}
          />
          <div className="relative z-20 shrink-0">
            <ApprovalPanel
              approvals={session.approvals}
              drafts={session.drafts}
              onDraft={session.draftCall}
              onSubmit={session.submitApprovals}
              onAllowPattern={session.addAllowPattern}
            />
            <Composer
              status={session.status}
              running={session.running}
              workspace={session.activeWorkspace}
              workspaces={workspaceIds}
              onChangeWorkspace={(workspaceId) => {
                if (session.activeServer) {
                  session.newSession(session.activeServer, workspaceId);
                }
              }}
              onSend={session.sendTask}
              onAbort={session.abort}
            />
          </div>
        </section>
      </main>
    </div>
  );
}
