import { Folder, Menu } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  abort,
  clearActiveSession,
  compactActiveSession,
  dismissPersistError,
  openSession,
  selectActiveApprovalCount,
  selectActiveCompacting,
  selectActiveEvicted,
  selectActivePersistError,
  selectActiveHasImages,
  selectActiveProviderId,
  selectActiveProviders,
  selectActivePermissionMode,
  selectActiveReasoningEffort,
  selectActiveEditing,
  selectActiveForkDraft,
  selectActiveKey,
  selectActiveRunning,
  selectActiveServer,
  selectActiveSessionTitle,
  selectActiveStarting,
  selectActiveStatus,
  selectActiveUsage,
  selectActiveWorkspace,
  selectServerSummaries,
  cancelEdit,
  rewindTurn,
  sendTask,
  sendTaskToNewSession,
  setModel,
  setPermissionMode,
  takeOverActiveSession,
  updateForkDraft,
  useCodaBootstrap,
  useCodaStore,
  type PermissionMode,
  type ReasoningEffort,
  type ServerSummary,
  type UsageRecord,
} from "@/store/session";
import { DEFAULT_PERMISSION_MODE } from "@/lib/protocol";
import { parseCompactCommand } from "@/lib/compact-command";
import { initialModelSelection, rememberModelSelection } from "@/store/model-preferences";
import { Sidebar } from "@/components/sidebar";
import { Button } from "@/components/ui/button";
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
  const activeCompacting = useCodaStore(selectActiveCompacting);
  const activeApprovalCount = useCodaStore(selectActiveApprovalCount);
  const activeEditing = useCodaStore(selectActiveEditing);
  const activeForkDraft = useCodaStore(selectActiveForkDraft);
  const activeKey = useCodaStore(selectActiveKey);
  const activeStarting = useCodaStore(selectActiveStarting);
  const activeEvicted = useCodaStore(selectActiveEvicted);
  const activePersistError = useCodaStore(selectActivePersistError);
  const activeProviders = useCodaStore(selectActiveProviders);
  const activeProviderId = useCodaStore(selectActiveProviderId);
  const activeReasoningEffort = useCodaStore(selectActiveReasoningEffort);
  const activePermissionMode = useCodaStore(selectActivePermissionMode);
  const activeSessionTitle = useCodaStore(selectActiveSessionTitle);
  const activeUsage = useCodaStore(selectActiveUsage);
  const activeHasImages = useCodaStore(selectActiveHasImages);

  const handleForkDraftChange = useCallback(
    (text: string, images: string[]) => {
      if (activeServer && activeKey) {
        updateForkDraft(activeServer, activeKey, text, images);
      }
    },
    [activeKey, activeServer],
  );

  const newSessionTarget = useNewSessionStore((state) => state.target);
  const [newSessionModel, setNewSessionModel] = useState<{
    serverUrl: string;
    workspaceId: string;
    providerId: string;
    reasoningEffort: ReasoningEffort | null;
  } | null>(null);
  const [newSessionMode, setNewSessionMode] = useState<PermissionMode>(DEFAULT_PERMISSION_MODE);
  const [sidebarOpen, setSidebarOpen] = useState(false);

  const selectedServerUrl = newSessionTarget?.serverUrl ?? activeServer ?? "";
  const selectedServerState = servers.find((server) => server.url === selectedServerUrl);
  const selectedWorkspace = newSessionTarget?.workspaceId ?? activeWorkspace;
  const showingNewSession = newSessionTarget !== null;
  const showComposer = showingNewSession || Boolean(activeWorkspace);
  // Evicted sessions get a full takeover mask over the conversation column:
  // the local state is a frozen pre-eviction snapshot (the server ended this
  // client's event stream), so nothing under it should look interactive.
  const evictedTakeover = activeEvicted && !showingNewSession;

  useEffect(() => {
    if (!newSessionTarget) {
      setNewSessionModel(null);
      // Nothing is remembered per workspace, so every new conversation opens on
      // the default rather than inheriting the last one's posture.
      setNewSessionMode(DEFAULT_PERMISSION_MODE);
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

  const selectedNewSessionModel = useMemo(() => {
    if (!newSessionTarget || !selectedServerState) {
      return null;
    }
    const currentProvider = selectedServerState.providers.find(
      (provider) =>
        provider.id === newSessionModel?.providerId &&
        newSessionModel.serverUrl === newSessionTarget.serverUrl &&
        newSessionModel.workspaceId === newSessionTarget.workspaceId,
    );
    if (currentProvider) {
      return newSessionModel;
    }
    const selection = initialModelSelection(selectedServerState, newSessionTarget.workspaceId);
    return selection.providerId
      ? {
          serverUrl: newSessionTarget.serverUrl,
          workspaceId: newSessionTarget.workspaceId,
          providerId: selection.providerId,
          reasoningEffort: selection.reasoningEffort,
        }
      : null;
  }, [newSessionModel, newSessionTarget, selectedServerState]);

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
          selectedNewSessionModel?.providerId,
          selectedNewSessionModel?.reasoningEffort ?? null,
          images,
          newSessionMode,
        );
        clearNewSessionTarget();
        return;
      }
      // While a message is being rewritten the composer is that message's
      // editor, so its submit rewinds instead of appending.
      if (activeEditing) {
        rewindTurn(task, images);
        return;
      }
      const compactInstructions = images.length === 0 ? parseCompactCommand(task) : null;
      if (compactInstructions !== null) {
        void compactActiveSession(compactInstructions);
        return;
      }
      sendTask(task, images);
    },
    [selectedNewSessionModel, newSessionMode, activeEditing],
  );

  const handleSetNewSessionModel = useCallback(
    (providerId: string, reasoningEffort: ReasoningEffort | null) => {
      const target = newSessionStore.getState().target;
      if (!target) {
        return;
      }
      setNewSessionModel({ ...target, providerId, reasoningEffort });
      rememberModelSelection(target.serverUrl, target.workspaceId, providerId, reasoningEffort);
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
        <div className="relative flex min-w-0 min-h-0 flex-1 flex-col">
          {/* `inert` also blocks keyboard focus, which the overlay alone
              wouldn't; the sidebar stays interactive for switching away. */}
          <div
            inert={evictedTakeover || undefined}
            className="flex min-w-0 min-h-0 flex-1 flex-col"
          >
            <Transcript suppressed={showingNewSession} workspace={selectedWorkspace} />
            <div className="relative z-20 shrink-0 bg-background pb-[env(safe-area-inset-bottom)]">
              {!showingNewSession && activePersistError ? (
                <div className="mx-3 mb-2 flex items-start gap-3 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2">
                  <p className="min-w-0 flex-1 text-xs text-destructive">
                    The last turn was not fully saved, so parts of it may be missing from this
                    session. {""}
                    <span className="text-destructive/80">{activePersistError}</span>
                  </p>
                  <Button
                    size="sm"
                    variant="ghost"
                    className="h-6 shrink-0 px-2 text-xs"
                    onClick={dismissPersistError}
                  >
                    Dismiss
                  </Button>
                </div>
              ) : null}
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
                  // Remounting on every change of edit target loads that
                  // message without a sync effect. A fork draft is keyed by its
                  // session so returning to it restores that session's text.
                  key={
                    activeEditing
                      ? `edit:${activeEditing.target ?? "orphan"}`
                      : activeForkDraft
                        ? `fork-draft:${activeKey}`
                        : "new"
                  }
                  status={
                    showingNewSession ? (selectedServerState?.status ?? "idle") : activeStatus
                  }
                  running={showingNewSession ? false : activeRunning}
                  compacting={showingNewSession ? false : activeCompacting}
                  approvalPending={showingNewSession ? false : activeApprovalCount > 0}
                  starting={showingNewSession ? false : activeStarting}
                  evicted={showingNewSession ? false : activeEvicted}
                  workspace={selectedWorkspace}
                  selectingTarget={showingNewSession}
                  permissionMode={showingNewSession ? newSessionMode : activePermissionMode}
                  providers={
                    showingNewSession ? (selectedServerState?.providers ?? []) : activeProviders
                  }
                  providerId={
                    showingNewSession ? selectedNewSessionModel?.providerId : activeProviderId
                  }
                  reasoningEffort={
                    showingNewSession
                      ? (selectedNewSessionModel?.reasoningEffort ?? null)
                      : activeReasoningEffort
                  }
                  usage={showingNewSession ? NO_USAGE : activeUsage}
                  sessionHasImages={showingNewSession ? false : activeHasImages}
                  serverUrl={selectedServerUrl}
                  workspaceId={selectedWorkspace ?? ""}
                  editing={showingNewSession ? undefined : activeEditing}
                  forkDraft={showingNewSession ? undefined : activeForkDraft}
                  onForkDraftChange={handleForkDraftChange}
                  onSetModel={showingNewSession ? handleSetNewSessionModel : setModel}
                  onSetPermissionMode={showingNewSession ? setNewSessionMode : setPermissionMode}
                  onSend={handleSend}
                  onAbort={abort}
                  onCancelEdit={cancelEdit}
                />
              ) : null}
            </div>
          </div>
          {evictedTakeover && (
            <div className="absolute inset-0 z-30 flex items-center justify-center bg-background/60 p-4 backdrop-blur-[2px]">
              <div className="flex max-w-sm flex-col items-center gap-3 rounded-lg border border-border bg-background p-6 text-center shadow-lg">
                <p className="text-sm text-muted-foreground">
                  This session is being driven by another window and this view is not receiving
                  updates. Taking over will disconnect it there.
                </p>
                <Button size="sm" onClick={takeOverActiveSession}>
                  Take over
                </Button>
              </div>
            </div>
          )}
        </div>
      </section>
    </div>
  );
}
