import { Brain, Cpu } from "lucide-react";
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
import type { ProviderInfo, ReasoningEffort } from "@/store/session";

function effortLabel(effort: ReasoningEffort) {
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

/**
 * Pick the reasoning value to carry over when switching model: a model with
 * no reasoning controls gets `null`; "off" and supported efforts carry over;
 * every other value becomes the model's first configured effort.
 */
function carryReasoning(
  model: ProviderInfo | undefined,
  current: ReasoningEffort | null,
): ReasoningEffort | null {
  if (!model || model.reasoning_efforts.length === 0) {
    return null;
  }
  if (current === "none") {
    return current;
  }
  if (current && model.reasoning_efforts.includes(current)) {
    return current;
  }
  return model.reasoning_efforts[0];
}

export function ModelSelector({
  providers,
  providerId,
  reasoningEffort,
  disabled,
  requireImageModel,
  onSetModel,
}: {
  providers: ProviderInfo[];
  providerId?: string;
  reasoningEffort: ReasoningEffort | null;
  disabled: boolean;
  /** Restrict selectable models to vision-capable ones (the conversation
   * already involves images). */
  requireImageModel: boolean;
  onSetModel: (providerId: string, reasoningEffort: ReasoningEffort | null) => void;
}) {
  const groups = useMemo(() => groupProviders(providers), [providers]);
  const providerNames = useMemo(() => Object.keys(groups).sort(), [groups]);
  const selected = providers.find((info) => info.id === providerId);
  const efforts = selected?.reasoning_efforts ?? [];

  if (providers.length === 0 || !providerId) {
    return null;
  }

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
          onSetModel(id, carryReasoning(next, reasoningEffort));
        }}
        disabled={disabled}
      >
        <SelectTrigger
          size="sm"
          className="h-7 max-w-44 gap-1 rounded-md border-0 bg-transparent px-2 text-xs shadow-none hover:bg-muted/70"
        >
          <Cpu className="size-3 text-muted-foreground" />
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
            className="h-7 max-w-32 gap-1 rounded-md border-0 bg-transparent px-2 text-xs shadow-none hover:bg-muted/70"
          >
            <Brain className="size-3 text-muted-foreground" />
            <SelectValue placeholder="Reasoning" />
          </SelectTrigger>
          <SelectContent position="popper" side="top">
            <SelectItem value="none">Off</SelectItem>
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
