## Problem

When switching models in the dashboard, the user's per-model reasoning effort choice is lost because the frontend only remembers one `(model, reasoning_effort)` pair per `(server, workspace)`. Additionally, the server always falls back to a model's first configured effort, with no way for the config to declare which effort level is recommended.

## Scope

In:
- TOML config: `default_reasoning_effort` per model.
- Wire protocol: surface the default to the frontend.
- Frontend: per-model effort memory in localStorage; fallback chain uses the configured default.
- Server: `initial_reasoning_effort` uses the configured default instead of always the first.

Out:
- Per-session persistence of model preferences (this stays client-side in localStorage).
- Any change to the reasoning effort wire semantics or provider-layer handling.

## Assumptions

- A model's `default_reasoning_effort`, when present, must be one of its declared `reasoning_efforts`. An invalid default is a hard startup error (consistent with agent model validation).
- When `default_reasoning_effort` is absent, the first entry in `reasoning_efforts` remains the implicit default (no behavioral change for existing configs).
- Models without `reasoning_efforts` never have a `default_reasoning_effort` (no thinking = no default effort).
- The frontend is the only consumer of per-model effort memory; the server doesn't need to know which effort a user "last used" for a given model.

## Validation Findings

**Q: Does the frontend preference store need structural changes?**
Method: Read `model-preferences.ts`.
Result: `ModelPref` stores `{ providerId, reasoningEffort }` — one flat pair per workspace. Switching models overwrites it. The `providerId` here is actually the composite `{provider}:{model}` key, so we can key effort preferences by this id.
Implication: Extend the store to track effort per model id, not just one global pair.

**Q: How does the server expose model metadata to the frontend?**
Method: Read `wire.rs` and `protocol.ts`.
Result: `ProviderInfoWire` has `reasoning_efforts: Vec<String>` but no default. The frontend gets this from `list_providers` on connect.
Implication: Add `default_reasoning_effort: Option<String>` to `ProviderInfoWire`.

**Q: Where does the server compute the initial effort for a new session?**
Method: Read `server.rs`.
Result: `initial_reasoning_effort()` returns `provider.reasoning_efforts.first().cloned()`. Used in `resolve_selection()` as the fallback when no client selection is given.
Implication: Change `initial_reasoning_effort` to prefer `default_reasoning_effort` over the first entry.

## Components

- **`ModelConfig`** (config.rs) — Gains `default_reasoning_effort: Option<String>`, validated against `reasoning_efforts` at parse time.
- **`ProviderHandle`** (server.rs) — Carries the parsed default to `initial_reasoning_effort()` and `ProviderInfoWire`.
- **`ProviderInfoWire`** (wire.rs) — Gains `default_reasoning_effort` field for the dashboard.
- **Model preferences store** (model-preferences.ts) — Per-model effort memory keyed by provider id, with fallback to the server-declared default.

## Interfaces

### TOML config

```toml
# default_reasoning_effort is optional; when absent, the first entry in
# reasoning_efforts is the implicit default.
models = [
  { id = "deepseek-reasoner", context_window = 128000,
    reasoning_efforts = ["low", "medium", "high"],
    default_reasoning_effort = "medium" },
]
```

### Wire: `ProviderInfoWire`

```rust
pub struct ProviderInfoWire {
    // ...existing fields...
    /// The model's recommended initial effort. Absent when the model has no
    /// reasoning controls; defaults to the first entry when unconfigured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<String>,
}
```

Trust boundary: the TOML parse validates `default_reasoning_effort ∈ reasoning_efforts` at startup. Downstream code can trust it.

### Frontend: `model-preferences.ts`

```typescript
type ModelPref = {
  providerId: string;
  reasoningEffort: ReasoningEffort | null;
  /** Per-model effort memory: provider-id → last-used effort. */
  modelEfforts?: Record<string, ReasoningEffort>;
};
```

`initialModelSelection` resolution order for effort:
1. Remembered effort for this specific model (from `modelEfforts[providerId]`), if it's still in the model's `reasoning_efforts`.
2. Server-declared `default_reasoning_effort`.
3. First entry in `reasoning_efforts`.

`rememberModelSelection` saves the effort under `modelEfforts[providerId]` alongside the current top-level pair.

### Frontend: `ProviderInfo` type

```typescript
export type ProviderInfo = {
  // ...existing fields...
  default_reasoning_effort?: ReasoningEffort | null;
};
```

## Data Model

No new entities. `ModelConfig` gains one optional field. `ModelPref` in localStorage gains a required `modelEfforts` map. Old data missing this field is rejected by `isModelPref` and falls through to defaults — no backward-compat shim needed.

## Load-Bearing Decisions

1. **Per-model effort lives in the frontend only.** The server doesn't track which effort a user last used per model — it only declares the default. This keeps the server stateless w.r.t. UI preferences and avoids a new persistence layer.

2. **`default_reasoning_effort` must be in `reasoning_efforts`.** Validated at TOML parse. This means the config author can't set a default that the UI won't offer, preventing a state where the default is invisible.

3. **Fallback chain: remembered → configured default → first.** This gives the user's explicit choice top priority, then the operator's recommendation, then the positional implicit default. A config change that removes a remembered effort falls through to the configured default (not to the first entry, unless the configured default is also gone).

4. **`modelEfforts` is a required nested map inside the `ModelPref` structure.** An alternative was a separate localStorage key per model; nesting keeps the storage footprint compact and atomic (one read/write per workspace switch). Old data without `modelEfforts` is rejected and falls through to defaults.

## Implementation Roadmap

- [x] [config] Add `default_reasoning_effort` to `ModelConfig`, validate at parse time.
      Purpose: Server rejects invalid defaults at startup.
      Verification: Unit test — valid default parses; default not in `reasoning_efforts` errors; absent default works.

- [x] [server] Propagate default through `ProviderHandle` → `ProviderInfoWire` → wire.
      Purpose: Frontend receives the configured default.
      Verification: `provider_catalog_roundtrips` test includes the new field. `initial_reasoning_effort` prefers default over first.

- [x] [frontend] Extend `model-preferences.ts` to track per-model efforts, update `initialModelSelection` fallback chain.
      Purpose: Switching models preserves per-model effort; new models start at the configured default.
      Verification: Unit tests cover the three-level fallback and config-change invalidation. `pnpm --filter coda-web test` passes.

- [x] [frontend] Wire `default_reasoning_effort` into `ProviderInfo` type and `model-selector.tsx` `carryReasoning`.
      Purpose: Model switching uses the configured default (not always the first effort) when no remembered preference exists.
      Verification: `pnpm --filter coda-web lint` and `pnpm --filter coda-web test` pass.

- [x] [docs] Update `AGENTS.md`, example configs, and the freeform-reasoning-effort spec to document the new field.
      Purpose: Documentation stays in sync.
      Verification: Config examples parse correctly.
