import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";

import {
  initialModelSelection,
  rememberModelSelection,
  resolveEffortForModel,
} from "../src/store/model-preferences.ts";

const values = new Map();
globalThis.window = {
  localStorage: {
    getItem(key) {
      return values.get(key) ?? null;
    },
    setItem(key, value) {
      values.set(key, value);
    },
  },
};

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

function catalog(url) {
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

  assert.deepEqual(initialModelSelection(catalog("ws://one"), "alpha"), {
    providerId: modelA.id,
    reasoningEffort: "high",
  });
  assert.deepEqual(initialModelSelection(catalog("ws://one"), "beta"), {
    providerId: modelB.id,
    reasoningEffort: null,
  });
  assert.deepEqual(initialModelSelection(catalog("ws://two"), "alpha"), {
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

  assert.deepEqual(initialModelSelection(catalog("ws://one"), "alpha"), {
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

  assert.deepEqual(initialModelSelection(catalog("ws://one"), "alpha"), {
    providerId: modelA.id,
    reasoningEffort: "low",
  });
});

test("remembers per-model effort and recalls it on switch back", () => {
  rememberModelSelection("ws://one", "alpha", modelA.id, "high");
  rememberModelSelection("ws://one", "alpha", modelC.id, "low");

  // Switch back to modelA — should recall "high" from per-model memory
  assert.deepEqual(initialModelSelection(catalog("ws://one"), "alpha"), {
    providerId: modelC.id,
    reasoningEffort: "low",
  });

  // resolveEffortForModel should recall modelA's remembered effort
  const effort = resolveEffortForModel(catalog("ws://one"), "alpha", modelA, null);
  assert.equal(effort, "high");
});

test("uses configured default when no per-model memory exists", () => {
  // modelC has default_reasoning_effort = "medium"
  const result = initialModelSelection(
    { url: "ws://fresh", providers: [modelC], defaultProvider: modelC.id },
    "alpha",
  );
  assert.deepEqual(result, {
    providerId: modelC.id,
    reasoningEffort: "medium",
  });
});

test("falls back to first effort when no default configured", () => {
  // modelA has no default_reasoning_effort
  const result = initialModelSelection(
    { url: "ws://fresh", providers: [modelA], defaultProvider: modelA.id },
    "alpha",
  );
  assert.deepEqual(result, {
    providerId: modelA.id,
    reasoningEffort: "low",
  });
});

test("remembered effort takes priority over configured default", () => {
  // modelC default is "medium", but user chose "high" last time
  rememberModelSelection("ws://one", "alpha", modelC.id, "high");

  const result = initialModelSelection(catalog("ws://one"), "alpha");
  assert.deepEqual(result, {
    providerId: modelC.id,
    reasoningEffort: "high",
  });
});

test("falls back to default when remembered effort is no longer valid", () => {
  // Simulate: user chose "xhigh" which was later removed from config
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
  assert.deepEqual(result, {
    providerId: modelC.id,
    reasoningEffort: "medium", // falls back to configured default
  });
});

test("resolveEffortForModel prefers per-model memory over current effort", () => {
  // Model A is at "high", model C was remembered as "low"
  rememberModelSelection("ws://one", "alpha", modelC.id, "low");

  // Switching from A (high) to C — C supports "high", but C's memory says "low"
  const effort = resolveEffortForModel(catalog("ws://one"), "alpha", modelC, "high");
  assert.equal(effort, "low");
});

test("resolveEffortForModel falls back to current effort when no memory", () => {
  const effort = resolveEffortForModel(catalog("ws://one"), "alpha", modelC, "high");
  assert.equal(effort, "high");
});

test("resolveEffortForModel uses per-model memory when current effort is invalid", () => {
  rememberModelSelection("ws://one", "alpha", modelC.id, "low");

  const effort = resolveEffortForModel(
    catalog("ws://one"),
    "alpha",
    modelC,
    "xhigh", // not in modelC's efforts
  );
  assert.equal(effort, "low");
});

test("resolveEffortForModel falls back to configured default then first", () => {
  // No memory, invalid current effort
  const effort = resolveEffortForModel(catalog("ws://one"), "alpha", modelC, "xhigh");
  assert.equal(effort, "medium"); // configured default

  const effortA = resolveEffortForModel(catalog("ws://one"), "alpha", modelA, "xhigh");
  assert.equal(effortA, "low"); // first entry (no configured default)
});
