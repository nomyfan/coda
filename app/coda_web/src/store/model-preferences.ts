import type { ProviderInfo, ReasoningEffort } from "@/lib/protocol";

const storageKey = "coda.modelPrefs";

type ModelPref = { providerId: string; reasoningEffort: ReasoningEffort | null };
type ModelPrefs = Record<string, Record<string, ModelPref>>;
type ModelCatalog = {
  url: string;
  providers: ProviderInfo[];
  defaultProvider?: string;
};

const reasoningEfforts: readonly ReasoningEffort[] = [
  "none",
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
];

function isModelPref(value: unknown): value is ModelPref {
  if (!value || typeof value !== "object") {
    return false;
  }
  const pref = value as Partial<ModelPref>;
  return (
    typeof pref.providerId === "string" &&
    (pref.reasoningEffort === null ||
      reasoningEfforts.includes(pref.reasoningEffort as ReasoningEffort))
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
    prefs[server][workspace] = { providerId, reasoningEffort };
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
  const reasoningEffort =
    rememberedProvider && remembered
      ? validEffort(provider, remembered.reasoningEffort)
      : (provider.reasoning_efforts[0] ?? null);
  return { providerId: provider.id, reasoningEffort };
}

function validEffort(
  provider: ProviderInfo,
  effort: ReasoningEffort | null,
): ReasoningEffort | null {
  if (provider.reasoning_efforts.length === 0) {
    return null;
  }
  if (effort === "none" || (effort && provider.reasoning_efforts.includes(effort))) {
    return effort;
  }
  return provider.reasoning_efforts[0];
}
