import { expect, test } from "vitest";

import type { ProviderInfo } from "../src/lib/protocol.ts";
import { groupProviders } from "../src/components/model-selector.tsx";

function model(provider: string, id: string): ProviderInfo {
  return {
    id: `${provider}:${id}`,
    provider,
    model: id,
    context_window: 100_000,
    reasoning_efforts: [],
    input_modalities: ["text"],
  };
}

test("preserves configured provider and model order", () => {
  const zetaFirst = model("zeta", "model-z");
  const zetaSecond = model("zeta", "model-a");
  const alpha = model("alpha", "model-b");

  expect(groupProviders([zetaFirst, zetaSecond, alpha])).toEqual([
    ["zeta", [zetaFirst, zetaSecond]],
    ["alpha", [alpha]],
  ]);
});
