import { Brain, Cpu } from "lucide-react";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import type { ProviderInfo, ReasoningEffort } from "@/lib/session";

function effortLabel(effort: ReasoningEffort) {
  return effort.charAt(0).toUpperCase() + effort.slice(1);
}

/**
 * Pick the reasoning value to carry over when switching provider: a model with
 * no reasoning controls gets `null`; "off" stays off; an effort the new
 * provider still accepts is kept, otherwise its first declared level.
 */
function carryReasoning(
  provider: ProviderInfo,
  current: ReasoningEffort | null
): ReasoningEffort | null {
  if (provider.reasoning_efforts.length === 0) {
    return null;
  }
  if (current === "none") {
    return current;
  }
  if (current !== null && provider.reasoning_efforts.includes(current)) {
    return current;
  }
  return provider.reasoning_efforts[0];
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
  if (providers.length === 0 || !providerId) {
    return null;
  }
  const selected = providers.find((provider) => provider.id === providerId);
  const efforts = selected?.reasoning_efforts ?? [];

  return (
    <>
      <Select
        value={providerId}
        onValueChange={(id) => {
          const next = providers.find((provider) => provider.id === id);
          if (next) {
            onSetModel(id, carryReasoning(next, reasoningEffort));
          }
        }}
        disabled={disabled}
      >
        <SelectTrigger size="sm" className="w-auto gap-1.5 rounded-md text-xs">
          <Cpu className="size-3.5 text-muted-foreground" />
          <SelectValue placeholder="Model" />
        </SelectTrigger>
        <SelectContent position="popper" side="top">
          {providers.map((provider) => (
            <SelectItem key={provider.id} value={provider.id}>
              {provider.model}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      {efforts.length > 0 ? (
        <Select
          value={reasoningEffort ?? undefined}
          onValueChange={(value) =>
            onSetModel(providerId, value as ReasoningEffort)
          }
          disabled={disabled}
        >
          <SelectTrigger size="sm" className="w-auto gap-1.5 rounded-md text-xs">
            <Brain className="size-3.5 text-muted-foreground" />
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
