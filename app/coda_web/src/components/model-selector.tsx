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
import type { ProviderInfo, ReasoningEffort } from "@/lib/session";

const defaultReasoning = "__default__";

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
 * no reasoning controls gets `null`; provider default and "off" carry over; an
 * effort the new model still accepts is kept, otherwise its first level.
 */
function carryReasoning(
  model: ProviderInfo | undefined,
  current: ReasoningEffort | null
): ReasoningEffort | null {
  if (!model || model.reasoning_efforts.length === 0) {
    return null;
  }
  if (current === null || current === "none") {
    return current;
  }
  if (model.reasoning_efforts.includes(current)) {
    return current;
  }
  return model.reasoning_efforts[0];
}

export function ModelSelector({
  providers,
  providerId,
  reasoningEffort,
  disabled,
  onSetModel,
}: {
  providers: ProviderInfo[];
  providerId?: string;
  reasoningEffort: ReasoningEffort | null;
  disabled: boolean;
  onSetModel: (
    providerId: string,
    reasoningEffort: ReasoningEffort | null
  ) => void;
}) {
  const groups = useMemo(() => groupProviders(providers), [providers]);
  const providerNames = useMemo(
    () => Object.keys(groups).sort(),
    [groups]
  );
  const selected = providers.find((info) => info.id === providerId);
  const efforts = selected?.reasoning_efforts ?? [];

  if (providers.length === 0 || !providerId) {
    return null;
  }

  // Build a flat list of elements so Radix viewport receives only valid children.
  const dropdownItems = providerNames.flatMap((providerName, groupIndex) => {
    const models = groups[providerName];
    return [
      ...(groupIndex > 0
        ? [<SelectSeparator key={`sep-${providerName}`} />]
        : []),
      <SelectGroup key={providerName}>
        <SelectLabel>{providerName}</SelectLabel>
        {models.map((info) => (
          <SelectItem key={info.id} value={info.id}>
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
        <SelectTrigger size="sm" className="w-auto gap-1.5 rounded-md text-xs">
          <Cpu className="size-3.5 text-muted-foreground" />
          <SelectValue placeholder="Model" />
        </SelectTrigger>
        <SelectContent position="popper" side="top">
          {dropdownItems}
        </SelectContent>
      </Select>
      {efforts.length > 0 ? (
        <Select
          value={reasoningEffort ?? defaultReasoning}
          onValueChange={(value) =>
            onSetModel(
              providerId,
              value === defaultReasoning ? null : (value as ReasoningEffort)
            )
          }
          disabled={disabled}
        >
          <SelectTrigger size="sm" className="w-auto gap-1.5 rounded-md text-xs">
            <Brain className="size-3.5 text-muted-foreground" />
            <SelectValue placeholder="Reasoning" />
          </SelectTrigger>
          <SelectContent position="popper" side="top">
            <SelectItem value={defaultReasoning}>Default</SelectItem>
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
