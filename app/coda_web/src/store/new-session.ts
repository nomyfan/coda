import { useStore } from "zustand";
import type { ServerState } from "./session";
import { create } from "@/store/utils";

export type NewSessionTarget = {
  serverUrl: string;
  workspaceId: string;
};

export type NewSessionState = {
  target: NewSessionTarget | null;
  recentTarget: NewSessionTarget | null;
};

const storageKey = "coda.newSessionTarget";

function validTarget(target: NewSessionTarget | null | undefined) {
  return Boolean(target?.serverUrl && target.workspaceId);
}

function loadRecentTarget(): NewSessionTarget | null {
  try {
    const raw = window.localStorage.getItem(storageKey);
    if (!raw) {
      return null;
    }
    const parsed = JSON.parse(raw);
    if (
      parsed &&
      typeof parsed.serverUrl === "string" &&
      typeof parsed.workspaceId === "string"
    ) {
      return {
        serverUrl: parsed.serverUrl,
        workspaceId: parsed.workspaceId,
      };
    }
  } catch {
    // ignore storage failures
  }
  return null;
}

function writeRecentTarget(target: NewSessionTarget) {
  try {
    window.localStorage.setItem(storageKey, JSON.stringify(target));
  } catch {
    // ignore storage failures
  }
}

export const newSessionStore = create<NewSessionState>(() => ({
  target: null,
  recentTarget: loadRecentTarget(),
}));

export function useNewSessionStore<T>(
  selector: (state: NewSessionState) => T
) {
  return useStore(newSessionStore, selector);
}

export function resolveNewSessionTarget(
  servers: ServerState[],
  preferred?: NewSessionTarget | null,
  fallbackServer?: string
): NewSessionTarget {
  const available = servers.filter((server) => server.catalog.length > 0);
  const preferredServer = available.find(
    (server) => server.url === preferred?.serverUrl
  );
  if (preferredServer) {
    const preferredWorkspace = preferredServer.catalog.find(
      (workspace) => workspace.id === preferred?.workspaceId
    );
    return {
      serverUrl: preferredServer.url,
      workspaceId:
        preferredWorkspace?.id ?? preferredServer.catalog[0]?.id ?? "",
    };
  }
  const fallback =
    available.find((server) => server.url === fallbackServer) ?? available[0];
  return {
    serverUrl: fallback?.url ?? "",
    workspaceId: fallback?.catalog[0]?.id ?? "",
  };
}

export function rememberNewSessionTarget(target: NewSessionTarget) {
  if (!validTarget(target)) {
    return;
  }
  newSessionStore.setState((state) => {
    state.recentTarget = target;
  });
  writeRecentTarget(target);
}

export function beginNewSession(
  servers: ServerState[],
  fallbackServer?: string
) {
  const target = resolveNewSessionTarget(
    servers,
    newSessionStore.getState().recentTarget,
    fallbackServer
  );
  newSessionStore.setState((state) => {
    state.target = target;
  });
  rememberNewSessionTarget(target);
}

export function setNewSessionTarget(target: NewSessionTarget) {
  newSessionStore.setState((state) => {
    state.target = target;
  });
  rememberNewSessionTarget(target);
}

export function clearNewSessionTarget() {
  newSessionStore.setState((state) => {
    state.target = null;
  });
}
