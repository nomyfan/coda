import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";

import { initialModelSelection, rememberModelSelection } from "../src/store/model-preferences.ts";

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

function catalog(url) {
  return {
    url,
    providers: [modelA, modelB],
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
