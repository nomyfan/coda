import { Command } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  abort,
  codaStore,
  newSession,
  openSession,
  selectActiveApprovalCount,
  selectActiveHasImages,
  selectActiveProviderId,
  selectActiveProviders,
  selectActiveReasoningEffort,
  selectActiveRunning,
  selectActiveServer,
  selectActiveStatus,
  selectActiveUsage,
  selectActiveWorkspace,
  selectServerSummaries,
  sendTask,
  sendTaskToNewSession,
  setModel,
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
import {
  beginNewSession,
  clearNewSessionTarget,
  newSessionStore,
  rememberNewSessionTarget,
  resolveNewSessionTarget,
  setNewSessionTarget,
  useNewSessionStore,
} from "@/store/new-session";

/** Stable empty list so the composer's `servers` prop is referentially stable
 * in the active-session view (the list is only used while picking a target). */
const NO_SERVERS: ServerSummary[] = [];
const NO_USAGE: UsageRecord[] = [];

function WorkspaceHeader({ approvalCount }: { approvalCount: number }) {
  return (
    <header className="flex h-11 shrink-0 items-center justify-between border-b bg-background/90 px-4 backdrop-blur">
      <div className="flex min-w-0 items-center gap-2">
        <div className="flex size-6 items-center justify-center rounded-md bg-primary text-primary-foreground shadow-sm">
          <Command className="size-3.5" />
        </div>
        <h1 className="truncate text-sm font-semibold tracking-normal">Coda</h1>
        <span className="size-1 rounded-full bg-muted-foreground/45" />
        <span className="text-xs text-muted-foreground">{approvalCount} pending approval(s)</span>
      </div>
    </header>
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
  const activeProviders = useCodaStore(selectActiveProviders);
  const activeProviderId = useCodaStore(selectActiveProviderId);
  const activeReasoningEffort = useCodaStore(selectActiveReasoningEffort);
  const activeApprovalCount = useCodaStore(selectActiveApprovalCount);
  const activeUsage = useCodaStore(selectActiveUsage);
  const activeHasImages = useCodaStore(selectActiveHasImages);

  const newSessionTarget = useNewSessionStore((state) => state.target);
  const [newSessionModel, setNewSessionModel] = useState<{
    serverUrl: string;
    providerId: string;
    reasoningEffort: ReasoningEffort | null;
  } | null>(null);

  const selectedServerUrl = newSessionTarget?.serverUrl ?? activeServer ?? "";
  const selectedServerState = servers.find((server) => server.url === selectedServerUrl);
  const selectedWorkspace = newSessionTarget?.workspaceId ?? activeWorkspace;
  const workspaceIds = useMemo(
    () => selectedServerState?.catalog.map((ws) => ws.id) ?? [],
    [selectedServerState?.catalog],
  );
  const showingNewSession = newSessionTarget !== null;

  useEffect(() => {
    if (!newSessionTarget) {
      setNewSessionModel(null);
      return;
    }
    const resolved = resolveNewSessionTarget(servers, newSessionTarget, activeServer);
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

  // Handlers read the latest store state at call time rather than closing over
  // the subscribed values, so they keep a stable identity across renders and
  // don't defeat the memoized children.
  const startNewSession = useCallback(() => {
    const state = codaStore.getState();
    beginNewSession(selectServerSummaries(state), state.activeServer);
  }, []);

  const handleOpenSession = useCallback(
    (serverUrl: string, workspaceId: string, sessionId: string) => {
      rememberNewSessionTarget({ serverUrl, workspaceId });
      clearNewSessionTarget();
      openSession(serverUrl, workspaceId, sessionId);
    },
    [],
  );

  const createWorkspaceSession = useCallback((serverUrl: string, workspaceId: string) => {
    rememberNewSessionTarget({ serverUrl, workspaceId });
    clearNewSessionTarget();
    newSession(serverUrl, workspaceId);
  }, []);

  const changeNewSessionServer = useCallback((serverUrl: string) => {
    const server = selectServerSummaries(codaStore.getState()).find(
      (item) => item.url === serverUrl,
    );
    setNewSessionTarget({
      serverUrl,
      workspaceId: server?.catalog[0]?.id ?? "",
    });
  }, []);

  const changeWorkspace = useCallback((workspaceId: string) => {
    const target = newSessionStore.getState().target;
    if (target) {
      setNewSessionTarget({ ...target, workspaceId });
      return;
    }
    const server = codaStore.getState().activeServer;
    if (server) {
      rememberNewSessionTarget({ serverUrl: server, workspaceId });
      newSession(server, workspaceId);
    }
  }, []);

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
    <div className="flex h-screen min-h-[600px] flex-col overflow-hidden bg-background">
      <WorkspaceHeader approvalCount={showingNewSession ? 0 : activeApprovalCount} />
      <main className="grid min-h-0 flex-1 grid-cols-1 lg:grid-cols-[auto_minmax(0,1fr)]">
        <Sidebar
          onOpenSession={handleOpenSession}
          onStartNewSession={startNewSession}
          onNewSession={createWorkspaceSession}
        />
        <section className="flex min-h-0 flex-col">
          <Transcript suppressed={showingNewSession} workspace={selectedWorkspace} />
          <div className="relative z-20 shrink-0">
            {showingNewSession ? null : <ApprovalPanel />}
            <Composer
              status={showingNewSession ? (selectedServerState?.status ?? "idle") : activeStatus}
              running={showingNewSession ? false : activeRunning}
              server={selectedServerUrl}
              servers={showingNewSession ? servers : NO_SERVERS}
              workspace={selectedWorkspace}
              workspaces={workspaceIds}
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
              onChangeServer={changeNewSessionServer}
              onChangeWorkspace={changeWorkspace}
              onSend={handleSend}
              onAbort={abort}
            />
          </div>
        </section>
      </main>
    </div>
  );
}
