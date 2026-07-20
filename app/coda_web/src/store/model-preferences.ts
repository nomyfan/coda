import type { ProviderInfo, ReasoningEffort } from "@/lib/protocol";

const storageKey = "coda.modelPrefs";

type ModelPref = {
  providerId: string;
  reasoningEffort: ReasoningEffort | null;
  modelEfforts: Record<string, ReasoningEffort>;
};
type ModelPrefs = Record<string, Record<string, ModelPref>>;
type ModelCatalog = {
  url: string;
  providers: ProviderInfo[];
  defaultProvider?: string;
};

function isModelPref(value: unknown): value is ModelPref {
  if (!value || typeof value !== "object") {
    return false;
  }
  const pref = value as Partial<ModelPref>;
  return (
    typeof pref.providerId === "string" &&
    (pref.reasoningEffort === null || typeof pref.reasoningEffort === "string") &&
    typeof pref.modelEfforts === "object" &&
    pref.modelEfforts !== null
  );
}

function loadModelPrefs(): ModelPrefs {
  const prefs: ModelPrefs = Object.create(null);
  try {
    const raw = window.localStorage.getItem(storageKey);
    if (!raw) {
      return prefs;
    }
    const parsed = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      return prefs;
    }
    for (const [server, storedWorkspaces] of Object.entries(parsed)) {
      if (!storedWorkspaces || typeof storedWorkspaces !== "object") {
        continue;
      }
      const workspaces: Record<string, ModelPref> = Object.create(null);
      for (const [workspace, pref] of Object.entries(storedWorkspaces)) {
        if (isModelPref(pref)) {
          workspaces[workspace] = pref;
        }
      }
      if (Object.keys(workspaces).length > 0) {
        prefs[server] = workspaces;
      }
    }
  } catch {
    // ignore malformed/blocked storage
  }
  return prefs;
}

export function rememberModelSelection(
  server: string,
  workspace: string,
  providerId: string,
  reasoningEffort: ReasoningEffort | null,
) {
  try {
    const prefs = loadModelPrefs();
    prefs[server] ??= Object.create(null);
    const existing = prefs[server][workspace];
    const modelEfforts = { ...existing?.modelEfforts };
    if (reasoningEffort) {
      modelEfforts[providerId] = reasoningEffort;
    }
    prefs[server][workspace] = { providerId, reasoningEffort, modelEfforts };
    window.localStorage.setItem(storageKey, JSON.stringify(prefs));
  } catch {
    // ignore storage failures (private mode, disabled storage)
  }
}

/** Select the workspace's last-used model, falling back to the server default. */
export function initialModelSelection(
  server: ModelCatalog,
  workspace: string,
): {
  providerId?: string;
  reasoningEffort: ReasoningEffort | null;
} {
  const remembered = loadModelPrefs()[server.url]?.[workspace];
  const rememberedProvider = remembered
    ? server.providers.find((item) => item.id === remembered.providerId)
    : undefined;
  const provider =
    rememberedProvider ??
    server.providers.find((item) => item.id === server.defaultProvider) ??
    server.providers[0];
  if (!provider) {
    return { providerId: undefined, reasoningEffort: null };
  }
  const reasoningEffort = resolveEffort(provider, remembered);
  return { providerId: provider.id, reasoningEffort };
}

/**
 * Resolve the reasoning effort for a model, following the fallback chain:
 * 1. Per-model remembered effort (if still valid)
 * 2. Server-declared default_reasoning_effort
 * 3. First entry in reasoning_efforts
 */
function resolveEffort(
  provider: ProviderInfo,
  remembered: ModelPref | undefined,
): ReasoningEffort | null {
  if (provider.reasoning_efforts.length === 0) {
    return null;
  }
  const memorized = remembered?.modelEfforts[provider.id];
  if (memorized && provider.reasoning_efforts.includes(memorized)) {
    return memorized;
  }
  if (
    provider.default_reasoning_effort &&
    provider.reasoning_efforts.includes(provider.default_reasoning_effort)
  ) {
    return provider.default_reasoning_effort;
  }
  return provider.reasoning_efforts[0];
}

/**
 * Resolve the effort for a model the user is switching to, using remembered
 * per-model effort from the workspace preference.
 */
export function resolveEffortForModel(
  server: ModelCatalog,
  workspace: string,
  provider: ProviderInfo,
  currentEffort: ReasoningEffort | null,
): ReasoningEffort | null {
  if (provider.reasoning_efforts.length === 0) {
    return null;
  }
  const remembered = loadModelPrefs()[server.url]?.[workspace];
  const memorized = remembered?.modelEfforts[provider.id];
  if (memorized && provider.reasoning_efforts.includes(memorized)) {
    return memorized;
  }
  if (currentEffort && provider.reasoning_efforts.includes(currentEffort)) {
    return currentEffort;
  }
  if (
    provider.default_reasoning_effort &&
    provider.reasoning_efforts.includes(provider.default_reasoning_effort)
  ) {
    return provider.default_reasoning_effort;
  }
  return provider.reasoning_efforts[0];
}
