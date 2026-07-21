import { beforeEach, expect, test } from "vitest";

import {
  initialModelSelection,
  rememberModelSelection,
  resolveEffortForModel,
} from "../src/store/model-preferences.ts";

const values = new Map<string, string>();
globalThis.window = {
  localStorage: {
    getItem(key: string) {
      return values.get(key) ?? null;
    },
    setItem(key: string, value: string) {
      values.set(key, value);
    },
  },
} as unknown as Window & typeof globalThis;

const modelA = {
  id: "provider:model-a",
  provider: "provider",
  model: "model-a",
  context_window: 100_000,
  reasoning_efforts: ["low", "high"],
  input_modalities: ["text"],
};
const modelB = {
  id: "provider:model-b",
  provider: "provider",
  model: "model-b",
  context_window: 100_000,
  reasoning_efforts: [],
  input_modalities: ["text"],
};
const modelC = {
  id: "provider:model-c",
  provider: "provider",
  model: "model-c",
  context_window: 100_000,
  reasoning_efforts: ["low", "medium", "high"],
  default_reasoning_effort: "medium",
  input_modalities: ["text"],
};

function catalog(url: string) {
  return {
    url,
    providers: [modelA, modelB, modelC],
    defaultProvider: modelA.id,
  };
}

beforeEach(() => values.clear());

test("remembers model selections per server and workspace", () => {
  rememberModelSelection("ws://one", "alpha", modelA.id, "high");
  rememberModelSelection("ws://one", "beta", modelB.id, null);
  rememberModelSelection("ws://two", "alpha", modelB.id, null);

  expect(initialModelSelection(catalog("ws://one"), "alpha")).toEqual({
    providerId: modelA.id,
    reasoningEffort: "high",
  });
  expect(initialModelSelection(catalog("ws://one"), "beta")).toEqual({
    providerId: modelB.id,
    reasoningEffort: null,
  });
  expect(initialModelSelection(catalog("ws://two"), "alpha")).toEqual({
    providerId: modelB.id,
    reasoningEffort: null,
  });
});

test("falls back when a workspace has no valid remembered model", () => {
  values.set(
    "coda.modelPrefs",
    JSON.stringify({
      "ws://one": {
        alpha: { providerId: "removed:model", reasoningEffort: "high" },
      },
    }),
  );

  expect(initialModelSelection(catalog("ws://one"), "alpha")).toEqual({
    providerId: modelA.id,
    reasoningEffort: "low",
  });
});

test("ignores the old server-only preference format", () => {
  values.set(
    "coda.modelPrefs",
    JSON.stringify({
      "ws://one": { providerId: modelB.id, reasoningEffort: null },
    }),
  );

  expect(initialModelSelection(catalog("ws://one"), "alpha")).toEqual({
    providerId: modelA.id,
    reasoningEffort: "low",
  });
});

test("remembers per-model effort and recalls it on switch back", () => {
  rememberModelSelection("ws://one", "alpha", modelA.id, "high");
  rememberModelSelection("ws://one", "alpha", modelC.id, "low");

  expect(initialModelSelection(catalog("ws://one"), "alpha")).toEqual({
    providerId: modelC.id,
    reasoningEffort: "low",
  });

  const effort = resolveEffortForModel(catalog("ws://one"), "alpha", modelA, null);
  expect(effort).toBe("high");
});

test("uses configured default when no per-model memory exists", () => {
  const result = initialModelSelection(
    { url: "ws://fresh", providers: [modelC], defaultProvider: modelC.id },
    "alpha",
  );
  expect(result).toEqual({
    providerId: modelC.id,
    reasoningEffort: "medium",
  });
});

test("falls back to first effort when no default configured", () => {
  const result = initialModelSelection(
    { url: "ws://fresh", providers: [modelA], defaultProvider: modelA.id },
    "alpha",
  );
  expect(result).toEqual({
    providerId: modelA.id,
    reasoningEffort: "low",
  });
});

test("remembered effort takes priority over configured default", () => {
  rememberModelSelection("ws://one", "alpha", modelC.id, "high");

  const result = initialModelSelection(catalog("ws://one"), "alpha");
  expect(result).toEqual({
    providerId: modelC.id,
    reasoningEffort: "high",
  });
});

test("falls back to default when remembered effort is no longer valid", () => {
  values.set(
    "coda.modelPrefs",
    JSON.stringify({
      "ws://one": {
        alpha: {
          providerId: modelC.id,
          reasoningEffort: "xhigh",
          modelEfforts: { [modelC.id]: "xhigh" },
        },
      },
    }),
  );

  const result = initialModelSelection(catalog("ws://one"), "alpha");
  expect(result).toEqual({
    providerId: modelC.id,
    reasoningEffort: "medium",
  });
});

test("resolveEffortForModel prefers per-model memory over current effort", () => {
  rememberModelSelection("ws://one", "alpha", modelC.id, "low");

  const effort = resolveEffortForModel(catalog("ws://one"), "alpha", modelC, "high");
  expect(effort).toBe("low");
});

test("resolveEffortForModel falls back to current effort when no memory", () => {
  const effort = resolveEffortForModel(catalog("ws://one"), "alpha", modelC, "high");
  expect(effort).toBe("high");
});

test("resolveEffortForModel uses per-model memory when current effort is invalid", () => {
  rememberModelSelection("ws://one", "alpha", modelC.id, "low");

  const effort = resolveEffortForModel(catalog("ws://one"), "alpha", modelC, "xhigh");
  expect(effort).toBe("low");
});

test("resolveEffortForModel falls back to configured default then first", () => {
  const effort = resolveEffortForModel(catalog("ws://one"), "alpha", modelC, "xhigh");
  expect(effort).toBe("medium");

  const effortA = resolveEffortForModel(catalog("ws://one"), "alpha", modelA, "xhigh");
  expect(effortA).toBe("low");
});
