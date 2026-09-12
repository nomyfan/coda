import { useMemo } from "react";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectSeparator,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import type { ProviderInfo, ReasoningEffort } from "@/lib/protocol";
import { compatibleEffort, resolveEffortForModel } from "@/store/model-preferences";

function effortLabel(effort: ReasoningEffort) {
  if (effort === "off") return "Off";
  return effort.charAt(0).toUpperCase() + effort.slice(1);
}

/** Group providers without changing the catalog order received from the server. */
export function groupProviders(providers: ProviderInfo[]): Array<[string, ProviderInfo[]]> {
  const groups = new Map<string, ProviderInfo[]>();
  for (const info of providers) {
    const models = groups.get(info.provider);
    if (models) {
      models.push(info);
    } else {
      groups.set(info.provider, [info]);
    }
  }
  return [...groups.entries()];
}

export function ModelSelector({
  providers,
  providerId,
  reasoningEffort,
  disabled,
  allowedModelIds,
  draftHasImages,
  serverUrl,
  workspaceId,
  onSetModel,
}: {
  providers: ProviderInfo[];
  providerId?: string;
  reasoningEffort: ReasoningEffort | null;
  disabled: boolean;
  /** Undefined for a new session; otherwise supplied by the server. */
  allowedModelIds?: string[];
  /** History compatibility comes from allowedModelIds; only unsent images add a local constraint. */
  draftHasImages: boolean;
  serverUrl: string;
  workspaceId: string;
  onSetModel: (providerId: string, reasoningEffort: ReasoningEffort | null) => void;
}) {
  const groups = useMemo(() => groupProviders(providers), [providers]);
  const selected = providers.find((info) => info.id === providerId);
  const efforts = selected?.reasoning_efforts ?? [];

  if (!providerId) return null;

  const catalog = { url: serverUrl, providers };

  // Build a flat list of elements so Radix viewport receives only valid children.
  const dropdownItems = groups.flatMap(([providerName, models], groupIndex) => {
    return [
      ...(groupIndex > 0 ? [<SelectSeparator key={`sep-${providerName}`} />] : []),
      <SelectGroup key={providerName}>
        <SelectLabel>{providerName}</SelectLabel>
        {models.map((info) => (
          <SelectItem
            key={info.id}
            value={info.id}
            disabled={
              (allowedModelIds !== undefined && !allowedModelIds.includes(info.id)) ||
              (draftHasImages && !info.input_modalities.includes("image"))
            }
          >
            {info.model}
          </SelectItem>
        ))}
      </SelectGroup>,
    ];
  });

  return (
    <>
      <Select
        value={providerId}
        onValueChange={(id) => {
          const next = providers.find((info) => info.id === id);
          if (!next) return;
          const effort =
            allowedModelIds === undefined
              ? resolveEffortForModel(catalog, workspaceId, next, reasoningEffort)
              : compatibleEffort(next, reasoningEffort);
          onSetModel(id, effort);
        }}
        disabled={disabled || (allowedModelIds !== undefined && allowedModelIds.length === 0)}
      >
        <SelectTrigger
          size="sm"
          className="h-7 max-w-36 gap-1 rounded-md border-0 bg-transparent px-2 text-xs shadow-none hover:bg-muted/70 sm:max-w-44 dark:bg-transparent dark:hover:bg-muted/70"
        >
          <SelectValue placeholder="Model">{selected?.model ?? providerId}</SelectValue>
        </SelectTrigger>
        <SelectContent position="popper" side="top">
          {dropdownItems}
        </SelectContent>
      </Select>
      {efforts.length > 0 ? (
        <Select
          value={reasoningEffort ?? efforts[0]}
          onValueChange={(value) => onSetModel(providerId, value as ReasoningEffort)}
          disabled={
            disabled || (allowedModelIds !== undefined && !allowedModelIds.includes(providerId))
          }
        >
          <SelectTrigger
            size="sm"
            className="h-7 max-w-28 gap-1 rounded-md border-0 bg-transparent px-2 text-xs shadow-none hover:bg-muted/70 sm:max-w-32 dark:bg-transparent dark:hover:bg-muted/70"
          >
            <SelectValue placeholder="Reasoning">
              {reasoningEffort && !efforts.includes(reasoningEffort)
                ? reasoningEffort
                : effortLabel(reasoningEffort ?? efforts[0])}
            </SelectValue>
          </SelectTrigger>
          <SelectContent position="popper" side="top">
            {efforts.map((effort) => (
              <SelectItem key={effort} value={effort}>
                {effortLabel(effort)}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      ) : reasoningEffort ? (
        <span
          className="px-2 font-mono text-xs text-muted-foreground"
          title="Saved reasoning effort"
        >
          {reasoningEffort}
        </span>
      ) : null}
    </>
  );
}
