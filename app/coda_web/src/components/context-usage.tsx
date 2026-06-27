import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import type { CompletionUsage, UsageRecord } from "@/store/session";

const rootAgent = "coda";

function tokenCount(usage: CompletionUsage) {
  return usage.total_tokens || usage.prompt_tokens + usage.completion_tokens;
}

function formatTokens(tokens: number) {
  return new Intl.NumberFormat("en-US").format(tokens);
}

function UsageRow({ label, value }: { label: string; value?: number }) {
  if (value === undefined) {
    return null;
  }
  return (
    <div className="flex items-center justify-between gap-4">
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="font-mono text-foreground">{formatTokens(value)}</dd>
    </div>
  );
}

export function ContextUsage({
  contextWindow,
  records,
}: {
  contextWindow: number;
  records: UsageRecord[];
}) {
  let latestRootUsage: CompletionUsage | undefined;
  for (let index = records.length - 1; index >= 0; index -= 1) {
    if (records[index].agentName === rootAgent) {
      latestRootUsage = records[index].usage;
      break;
    }
  }
  const contextTokens = latestRootUsage ? tokenCount(latestRootUsage) : 0;
  const percentage = Math.min(100, (contextTokens / contextWindow) * 100);
  const circumference = 2 * Math.PI * 15;

  return (
    <Popover>
      <PopoverTrigger asChild>
        <button
          type="button"
          className="group relative size-7 shrink-0 cursor-pointer rounded-full text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
          title={`${formatTokens(contextTokens)} / ${formatTokens(contextWindow)} context tokens`}
          aria-label={`Context usage: ${formatTokens(contextTokens)} of ${formatTokens(contextWindow)} tokens`}
        >
          <svg className="absolute inset-1.5 size-4 -rotate-90" viewBox="0 0 36 36">
            <circle
              cx="18"
              cy="18"
              r="15"
              fill="none"
              stroke="currentColor"
              strokeWidth="4"
              className="text-muted"
            />
            <circle
              cx="18"
              cy="18"
              r="15"
              fill="none"
              stroke="currentColor"
              strokeWidth="4"
              strokeLinecap="round"
              strokeDasharray={circumference}
              strokeDashoffset={circumference * (1 - percentage / 100)}
              className="text-primary transition-colors group-hover:text-primary/70"
            />
          </svg>
        </button>
      </PopoverTrigger>
      <PopoverContent
        align="end"
        side="top"
        sideOffset={8}
        className="max-h-[min(70vh,40rem)] w-72 overflow-y-auto p-3"
      >
        <div className="mb-3">
          <h2 className="font-semibold leading-none">Context usage</h2>
        </div>

        <div className="space-y-3 text-sm">
          <section>
            <div className="mb-2 flex items-baseline justify-between gap-4">
              <span className="text-muted-foreground">Current context</span>
              <span className="font-mono font-semibold">{percentage.toFixed(1)}%</span>
            </div>
            <div className="h-2 overflow-hidden rounded-full bg-muted">
              <div className="h-full rounded-full bg-primary" style={{ width: `${percentage}%` }} />
            </div>
            <dl className="mt-2 space-y-1">
              <UsageRow label="Used" value={contextTokens} />
              <UsageRow label="Context window" value={contextWindow} />
            </dl>
          </section>

          {latestRootUsage ? (
            <section className="border-t pt-3">
              <h3 className="mb-2 font-medium">Latest request</h3>
              <dl className="space-y-1">
                <UsageRow label="Input tokens" value={latestRootUsage.prompt_tokens} />
                <UsageRow label="Output tokens" value={latestRootUsage.completion_tokens} />
                <UsageRow label="Total tokens" value={tokenCount(latestRootUsage)} />
                <UsageRow
                  label="Cached input"
                  value={latestRootUsage.prompt_tokens_details?.cached_tokens ?? undefined}
                />
                <UsageRow
                  label="Cache hits"
                  value={latestRootUsage.prompt_tokens_details?.cache_hit_tokens ?? undefined}
                />
                <UsageRow
                  label="Cache misses"
                  value={latestRootUsage.prompt_tokens_details?.cache_miss_tokens ?? undefined}
                />
                <UsageRow
                  label="Input audio"
                  value={latestRootUsage.prompt_tokens_details?.audio_tokens ?? undefined}
                />
                <UsageRow
                  label="Reasoning tokens"
                  value={latestRootUsage.completion_tokens_details?.reasoning_tokens ?? undefined}
                />
                <UsageRow
                  label="Output audio"
                  value={latestRootUsage.completion_tokens_details?.audio_tokens ?? undefined}
                />
                <UsageRow
                  label="Accepted prediction"
                  value={
                    latestRootUsage.completion_tokens_details?.accepted_prediction_tokens ??
                    undefined
                  }
                />
                <UsageRow
                  label="Rejected prediction"
                  value={
                    latestRootUsage.completion_tokens_details?.rejected_prediction_tokens ??
                    undefined
                  }
                />
              </dl>
            </section>
          ) : null}
        </div>
      </PopoverContent>
    </Popover>
  );
}
