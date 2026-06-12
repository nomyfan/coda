import { Command } from "lucide-react";
import { useEffect, useState } from "react";
import type { PendingApproval } from "@/lib/protocol";
import {
  type ReasoningEffort,
  useCodaSession,
} from "@/lib/session";
import { Sidebar } from "@/components/sidebar";
import { Composer } from "@/components/composer";
import { Transcript } from "@/components/transcript";
import { ApprovalPanel } from "@/components/approval-panel";
import {
  beginNewSession,
  clearNewSessionTarget,
  rememberNewSessionTarget,
  resolveNewSessionTarget,
  setNewSessionTarget,
  useNewSessionStore,
} from "@/store/new-session";

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

export default function App() {
  const session = useCodaSession();
  const newSessionTarget = useNewSessionStore((state) => state.target);
  const [newSessionModel, setNewSessionModel] = useState<{
    serverUrl: string;
    providerId: string;
    reasoningEffort: ReasoningEffort | null;
  } | null>(null);

  const selectedServerUrl =
    newSessionTarget?.serverUrl ?? session.activeServer ?? "";
  const selectedServerState = session.servers.find(
    (server) => server.url === selectedServerUrl
  );
  const selectedWorkspace =
    newSessionTarget?.workspaceId ?? session.activeWorkspace;
  const workspaceIds = selectedServerState?.catalog.map((ws) => ws.id) ?? [];
  const showingNewSession = newSessionTarget !== null;

  useEffect(() => {
    if (!newSessionTarget) {
      if (newSessionModel) {
        setNewSessionModel(null);
      }
      return;
    }
    const resolved = resolveNewSessionTarget(
      session.servers,
      newSessionTarget,
      session.activeServer
    );
    if (
      resolved.serverUrl !== newSessionTarget.serverUrl ||
      resolved.workspaceId !== newSessionTarget.workspaceId
    ) {
      setNewSessionTarget(resolved);
    }
  }, [newSessionTarget, session.servers]);

  useEffect(() => {
    if (!newSessionTarget) {
      return;
    }
    const server = session.servers.find(
      (item) => item.url === newSessionTarget.serverUrl
    );
    const currentProvider = server?.providers.find(
      (provider) =>
        provider.id === newSessionModel?.providerId &&
        newSessionModel.serverUrl === newSessionTarget.serverUrl
    );
    if (currentProvider) {
      return;
    }
    const provider =
      server?.providers.find(
        (item) => item.id === server.defaultProvider
      ) ?? server?.providers[0];
    setNewSessionModel(
      provider
        ? {
            serverUrl: newSessionTarget.serverUrl,
            providerId: provider.id,
            reasoningEffort: provider.reasoning_efforts[0] ?? null,
          }
        : null
    );
  }, [newSessionModel, newSessionTarget, session.servers]);

  function startNewSession() {
    beginNewSession(session.servers, session.activeServer);
  }

  function openSession(serverUrl: string, workspaceId: string, sessionId: string) {
    rememberNewSessionTarget({ serverUrl, workspaceId });
    clearNewSessionTarget();
    session.openSession(serverUrl, workspaceId, sessionId);
  }

  function createWorkspaceSession(serverUrl: string, workspaceId: string) {
    rememberNewSessionTarget({ serverUrl, workspaceId });
    clearNewSessionTarget();
    session.newSession(serverUrl, workspaceId);
  }

  function changeNewSessionServer(serverUrl: string) {
    const server = session.servers.find((item) => item.url === serverUrl);
    const target = {
      serverUrl,
      workspaceId: server?.catalog[0]?.id ?? "",
    };
    setNewSessionTarget(target);
  }

  function changeWorkspace(workspaceId: string) {
    if (newSessionTarget) {
      const target = { ...newSessionTarget, workspaceId };
      setNewSessionTarget(target);
      return;
    }
    if (session.activeServer) {
      rememberNewSessionTarget({
        serverUrl: session.activeServer,
        workspaceId,
      });
      session.newSession(session.activeServer, workspaceId);
    }
  }

  function sendTask(task: string) {
    if (newSessionTarget) {
      rememberNewSessionTarget(newSessionTarget);
      session.sendTaskToNewSession(
        newSessionTarget.serverUrl,
        newSessionTarget.workspaceId,
        task,
        newSessionModel?.providerId,
        newSessionModel?.reasoningEffort ?? null
      );
      clearNewSessionTarget();
      return;
    }
    session.sendTask(task);
  }

  return (
    <div className="flex h-screen min-h-[600px] flex-col overflow-hidden bg-background">
      <WorkspaceHeader approvals={showingNewSession ? [] : session.approvals} />
      <main className="grid min-h-0 flex-1 grid-cols-1 lg:grid-cols-[256px_minmax(0,1fr)]">
        <Sidebar
          servers={session.servers}
          activeServer={session.activeServer}
          activeKey={session.activeKey}
          onConnectServer={session.connectServer}
          onDisconnectServer={session.disconnectServer}
          onRemoveServer={session.removeServer}
          onRenameServer={session.renameServer}
          onOpenSession={openSession}
          onStartNewSession={startNewSession}
          onNewSession={createWorkspaceSession}
          onDeleteSession={session.deleteSession}
        />
        <section className="flex min-h-0 flex-col">
          <Transcript
            entries={showingNewSession ? [] : session.entries}
            running={showingNewSession ? false : session.running}
            workspace={selectedWorkspace}
          />
          <div className="relative z-20 shrink-0">
            {showingNewSession ? null : (
              <ApprovalPanel
                approvals={session.approvals}
                drafts={session.drafts}
                onDraft={session.draftCall}
                onSubmit={session.submitApprovals}
                onAllowPattern={session.addAllowPattern}
              />
            )}
            <Composer
              status={
                showingNewSession
                  ? selectedServerState?.status ?? "idle"
                  : session.status
              }
              running={showingNewSession ? false : session.running}
              server={selectedServerUrl}
              servers={session.servers}
              workspace={selectedWorkspace}
              workspaces={workspaceIds}
              selectingTarget={showingNewSession}
              providers={
                showingNewSession
                  ? selectedServerState?.providers ?? []
                  : session.providers
              }
              providerId={
                showingNewSession
                  ? newSessionModel?.providerId
                  : session.activeProviderId
              }
              reasoningEffort={
                showingNewSession
                  ? newSessionModel?.reasoningEffort ?? null
                  : session.activeReasoningEffort
              }
              onSetModel={
                showingNewSession
                  ? (providerId, reasoningEffort) =>
                      setNewSessionModel({
                        serverUrl: selectedServerUrl,
                        providerId,
                        reasoningEffort,
                      })
                  : session.setModel
              }
              onChangeServer={changeNewSessionServer}
              onChangeWorkspace={changeWorkspace}
              onSend={sendTask}
              onAbort={session.abort}
            />
          </div>
        </section>
      </main>
    </div>
  );
}
