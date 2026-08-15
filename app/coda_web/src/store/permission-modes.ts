import { DEFAULT_PERMISSION_MODE, isPermissionMode, type PermissionMode } from "@/lib/protocol";

const storageKey = "coda.permissionModes";

/**
 * How many sessions to remember per server. Every session ever opened would
 * otherwise accumulate here forever; the oldest entries are dropped first,
 * and a forgotten session simply reopens on the default.
 */
const MAX_REMEMBERED = 200;

/** `{ ts }` is the LRU stamp, refreshed on every write. */
type StoredMode = { mode: PermissionMode; ts: number };
type ModeMemory = Record<string, Record<string, StoredMode>>;

/**
 * The mode is remembered per *session*, not per workspace: switching mid-
 * conversation is scoped to that conversation, and a new one always starts on
 * {@link DEFAULT_PERMISSION_MODE}. Nothing is persisted server-side, so this
 * is the only record of what a released session was running under.
 */
function sessionSlot(workspaceId: string, sessionId: string) {
  return `${workspaceId}:${sessionId}`;
}

function isStoredMode(value: unknown): value is StoredMode {
  if (!value || typeof value !== "object") {
    return false;
  }
  const stored = value as Partial<StoredMode>;
  return isPermissionMode(stored.mode) && typeof stored.ts === "number";
}

function loadMemory(): ModeMemory {
  const memory: ModeMemory = Object.create(null);
  try {
    const raw = window.localStorage.getItem(storageKey);
    if (!raw) {
      return memory;
    }
    const parsed = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      return memory;
    }
    for (const [server, storedSessions] of Object.entries(parsed)) {
      if (!storedSessions || typeof storedSessions !== "object") {
        continue;
      }
      const sessions: Record<string, StoredMode> = Object.create(null);
      for (const [slot, stored] of Object.entries(storedSessions)) {
        if (isStoredMode(stored)) {
          sessions[slot] = stored;
        }
      }
      if (Object.keys(sessions).length > 0) {
        memory[server] = sessions;
      }
    }
  } catch {
    // ignore malformed/blocked storage
  }
  return memory;
}

/** Drop the least recently written entries once a server exceeds the cap. */
function prune(sessions: Record<string, StoredMode>) {
  const slots = Object.keys(sessions);
  if (slots.length <= MAX_REMEMBERED) {
    return;
  }
  const oldestFirst = slots.sort((a, b) => sessions[a].ts - sessions[b].ts);
  for (const slot of oldestFirst.slice(0, slots.length - MAX_REMEMBERED)) {
    delete sessions[slot];
  }
}

export function rememberSessionMode(
  server: string,
  workspaceId: string,
  sessionId: string,
  mode: PermissionMode,
) {
  try {
    const memory = loadMemory();
    memory[server] ??= Object.create(null);
    memory[server][sessionSlot(workspaceId, sessionId)] = { mode, ts: Date.now() };
    prune(memory[server]);
    window.localStorage.setItem(storageKey, JSON.stringify(memory));
  } catch {
    // ignore storage failures (private mode, disabled storage)
  }
}

export function forgetSessionMode(server: string, workspaceId: string, sessionId: string) {
  try {
    const memory = loadMemory();
    const sessions = memory[server];
    if (!sessions?.[sessionSlot(workspaceId, sessionId)]) {
      return;
    }
    delete sessions[sessionSlot(workspaceId, sessionId)];
    window.localStorage.setItem(storageKey, JSON.stringify(memory));
  } catch {
    // ignore storage failures
  }
}

/**
 * The mode to open this session on: what it was last seen running under, or
 * `fallback` for one this browser has no record of (a new conversation, a new
 * device, cleared storage — or storage being unavailable altogether, which
 * reads as "no record").
 *
 * `fallback` is what the caller already knows about the session, so a value
 * held in memory is not lost to a storage failure; it defaults to
 * {@link DEFAULT_PERMISSION_MODE} for callers that know nothing.
 *
 * Only a seed — a session the server still has live answers with its own mode
 * in the snapshot, and that value wins.
 */
export function initialSessionMode(
  server: string,
  workspaceId: string,
  sessionId: string,
  fallback: PermissionMode = DEFAULT_PERMISSION_MODE,
): PermissionMode {
  return loadMemory()[server]?.[sessionSlot(workspaceId, sessionId)]?.mode ?? fallback;
}
