import {
  Check,
  ChevronDown,
  ChevronRight,
  Folder,
  Loader2,
  MessageSquareText,
  PanelLeft,
  Pencil,
  Plus,
  PlugZap,
  RotateCcw,
  Trash2,
  Unplug,
  X,
} from "lucide-react";
import { useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import {
  type ConnectionStatus,
  type OpenedSession,
  type ServerState,
  type SessionKey,
  type WorkspaceSummary,
} from "@/lib/session";
import { cn } from "@/lib/utils";
import { serverLabel, sessionTitle, statusCopy } from "@/components/session-utils";

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
              {serverLabel(server)}
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

export function Sidebar({
  servers,
  activeServer,
  activeKey,
  onConnectServer,
  onDisconnectServer,
  onRemoveServer,
  onRenameServer,
  onOpenSession,
  onStartNewSession,
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
  onStartNewSession: () => void;
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
              Sessions
            </h2>
            <div className="flex items-center gap-1">
              <Button
                variant="ghost"
                size="icon"
                className="size-6"
                onClick={onStartNewSession}
                title="New session"
              >
                <MessageSquareText className="size-4" />
              </Button>
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
