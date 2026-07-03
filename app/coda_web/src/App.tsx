import { Folder, Menu } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  abort,
  clearActiveSession,
  openSession,
  selectActiveEvicted,
  selectActiveHasImages,
  selectActiveProviderId,
  selectActiveProviders,
  selectActiveReasoningEffort,
  selectActiveRunning,
  selectActiveServer,
  selectActiveSessionTitle,
  selectActiveStatus,
  selectActiveUsage,
  selectActiveWorkspace,
  selectServerSummaries,
  sendTask,
  sendTaskToNewSession,
  setModel,
  takeOverActiveSession,
  useCodaBootstrap,
  useCodaStore,
  type ReasoningEffort,
  type ServerSummary,
  type UsageRecord,
} from "@/store/session";
import { Sidebar } from "@/components/sidebar";
import { Composer } from "@/components/composer";
import { Transcript } from "@/components/transcript";
import { ApprovalPanel } from "@/components/approval-panel";
import { serverLabel } from "@/components/session-utils";
import { ThemeToggle } from "@/components/theme-toggle";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  clearNewSessionTarget,
  newSessionStore,
  rememberNewSessionTarget,
  type NewSessionTarget,
  resolveNewSessionTarget,
  setNewSessionTarget,
  useNewSessionStore,
} from "@/store/new-session";

const NO_USAGE: UsageRecord[] = [];

function workspaceOptionValue(serverUrl: string, workspaceId: string) {
  return JSON.stringify([serverUrl, workspaceId]);
}

function parseWorkspaceOptionValue(value: string): NewSessionTarget | null {
  try {
    const parsed = JSON.parse(value);
    if (Array.isArray(parsed) && typeof parsed[0] === "string" && typeof parsed[1] === "string") {
      return { serverUrl: parsed[0], workspaceId: parsed[1] };
    }
  } catch {
    // Ignore malformed select values.
  }
  return null;
}

function WorkspaceTargetSelect({
  servers,
  target,
  onSelectTarget,
}: {
  servers: ServerSummary[];
  target: NewSessionTarget | null;
  onSelectTarget: (target: NewSessionTarget) => void;
}) {
  const workspaceCount = servers.reduce((total, server) => total + server.catalog.length, 0);
  // Server context lives in the group header (and a trigger hint), so workspace
  // rows show only the bare workspace id — no `@server` suffix.
  const multiServer = servers.length > 1;
  const value = target ? workspaceOptionValue(target.serverUrl, target.workspaceId) : undefined;
  const selectedServer = target
    ? servers.find((server) => server.url === target.serverUrl)
    : undefined;
  const selectedWorkspace = selectedServer?.catalog.find(
    (workspace) => workspace.id === target?.workspaceId,
  );

  return (
    <Select
      value={value}
      onValueChange={(nextValue) => {
        const nextTarget = parseWorkspaceOptionValue(nextValue);
        if (nextTarget) {
          onSelectTarget(nextTarget);
        }
      }}
    >
      <SelectTrigger
        size="sm"
        className="h-7 w-auto max-w-[220px] border border-input bg-background px-2 shadow-none hover:bg-accent"
        disabled={workspaceCount === 0}
        title="Workspace"
      >
        {selectedWorkspace && selectedServer ? (
          <span className="flex min-w-0 items-center gap-1.5">
            <Folder className="size-4 shrink-0 text-muted-foreground" />
            <span className="truncate">{selectedWorkspace.id}</span>
            {multiServer ? (
              <span className="truncate text-muted-foreground/70">
                · {serverLabel(selectedServer)}
              </span>
            ) : null}
          </span>
        ) : (
          <SelectValue placeholder="Workspace" />
        )}
      </SelectTrigger>
      <SelectContent position="popper" align="start" className="w-56">
        {servers.map((server) => (
          <SelectGroup key={server.url}>
            {multiServer ? <SelectLabel>{serverLabel(server)}</SelectLabel> : null}
            {server.catalog.map((workspace) => (
              <SelectItem
                key={workspaceOptionValue(server.url, workspace.id)}
                value={workspaceOptionValue(server.url, workspace.id)}
                disabled={server.status !== "connected"}
                className="pr-8"
              >
                <span className="min-w-0 flex-1 truncate">{workspace.id}</span>
              </SelectItem>
            ))}
          </SelectGroup>
        ))}
      </SelectContent>
    </Select>
  );
}

function WorkspaceHeader({
  sessionTitle,
  onOpenSidebar,
}: {
  sessionTitle?: string;
  onOpenSidebar: () => void;
}) {
  return (
    <header className="flex h-[calc(2.75rem_+_env(safe-area-inset-top))] shrink-0 items-center gap-2 border-b bg-background px-2 pt-[env(safe-area-inset-top)] sm:px-4">
      <button
        type="button"
        className="flex size-8 shrink-0 items-center justify-center rounded-md text-muted-foreground hover:bg-accent hover:text-foreground lg:hidden"
        onClick={onOpenSidebar}
        title="Open sidebar"
        aria-label="Open sidebar"
      >
        <Menu className="size-4" />
      </button>
      <div className="min-w-0 flex-1 overflow-hidden text-sm">
        {sessionTitle ? (
          <span className="block truncate font-medium" title={sessionTitle}>
            {sessionTitle}
          </span>
        ) : null}
      </div>
      <ThemeToggle />
    </header>
  );
}

function WorkspaceTargetBar({
  servers,
  target,
  onSelectTarget,
}: {
  servers: ServerSummary[];
  target: NewSessionTarget | null;
  onSelectTarget: (target: NewSessionTarget) => void;
}) {
  return (
    <div className="bg-background px-3 pt-2">
      <div className="mx-auto flex max-w-4xl items-center">
        <WorkspaceTargetSelect servers={servers} target={target} onSelectTarget={onSelectTarget} />
      </div>
    </div>
  );
}

export default function App() {
  useCodaBootstrap();

  // Server summaries exclude session state, so streaming entries leave this
  // subscription stable.
  const servers = useCodaStore(selectServerSummaries);
  const activeServer = useCodaStore(selectActiveServer);
  const activeWorkspace = useCodaStore(selectActiveWorkspace);
  const activeStatus = useCodaStore(selectActiveStatus);
  const activeRunning = useCodaStore(selectActiveRunning);
  const activeEvicted = useCodaStore(selectActiveEvicted);
  const activeProviders = useCodaStore(selectActiveProviders);
  const activeProviderId = useCodaStore(selectActiveProviderId);
  const activeReasoningEffort = useCodaStore(selectActiveReasoningEffort);
  const activeSessionTitle = useCodaStore(selectActiveSessionTitle);
  const activeUsage = useCodaStore(selectActiveUsage);
  const activeHasImages = useCodaStore(selectActiveHasImages);

  const newSessionTarget = useNewSessionStore((state) => state.target);
  const [newSessionModel, setNewSessionModel] = useState<{
    serverUrl: string;
    providerId: string;
    reasoningEffort: ReasoningEffort | null;
  } | null>(null);
  const [sidebarOpen, setSidebarOpen] = useState(false);

  const selectedServerUrl = newSessionTarget?.serverUrl ?? activeServer ?? "";
  const selectedServerState = servers.find((server) => server.url === selectedServerUrl);
  const selectedWorkspace = newSessionTarget?.workspaceId ?? activeWorkspace;
  const showingNewSession = newSessionTarget !== null;
  const showComposer = showingNewSession || Boolean(activeWorkspace);

  useEffect(() => {
    if (!newSessionTarget) {
      setNewSessionModel(null);
      return;
    }
    const resolved = resolveNewSessionTarget(servers, newSessionTarget, activeServer);
    if (!resolved.serverUrl || !resolved.workspaceId) {
      clearNewSessionTarget();
      return;
    }
    if (
      resolved.serverUrl !== newSessionTarget.serverUrl ||
      resolved.workspaceId !== newSessionTarget.workspaceId
    ) {
      setNewSessionTarget(resolved);
    }
  }, [newSessionTarget, servers, activeServer]);

  useEffect(() => {
    if (!newSessionTarget) {
      return;
    }
    const server = servers.find((item) => item.url === newSessionTarget.serverUrl);
    const currentProvider = server?.providers.find(
      (provider) =>
        provider.id === newSessionModel?.providerId &&
        newSessionModel.serverUrl === newSessionTarget.serverUrl,
    );
    if (currentProvider) {
      return;
    }
    const provider =
      server?.providers.find((item) => item.id === server.defaultProvider) ?? server?.providers[0];
    setNewSessionModel(
      provider
        ? {
            serverUrl: newSessionTarget.serverUrl,
            providerId: provider.id,
            reasoningEffort: provider.reasoning_efforts[0] ?? null,
          }
        : null,
    );
  }, [newSessionModel, newSessionTarget, servers]);

  // On first load, restore the workspace last selected (persisted as
  // `recentTarget`). Prefer the remembered server: wait for it to connect rather
  // than falling back to whichever server happens to come up first, and give up
  // only if it's no longer configured or the user already picked something.
  const restoredTargetRef = useRef(false);
  useEffect(() => {
    if (restoredTargetRef.current) {
      return;
    }
    if (newSessionTarget || activeServer) {
      restoredTargetRef.current = true;
      return;
    }
    const recent = newSessionStore.getState().recentTarget;
    if (!recent) {
      restoredTargetRef.current = true;
      return;
    }
    const server = servers.find((item) => item.url === recent.serverUrl);
    if (!server || server.status !== "connected" || server.catalog.length === 0) {
      // Not yet in the (still-populating) server list, or still connecting —
      // keep waiting for the remembered server rather than giving up or falling
      // back to whichever server happens to come up first.
      return;
    }
    const workspace =
      server.catalog.find((item) => item.id === recent.workspaceId) ?? server.catalog[0];
    restoredTargetRef.current = true;
    setNewSessionTarget({ serverUrl: server.url, workspaceId: workspace.id });
  }, [servers, newSessionTarget, activeServer]);

  // Handlers read the latest store state at call time rather than closing over
  // the subscribed values, so they keep a stable identity across renders and
  // don't defeat the memoized children.
  const startNewSession = useCallback((serverUrl: string, workspaceId: string) => {
    setSidebarOpen(false);
    clearActiveSession();
    setNewSessionTarget({ serverUrl, workspaceId });
  }, []);

  const handleOpenSession = useCallback(
    (serverUrl: string, workspaceId: string, sessionId: string) => {
      setSidebarOpen(false);
      rememberNewSessionTarget({ serverUrl, workspaceId });
      clearNewSessionTarget();
      openSession(serverUrl, workspaceId, sessionId);
    },
    [],
  );

  const handleSend = useCallback(
    (task: string, images: string[] = []) => {
      const target = newSessionStore.getState().target;
      if (target) {
        rememberNewSessionTarget(target);
        sendTaskToNewSession(
          target.serverUrl,
          target.workspaceId,
          task,
          newSessionModel?.providerId,
          newSessionModel?.reasoningEffort ?? null,
          images,
        );
        clearNewSessionTarget();
        return;
      }
      sendTask(task, images);
    },
    [newSessionModel],
  );

  const handleSetNewSessionModel = useCallback(
    (providerId: string, reasoningEffort: ReasoningEffort | null) => {
      const serverUrl = newSessionStore.getState().target?.serverUrl ?? "";
      setNewSessionModel({ serverUrl, providerId, reasoningEffort });
    },
    [],
  );

  return (
    <div className="relative flex h-dvh min-h-0 overflow-hidden bg-background lg:min-h-[600px]">
      <Sidebar
        mobileOpen={sidebarOpen}
        onMobileOpenChange={setSidebarOpen}
        newSessionTarget={newSessionTarget}
        onOpenSession={handleOpenSession}
        onStartNewSession={startNewSession}
        onNewSession={startNewSession}
      />
      <section className="flex min-w-0 min-h-0 flex-1 flex-col bg-background">
        <WorkspaceHeader
          sessionTitle={activeSessionTitle}
          onOpenSidebar={() => setSidebarOpen(true)}
        />
        <Transcript suppressed={showingNewSession} workspace={selectedWorkspace} />
        <div className="relative z-20 shrink-0 bg-background pb-[env(safe-area-inset-bottom)]">
          {showingNewSession ? (
            <WorkspaceTargetBar
              servers={servers}
              target={newSessionTarget}
              onSelectTarget={setNewSessionTarget}
            />
          ) : (
            <ApprovalPanel />
          )}
          {showComposer ? (
            <Composer
              status={showingNewSession ? (selectedServerState?.status ?? "idle") : activeStatus}
              running={showingNewSession ? false : activeRunning}
              evicted={showingNewSession ? false : activeEvicted}
              workspace={selectedWorkspace}
              selectingTarget={showingNewSession}
              providers={
                showingNewSession ? (selectedServerState?.providers ?? []) : activeProviders
              }
              providerId={showingNewSession ? newSessionModel?.providerId : activeProviderId}
              reasoningEffort={
                showingNewSession
                  ? (newSessionModel?.reasoningEffort ?? null)
                  : activeReasoningEffort
              }
              usage={showingNewSession ? NO_USAGE : activeUsage}
              sessionHasImages={showingNewSession ? false : activeHasImages}
              onSetModel={showingNewSession ? handleSetNewSessionModel : setModel}
              onSend={handleSend}
              onAbort={abort}
              onTakeOver={takeOverActiveSession}
            />
          ) : null}
        </div>
      </section>
    </div>
  );
}
