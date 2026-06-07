import {
  Ban,
  Check,
  ChevronDown,
  ChevronRight,
  CircleStop,
  Command,
  Cpu,
  Folder,
  KeyRound,
  Loader2,
  MessageSquareText,
  Pencil,
  Play,
  Plus,
  PlugZap,
  RotateCcw,
  Send,
  ShieldCheck,
  Sparkles,
  TerminalSquare,
  Trash2,
  X,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
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

function AddServerRow({
  onConnect,
  onCancel,
}: {
  onConnect: (serverUrl: string) => void;
  onCancel?: () => void;
}) {
  const [url, setUrl] = useState("ws://127.0.0.1:3000");

  function commit() {
    const value = url.trim();
    if (!value) {
      return;
    }
    onConnect(value);
    setUrl("ws://127.0.0.1:3000");
  }

  return (
    <div className="flex items-center gap-2 px-1 py-1">
      <Input
        autoFocus
        value={url}
        onChange={(event) => setUrl(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            commit();
          } else if (event.key === "Escape") {
            onCancel?.();
          }
        }}
        placeholder="ws://127.0.0.1:3000"
        className="h-8"
      />
      <Button
        size="icon"
        className="size-8 shrink-0"
        onClick={commit}
        disabled={!url.trim()}
        title="Connect"
      >
        <PlugZap />
      </Button>
      {onCancel ? (
        <Button
          variant="ghost"
          size="icon"
          className="size-8 shrink-0"
          onClick={onCancel}
          title="Cancel"
        >
          <X />
        </Button>
      ) : null}
    </div>
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
          <span className="flex size-4 shrink-0 items-center justify-center" title="Awaiting approval">
            <span className="size-2.5 animate-pulse rounded-full bg-amber-500" />
          </span>
        ) : running ? (
          <Loader2 className="size-4 shrink-0 animate-spin text-amber-500" />
        ) : (
          <MessageSquareText className="size-4 shrink-0 text-muted-foreground" />
        )}
        <span className="min-w-0 flex-1 truncate text-sm">{sessionTitle(session)}</span>
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
          ) : null}
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

  return (
    <aside className="flex min-h-0 w-full flex-col gap-2 border-r bg-card/55 p-2.5 lg:w-[256px]">
      <div className="flex items-center justify-between pl-1">
        <h2 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
          Servers
        </h2>
        <Button
          variant="ghost"
          size="icon"
          className="size-6"
          onClick={() => setAdding(true)}
          disabled={servers.length === 0}
          title="Add server"
        >
          <Plus className="size-4" />
        </Button>
      </div>
      <div className="scrollbar-fine min-h-0 flex-1 space-y-0.5 overflow-y-auto rounded-md border bg-background/70 p-1.5">
        {servers.length === 0 ? (
          <AddServerRow onConnect={onConnectServer} />
        ) : (
          <>
            {adding ? (
              <AddServerRow
                onConnect={(url) => {
                  onConnectServer(url);
                  setAdding(false);
                }}
                onCancel={() => setAdding(false)}
              />
            ) : null}
            {servers.map((server) => (
              <ServerNode
                key={server.url}
                server={server}
                activeServer={activeServer}
                activeKey={activeKey}
                onReconnect={onConnectServer}
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
          entries.map((entry) => (
            <TranscriptItem key={entry.id} entry={entry} />
          ))
        )}
        {running ? <WorkingIndicator /> : null}
        <div ref={bottomRef} />
      </div>
    </section>
  );
}

function TranscriptItem({ entry }: { entry: TranscriptEntry }) {
  if (entry.kind === "user") {
    return (
      <div className="flex justify-end">
        <div className="max-w-[82%] rounded-md bg-primary px-3.5 py-2 text-primary-foreground shadow-sm">
          <Markdown>{entry.content}</Markdown>
        </div>
      </div>
    );
  }

  const tone =
    entry.kind === "error"
      ? "border-rose-500/35 bg-rose-500/10"
      : entry.kind === "tool_call"
      ? "border-amber-500/35 bg-amber-500/10"
      : entry.kind === "tool_result"
      ? "border-emerald-500/30 bg-emerald-500/10"
      : "border-border bg-card";

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

  return (
    <article className={cn("rounded-md border p-3 shadow-sm", tone)}>
      <div className="mb-2 flex items-center justify-between gap-3">
        <div className="flex min-w-0 items-center gap-2">
          <Icon className="size-4 shrink-0 text-muted-foreground" />
          <span className="truncate text-sm font-medium">
            {entry.title ??
              (entry.agentName ? `${entry.agentName}` : entry.kind)}
          </span>
          {entry.agentName && entry.agentName !== "coda" ? (
            <Badge variant="cyan">sub-agent</Badge>
          ) : null}
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {entry.status ? (
            <Badge
              variant={entry.status === "running" ? "warning" : "secondary"}
            >
              {entry.status}
            </Badge>
          ) : null}
          {entry.usage ? (
            <Badge variant="outline">
              {entry.usage.prompt_tokens + entry.usage.completion_tokens} tokens
            </Badge>
          ) : null}
        </div>
      </div>
      {entry.kind === "assistant" ? (
        <Markdown>{entry.content}</Markdown>
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
      className="border-t bg-background/95 p-3 backdrop-blur"
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

function ApprovalPanel({
  approvals,
  onResolve,
  onAllowPattern,
}: {
  approvals: PendingApproval[];
  onResolve: (
    approval: PendingApproval,
    call: ToolCall,
    resolution: ToolCallResolution
  ) => void;
  onAllowPattern: (pattern: string) => void;
}) {
  if (approvals.length === 0) {
    return null;
  }
  return (
    <section className="scrollbar-fine max-h-[44vh] shrink-0 overflow-y-auto border-t border-amber-500/50 bg-amber-500/5 px-4 py-2.5">
      <div className="mx-auto w-full max-w-4xl space-y-2.5">
        <div className="flex items-center justify-between">
          <h2 className="flex items-center gap-2 text-sm font-medium">
            <ShieldCheck className="size-4 text-amber-600" />
            Approval required
          </h2>
          <Badge variant="warning">{approvals.length}</Badge>
        </div>
        {approvals.map((approval) => (
          <div
            key={`${approval.agent_name}:${approval.thread_id}`}
            className="space-y-3 rounded-md border bg-card p-3"
          >
            <div className="flex items-center justify-between gap-2">
              <div className="min-w-0">
                <div className="truncate text-sm font-medium">
                  {approval.agent_name}
                </div>
                <div className="truncate text-xs text-muted-foreground">
                  {approval.thread_id}
                </div>
              </div>
              <Badge variant="warning">{approval.calls.length} call(s)</Badge>
            </div>
            {approval.calls.map((call) => (
              <ApprovalCall
                key={call.id}
                approval={approval}
                call={call}
                onResolve={onResolve}
                onAllowPattern={onAllowPattern}
              />
            ))}
          </div>
        ))}
      </div>
    </section>
  );
}

function ApprovalCall({
  approval,
  call,
  onResolve,
  onAllowPattern,
}: {
  approval: PendingApproval;
  call: ToolCall;
  onResolve: (
    approval: PendingApproval,
    call: ToolCall,
    resolution: ToolCallResolution
  ) => void;
  onAllowPattern: (pattern: string) => void;
}) {
  const [reason, setReason] = useState("");
  const [answer, setAnswer] = useState("");
  const [allowPattern, setAllowPattern] = useState(() =>
    deriveAllowPattern(extractShellCommand(call))
  );
  const askUser = call.name === "ask_user" ? parseAskUserParams(call) : null;

  if (askUser) {
    return (
      <div className="space-y-3 rounded-md border bg-background p-3">
        <div className="flex items-center gap-2 text-sm font-medium">
          <KeyRound className="size-4 text-muted-foreground" />
          ask_user
        </div>
        <p className="text-sm leading-6">{askUser.question}</p>
        {askUser.options.length ? (
          <div className="grid gap-2">
            {askUser.options.map((option) => (
              <Button
                key={option}
                variant="outline"
                className="h-auto justify-start whitespace-normal py-2 text-left"
                onClick={() =>
                  onResolve(approval, call, { Resolved: { Ok: option } })
                }
              >
                <ChevronRight />
                {option}
              </Button>
            ))}
          </div>
        ) : null}
        <div className="space-y-2">
          <Textarea
            value={answer}
            onChange={(event) => setAnswer(event.target.value)}
            placeholder="Custom response"
          />
          <Button
            variant="secondary"
            className="w-full"
            disabled={!answer.trim()}
            onClick={() =>
              onResolve(approval, call, { Resolved: { Ok: answer.trim() } })
            }
          >
            <Check />
            Submit
          </Button>
        </div>
      </div>
    );
  }

  return (
    <div className="space-y-3 rounded-md border bg-background p-3">
      <div className="flex items-center justify-between gap-2">
        <div className="flex min-w-0 items-center gap-2 text-sm font-medium">
          <TerminalSquare className="size-4 shrink-0 text-muted-foreground" />
          <span className="truncate">{call.name}</span>
        </div>
        <Badge variant={call.name === "shell" ? "warning" : "outline"}>
          {call.name === "shell" ? "shell" : "tool"}
        </Badge>
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
              onResolve(approval, call, "Execute");
            }}
          >
            <ShieldCheck />
            Always
          </Button>
        </div>
      ) : null}
      <div className="grid grid-cols-2 gap-2">
        <Button
          variant="secondary"
          onClick={() => onResolve(approval, call, "Execute")}
        >
          <Play />
          Run
        </Button>
        <Button
          variant="outline"
          onClick={() =>
            onResolve(approval, call, {
              Rejected: { reason: reason.trim() ? reason.trim() : null },
            })
          }
        >
          <X />
          Reject
        </Button>
      </div>
      <Input
        value={reason}
        onChange={(event) => setReason(event.target.value)}
        placeholder="Rejection reason"
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
          <ApprovalPanel
            approvals={session.approvals}
            onResolve={session.resolveCall}
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
        </section>
      </main>
    </div>
  );
}
