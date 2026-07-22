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
import { resolveEffortForModel } from "@/store/model-preferences";

function effortLabel(effort: ReasoningEffort) {
  if (effort === "off") return "Off";
  return effort.charAt(0).toUpperCase() + effort.slice(1);
}

/** Map of provider name → list of its models, as received from the server. */
type ProviderGroups = Record<string, ProviderInfo[]>;

function groupProviders(providers: ProviderInfo[]): ProviderGroups {
  const groups: ProviderGroups = {};
  for (const info of providers) {
    (groups[info.provider] ??= []).push(info);
  }
  return groups;
}

export function ModelSelector({
  providers,
  providerId,
  reasoningEffort,
  disabled,
  modelLocked,
  requireImageModel,
  serverUrl,
  workspaceId,
  onSetModel,
}: {
  providers: ProviderInfo[];
  providerId?: string;
  reasoningEffort: ReasoningEffort | null;
  disabled: boolean;
  /** Once a session is opened its provider/model is durable; only reasoning
   * effort remains adjustable while idle. */
  modelLocked: boolean;
  /** Restrict selectable models to vision-capable ones (the conversation
   * already involves images). */
  requireImageModel: boolean;
  serverUrl: string;
  workspaceId: string;
  onSetModel: (providerId: string, reasoningEffort: ReasoningEffort | null) => void;
}) {
  const groups = useMemo(() => groupProviders(providers), [providers]);
  const providerNames = useMemo(() => Object.keys(groups).sort(), [groups]);
  const selected = providers.find((info) => info.id === providerId);
  const efforts = selected?.reasoning_efforts ?? [];

  if (providers.length === 0 || !providerId) {
    return null;
  }

  const catalog = { url: serverUrl, providers };

  // Build a flat list of elements so Radix viewport receives only valid children.
  const dropdownItems = providerNames.flatMap((providerName, groupIndex) => {
    const models = groups[providerName];
    return [
      ...(groupIndex > 0 ? [<SelectSeparator key={`sep-${providerName}`} />] : []),
      <SelectGroup key={providerName}>
        <SelectLabel>{providerName}</SelectLabel>
        {models.map((info) => (
          <SelectItem
            key={info.id}
            value={info.id}
            disabled={requireImageModel && !info.input_modalities.includes("image")}
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
          const effort = resolveEffortForModel(catalog, workspaceId, next, reasoningEffort);
          onSetModel(id, effort);
        }}
        disabled={disabled || modelLocked}
      >
        <SelectTrigger
          size="sm"
          className="h-7 max-w-36 gap-1 rounded-md border-0 bg-transparent px-2 text-xs shadow-none hover:bg-muted/70 sm:max-w-44 dark:bg-transparent dark:hover:bg-muted/70"
        >
          <SelectValue placeholder="Model" />
        </SelectTrigger>
        <SelectContent position="popper" side="top">
          {dropdownItems}
        </SelectContent>
      </Select>
      {efforts.length > 0 ? (
        <Select
          value={reasoningEffort ?? efforts[0]}
          onValueChange={(value) => onSetModel(providerId, value as ReasoningEffort)}
          disabled={disabled}
        >
          <SelectTrigger
            size="sm"
            className="h-7 max-w-28 gap-1 rounded-md border-0 bg-transparent px-2 text-xs shadow-none hover:bg-muted/70 sm:max-w-32 dark:bg-transparent dark:hover:bg-muted/70"
          >
            <SelectValue placeholder="Reasoning" />
          </SelectTrigger>
          <SelectContent position="popper" side="top">
            {efforts.map((effort) => (
              <SelectItem key={effort} value={effort}>
                {effortLabel(effort)}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      ) : null}
    </>
  );
}
