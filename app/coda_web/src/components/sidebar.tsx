import {
  Check,
  ChevronDown,
  ChevronRight,
  MoreHorizontal,
  PanelLeft,
  Pencil,
  Plug,
  Plus,
  PlugZap,
  RotateCcw,
  Trash,
  Unplug,
  X,
} from "lucide-react";
import { memo, useEffect, useState } from "react";
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
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
import { StatusDot, type DotTone } from "@/components/status-dot";
import {
  connectServer,
  deleteSession,
  disconnectServer,
  removeServer,
  renameServer,
  selectActiveKey,
  selectActiveServer,
  selectServers,
  useCodaShallow,
  useCodaStore,
  type ConnectionStatus,
  type OpenedSession,
  type ServerState,
  type SessionKey,
  type WorkspaceSummary,
} from "@/store/session";
import type { NewSessionTarget } from "@/store/new-session";
import { cn } from "@/lib/utils";
import { serverLabel, sessionTitle, statusCopy } from "@/components/session-utils";

function ServerStatusDot({ status }: { status: ConnectionStatus }) {
  const tone: DotTone =
    status === "connected" ? "online" : status === "connecting" ? "busy" : "offline";
  return (
    <StatusDot
      tone={tone}
      motion={status === "connecting" ? "breathe" : "static"}
      title={statusCopy[status].label}
    />
  );
}

/** Compact server entry shown in the collapsed rail: click starts a new session
 * under that server (in its first workspace). */
const CollapsedServerButton = memo(function CollapsedServerButton({
  url,
  onNewSession,
}: {
  url: string;
  onNewSession: (serverUrl: string, workspaceId: string) => void;
}) {
  const server = useCodaStore((state) => state.servers[url]);
  if (!server) {
    return null;
  }
  const firstWorkspace = server.catalog[0]?.id;
  return (
    <Button
      variant="ghost"
      size="icon"
      // The dot's color is itself the status indicator, so keep it at full
      // opacity even when the button is disabled (server not connected).
      className="size-6 disabled:opacity-100"
      disabled={server.status !== "connected" || !firstWorkspace}
      onClick={() => firstWorkspace && onNewSession(url, firstWorkspace)}
      title={`New session · ${serverLabel(server)}`}
    >
      <ServerStatusDot status={server.status} />
    </Button>
  );
});

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
      <DialogContent className="sm:max-w-sm">
        <DialogHeader>
          <DialogTitle>Add server</DialogTitle>
          <DialogDescription>Connect to a running Coda server by URL.</DialogDescription>
        </DialogHeader>
        <form
          className="space-y-4"
          onSubmit={(event) => {
            event.preventDefault();
            commit();
          }}
        >
          <Input
            id="server-url"
            autoFocus
            value={url}
            onChange={(event) => setUrl(event.target.value)}
            placeholder={defaultUrl}
          />
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
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

/** A connected server rendered as a collapsible group header; its workspaces
 * nest beneath it, so workspace rows no longer need an `@server` suffix.
 * Per-server management (rename / reconnect / disconnect / remove) lives in the
 * header's hover actions. */
const ServerGroup = memo(function ServerGroup({
  server,
  activeServer,
  activeKey,
  newSessionTarget,
  onOpenSession,
  onNewSession,
  onDeleteSession,
}: {
  server: ServerState;
  activeServer?: string;
  activeKey?: SessionKey;
  newSessionTarget: NewSessionTarget | null;
  onOpenSession: (serverUrl: string, workspaceId: string, sessionId: string) => void;
  onNewSession: (serverUrl: string, workspaceId: string) => void;
  onDeleteSession: (serverUrl: string, workspaceId: string, sessionId: string) => void;
}) {
  const [collapsed, setCollapsed] = useState(false);
  const [editing, setEditing] = useState(false);
  const [aliasDraft, setAliasDraft] = useState(server.alias ?? "");

  function startEditing() {
    setAliasDraft(server.alias ?? "");
    setEditing(true);
  }

  function commitAlias() {
    renameServer(server.url, aliasDraft);
    setEditing(false);
  }

  const offline = server.status !== "connected" && server.status !== "connecting";

  return (
    <div className="space-y-0.5">
      {editing ? (
        <div className="flex items-center gap-1 px-1 py-1">
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
            className="h-7 flex-1 px-2"
          />
          <Button
            variant="ghost"
            size="icon"
            className="size-6 text-emerald-600"
            onClick={commitAlias}
            title="Save name"
          >
            <Check className="size-4" />
          </Button>
          <Button
            variant="ghost"
            size="icon"
            className="size-6"
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
            className="flex min-w-0 flex-1 items-center gap-1.5 rounded-md px-1 py-1 text-left text-sm font-medium hover:bg-accent"
            onClick={() => setCollapsed((value) => !value)}
            title={server.url}
          >
            {collapsed ? (
              <ChevronRight className="size-4 shrink-0 text-muted-foreground" />
            ) : (
              <ChevronDown className="size-4 shrink-0 text-muted-foreground" />
            )}
            <ServerStatusDot status={server.status} />
            <span className="min-w-0 flex-1 truncate">{serverLabel(server)}</span>
          </button>
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button
                variant="ghost"
                size="icon"
                className="size-6 shrink-0 opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100 data-[state=open]:opacity-100"
                title="Server actions"
              >
                <MoreHorizontal className="size-4" />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              <DropdownMenuItem onClick={startEditing}>
                <Pencil />
                Rename
              </DropdownMenuItem>
              {offline ? (
                <DropdownMenuItem onClick={() => connectServer(server.url)}>
                  <RotateCcw />
                  Reconnect
                </DropdownMenuItem>
              ) : (
                <DropdownMenuItem onClick={() => disconnectServer(server.url)}>
                  <Unplug />
                  Disconnect
                </DropdownMenuItem>
              )}
              <DropdownMenuSeparator />
              <DropdownMenuItem variant="destructive" onClick={() => removeServer(server.url)}>
                <Trash />
                Remove
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      )}
      {!collapsed ? (
        <div className="space-y-0.5 pl-2.5">
          {server.catalog.length > 0 ? (
            server.catalog.map((workspace) => (
              <WorkspaceNode
                key={workspace.id}
                serverUrl={server.url}
                workspace={workspace}
                displayName={workspace.id}
                status={server.status}
                activeServer={activeServer}
                activeKey={activeKey}
                isTargetWorkspace={
                  newSessionTarget?.serverUrl === server.url &&
                  newSessionTarget.workspaceId === workspace.id
                }
                sessions={server.sessions}
                onOpenSession={onOpenSession}
                onNewSession={onNewSession}
                onDeleteSession={onDeleteSession}
              />
            ))
          ) : (
            <div className="px-2 py-1 text-xs text-muted-foreground">
              {server.status === "connected" ? "No workspaces" : statusCopy[server.status].label}
            </div>
          )}
        </div>
      ) : null}
    </div>
  );
});

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
            <StatusDot tone="busy" motion="ping" />
          </span>
        ) : running ? (
          <span className="flex size-4 shrink-0 items-center justify-center" title="Running">
            <StatusDot tone="busy" motion="breathe" />
          </span>
        ) : null}
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
          <Trash className="size-4" />
        </Button>
      )}
    </div>
  );
}

function WorkspaceNode({
  serverUrl,
  workspace,
  displayName,
  status,
  activeServer,
  activeKey,
  isTargetWorkspace,
  sessions: openedSessions,
  onOpenSession,
  onNewSession,
  onDeleteSession,
}: {
  serverUrl: string;
  workspace: WorkspaceSummary;
  displayName: string;
  status: ConnectionStatus;
  activeServer?: string;
  activeKey?: SessionKey;
  isTargetWorkspace: boolean;
  sessions: Record<SessionKey, OpenedSession>;
  onOpenSession: (serverUrl: string, workspaceId: string, sessionId: string) => void;
  onNewSession: (serverUrl: string, workspaceId: string) => void;
  onDeleteSession: (serverUrl: string, workspaceId: string, sessionId: string) => void;
}) {
  const [collapsed, setCollapsed] = useState(false);
  const sessions = [...workspace.sessions].sort(
    (a, b) =>
      (b.updated_at_ms ?? Number.POSITIVE_INFINITY) - (a.updated_at_ms ?? Number.POSITIVE_INFINITY),
  );

  return (
    <div className="space-y-0.5">
      <div className="flex items-center gap-1 pr-1 text-sm">
        <button
          type="button"
          className={cn(
            "flex min-w-0 flex-1 items-center gap-1.5 rounded-md px-1 py-1 text-left hover:bg-accent",
            isTargetWorkspace && "bg-accent text-accent-foreground",
          )}
          onClick={() => setCollapsed((value) => !value)}
        >
          {collapsed ? (
            <ChevronRight className="size-4 shrink-0 text-muted-foreground" />
          ) : (
            <ChevronDown className="size-4 shrink-0 text-muted-foreground" />
          )}
          <span className="min-w-0 flex-1 truncate font-medium" title={displayName}>
            {displayName}
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
                awaitingApproval={
                  opened ? opened.approvals.length > 0 : session.has_pending_approval
                }
                disabled={status !== "connected"}
                onOpen={onOpenSession}
                onDelete={onDeleteSession}
              />
            );
          })}
          {sessions.length === 0 ? (
            <div className="px-2 py-1 text-xs text-muted-foreground">No sessions yet</div>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

export function Sidebar({
  mobileOpen,
  onMobileOpenChange,
  newSessionTarget,
  onOpenSession,
  onStartNewSession,
  onNewSession,
}: {
  mobileOpen: boolean;
  onMobileOpenChange: (open: boolean) => void;
  newSessionTarget: NewSessionTarget | null;
  onOpenSession: (serverUrl: string, workspaceId: string, sessionId: string) => void;
  onStartNewSession: (serverUrl: string, workspaceId: string) => void;
  onNewSession: (serverUrl: string, workspaceId: string) => void;
}) {
  const activeServer = useCodaStore(selectActiveServer);
  const activeKey = useCodaStore(selectActiveKey);
  const servers = useCodaShallow(selectServers);
  const activeWorkspaceId = activeKey?.split("/")[0];
  const targetServer = newSessionTarget
    ? servers.find((server) => server.url === newSessionTarget.serverUrl)
    : undefined;
  const targetWorkspace =
    targetServer?.status === "connected"
      ? targetServer.catalog.find((workspace) => workspace.id === newSessionTarget?.workspaceId)
      : undefined;
  const activeServerState = activeServer
    ? servers.find((server) => server.url === activeServer)
    : undefined;
  const activeWorkspace =
    activeServerState?.status === "connected"
      ? activeServerState.catalog.find((workspace) => workspace.id === activeWorkspaceId)
      : undefined;
  const firstConnectedServer = servers.find(
    (server) => server.status === "connected" && server.catalog.length > 0,
  );
  const startTarget =
    targetServer && targetWorkspace
      ? { serverUrl: targetServer.url, workspaceId: targetWorkspace.id }
      : activeServerState && activeWorkspace
        ? { serverUrl: activeServerState.url, workspaceId: activeWorkspace.id }
        : firstConnectedServer
          ? { serverUrl: firstConnectedServer.url, workspaceId: firstConnectedServer.catalog[0].id }
          : undefined;
  const [adding, setAdding] = useState(false);
  const [collapsed, setCollapsed] = useState(false);
  const [isMobileViewport, setIsMobileViewport] = useState(false);
  const mobileDrawerHidden = isMobileViewport && !mobileOpen;

  useEffect(() => {
    if (mobileOpen) {
      setCollapsed(false);
    }
  }, [mobileOpen]);

  useEffect(() => {
    const media = window.matchMedia("(max-width: 1023.98px)");

    function syncViewport() {
      setIsMobileViewport(media.matches);
    }

    syncViewport();
    media.addEventListener("change", syncViewport);

    return () => media.removeEventListener("change", syncViewport);
  }, []);

  function startSelectedWorkspaceSession() {
    if (startTarget) {
      onMobileOpenChange(false);
      onStartNewSession(startTarget.serverUrl, startTarget.workspaceId);
    }
  }

  function openSession(serverUrl: string, workspaceId: string, sessionId: string) {
    onMobileOpenChange(false);
    onOpenSession(serverUrl, workspaceId, sessionId);
  }

  function newSession(serverUrl: string, workspaceId: string) {
    onMobileOpenChange(false);
    onNewSession(serverUrl, workspaceId);
  }

  return (
    <>
      {mobileOpen ? (
        <button
          type="button"
          className="fixed inset-0 z-40 bg-background/70 backdrop-blur-sm lg:hidden"
          onClick={() => onMobileOpenChange(false)}
          aria-label="Close sidebar"
        />
      ) : null}
      <aside
        aria-hidden={mobileDrawerHidden}
        inert={mobileDrawerHidden ? true : undefined}
        className={cn(
          // The width animates between collapsed/expanded; `overflow-hidden` lets
          // the fixed-width content below act as a curtain-revealed layer so its
          // children never reflow (slide) mid-animation. See `lg:w-[calc(...)]`.
          "absolute inset-y-0 left-0 z-50 flex h-dvh min-h-0 w-[min(22rem,calc(100vw-2rem))] shrink-0 flex-col gap-2 overflow-hidden border-r p-2.5 transition-[transform,width] duration-200 lg:static lg:z-auto lg:w-[256px] lg:translate-x-0 bg-background/70",
          mobileOpen ? "translate-x-0" : "-translate-x-full",
          collapsed && "lg:w-12",
        )}
      >
        <div className="flex items-center justify-start">
          {collapsed ? (
            <Button
              variant="ghost"
              size="icon"
              className="hidden size-6 lg:inline-flex"
              onClick={() => setCollapsed(false)}
              title="Expand servers"
            >
              <PanelLeft className="size-4" />
            </Button>
          ) : (
            <div className="flex min-w-0 flex-1 items-center gap-1 lg:w-[calc(256px-var(--spacing)*5)] lg:flex-none">
              <Button
                variant="ghost"
                size="icon"
                className="size-6 lg:hidden"
                onClick={() => onMobileOpenChange(false)}
                title="Close sidebar"
              >
                <X className="size-4" />
              </Button>
              <Button
                variant="ghost"
                size="icon"
                className="hidden size-6 lg:inline-flex"
                onClick={() => setCollapsed(true)}
                title="Collapse servers"
              >
                <PanelLeft className="size-4" />
              </Button>
              <Button
                variant="ghost"
                size="icon"
                className="size-6"
                disabled={!startTarget}
                onClick={startSelectedWorkspaceSession}
                title="New session"
              >
                <Plus className="size-4" />
              </Button>
              <Button
                variant="ghost"
                size="icon"
                className="size-6"
                onClick={() => setAdding(true)}
                title="Add server"
              >
                <Plug className="size-4" />
              </Button>
            </div>
          )}
        </div>
        {collapsed ? (
          <div className="scrollbar-fine flex min-h-0 flex-1 flex-col items-start gap-1 overflow-y-auto">
            <Button
              variant="ghost"
              size="icon"
              className="size-6"
              disabled={!startTarget}
              onClick={startSelectedWorkspaceSession}
              title="New session"
            >
              <Plus className="size-4" />
            </Button>
            <Button
              variant="ghost"
              size="icon"
              className="size-6"
              onClick={() => setAdding(true)}
              title="Add server"
            >
              <Plug className="size-4" />
            </Button>
            {servers.length > 0 ? <Separator className="my-1 w-6" /> : null}
            {servers.map((server) => (
              <CollapsedServerButton
                key={server.url}
                url={server.url}
                onNewSession={onNewSession}
              />
            ))}
          </div>
        ) : (
          <div className="scrollbar-fine min-h-0 flex-1 space-y-0.5 overflow-y-auto rounded-md p-1.5 lg:w-[calc(256px-var(--spacing)*5)]">
            {servers.length === 0 ? (
              <div className="flex min-h-32 flex-col items-center justify-center gap-3 px-3 py-6 text-center">
                <div className="text-sm font-medium">No servers</div>
                <p className="text-xs leading-5 text-muted-foreground">
                  Connect to a local or remote Coda server.
                </p>
                <Button size="sm" onClick={() => setAdding(true)}>
                  <Plug className="size-4" />
                  Add server
                </Button>
              </div>
            ) : (
              <>
                {servers.map((server) => (
                  <ServerGroup
                    key={server.url}
                    server={server}
                    activeServer={activeServer}
                    activeKey={activeKey}
                    newSessionTarget={newSessionTarget}
                    onOpenSession={openSession}
                    onNewSession={newSession}
                    onDeleteSession={deleteSession}
                  />
                ))}
              </>
            )}
          </div>
        )}
        <AddServerDialog open={adding} onOpenChange={setAdding} onConnect={connectServer} />
      </aside>
    </>
  );
}
